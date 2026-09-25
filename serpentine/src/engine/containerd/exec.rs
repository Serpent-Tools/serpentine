//! Code for executing commands in containers

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use containerd_client::services::v1 as containerd_services;
use miette::{Context, Diagnostic, IntoDiagnostic};
use serpentine_internal::network;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead};
use typed_path::UnixPathBuf;

use super::{CONTAINERD_GC_ROOT_LABEL, SNAPSHOTTER, WithLease, container_config};
use crate::engine::cache::{CacheHash, CacheScope};
use crate::engine::{WrapInternal, internal, userdb};
use crate::events::{Reporter, TaskHandle, TaskId, TaskKind};

/// A command failed to execute
#[derive(Debug, Error, Diagnostic)]
#[error("Failed to execute command (exit code {code}): {command:?} \n{output}")]
#[diagnostic(code(command_execution_error))]
struct CommandFailed {
    /// The exit code
    code: u32,
    /// The command that was run
    command: String,
    /// The stdout/stderr of the command
    output: String,
}

/// A healthcheck didnt pass in time
#[derive(Debug, Error, Diagnostic)]
#[error("Healthcheck {check:?} did not pass in {timeout:?}")]
#[diagnostic(code(healthcheck_timeout))]
struct HealthcheckTimeout {
    /// Which healthcheck didnt pass
    check: String,
    /// How long we waited for it to pass
    timeout: Duration,
}

/// Attempted to capture non-utf8 output
#[derive(Debug, Error, Diagnostic)]
#[error("Failed to capture stdout of command as non-utf8 was found: \n{output}")]
#[diagnostic(code(non_utf8_capture))]
struct NonUtf8Capture {
    /// The stdout/stderr of the command
    output: String,
}

/// A handle to a running container
struct ContainerHandle {
    /// The id of the container in containerd
    id: String,
    /// The stdout handle of the container process
    stdout: tokio_util::task::AbortOnDropHandle<Result<String, String>>,
    /// The mutable snapshot the container is being run with.
    snapshot: String,
    /// The original node that spawned this container
    node: container_config::ContainerTopologyNode,
    /// Tracks this container's output as a task; finished when the handle is dropped.
    exec_task: TaskHandle,
}

impl super::Client {
    /// Execute a command on top of a given state and return a new state representing the result
    pub async fn exec(
        &self,
        state: &container_config::ContainerState,
        cmd: String,
    ) -> miette::Result<container_config::ContainerState> {
        let lease = self.new_lease().await?;
        let (container, _) = self.exec_internal(state.clone(), cmd, &lease).await?;
        self.drop_lease(lease).await?;

        Ok(container)
    }

    /// Execute a command return its stdout and stderr.
    pub async fn exec_get_output(
        &self,
        state: &container_config::ContainerState,
        cmd: String,
    ) -> miette::Result<String> {
        let lease = self.new_lease().await?;
        let stdout = self
            .exec_internal(state.clone(), cmd, &lease)
            .await?
            .1
            .map_err(|output| NonUtf8Capture { output })?;
        self.drop_lease(lease).await?;

        Ok(stdout)
    }

    /// Retrieve a `ConcreteTopology` matching the given `AbstractTopology` from the free network pool, or create a new one if none are available.
    async fn get_network(
        &self,
        topology: network::AbstractTopology,
    ) -> miette::Result<network::ConcreteTopology> {
        if let Some(concrete_topology) = self
            .free_networks
            .lock()
            .map_err(|err| miette::MietteDiagnostic::new(err.to_string()))?
            .get_mut(&topology)
            .and_then(Vec::pop)
        {
            log::debug!("Reusing network {concrete_topology:?} for topology {topology:?}");
            Ok(concrete_topology)
        } else {
            log::debug!("Creating new network for topology {topology:?}");
            let concrete_topology = self.sidecar.create_network(topology.clone()).await?;
            self.register_dangling(super::DanglingResource::Network(concrete_topology.clone()));
            Ok(concrete_topology)
        }
    }

    /// Execute a command on the given mutable snapshot, returning its stdout and stderr
    /// The stdout will be wrapped in `Ok` if all the data was UTF-8, `Err` if not.
    async fn exec_internal(
        &self,
        state: container_config::ContainerState,
        cmd: String,
        lease: &str,
    ) -> miette::Result<(container_config::ContainerState, Result<String, String>)> {
        let hash = CacheHash::from_data(CacheScope::ExecInputs, &(&state, &cmd)).await?;
        let snapshot_name = base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(*hash);

        let exec_lock = self.exec_lock.acquire().await;
        log::debug!("Preparing to execute {cmd:?} in {state:?}");
        let container_topology = state.into_topology(cmd.into());
        let abstract_topology = container_topology.map_data_ref(|_| ());
        let network_topology = self.get_network(abstract_topology.clone()).await?;
        let complete_topology = container_topology.zip(network_topology.clone());

        let running_topology = self.spinup_topology(complete_topology, lease).await?;
        let handle = running_topology.get_data();

        self.wait_for_exit(handle.id.clone(), String::new()).await?;

        let (container, stdout) = self
            .spindown_topology(running_topology, snapshot_name)
            .await?;

        if let Ok(mut free_networks) = self.free_networks.lock() {
            free_networks
                .entry(abstract_topology)
                .or_default()
                .push(network_topology);
        } else {
            log::warn!("Failed to get lock on free_networks");
        }

        let container = match container {
            container_config::ContainerLike::Container(container) => container,
            container_config::ContainerLike::Service(_) => {
                log::error!("Root of topology was a service, this should never happen");
                return Err(internal(
                    "Invalid container topology: root node was a service".to_owned(),
                ));
            }
        };

        drop(exec_lock);
        Ok((container, stdout))
    }

    /// Spinup a topology tree
    #[expect(clippy::too_many_lines, reason = "Tightly coupled linear task")]
    async fn spinup_topology(
        &self,
        topology: network::Topology<(container_config::ContainerTopologyNode, network::Namespace)>,
        lease: &str,
    ) -> miette::Result<network::Topology<ContainerHandle>> {
        let ((node, network_namespace), children) = topology.into_parts();

        let mut hosts = Vec::new();
        for child in &children {
            let hostname = child.get_data().0.get_hostname();
            let ip = child.get_data().1.ip;

            hosts.push((Arc::clone(&hostname), ip));
        }

        let service_handles = futures_util::future::try_join_all(
            children
                .into_iter()
                .map(|child| self.spinup_topology(child, lease)),
        )
        .await?;

        let mutable_snapshot = uuid::Uuid::new_v4().to_string();
        log::debug!(
            "Creating mutable snapshot {mutable_snapshot:?} from {:?}",
            node.state.snapshot
        );
        let mounts = self
            .containerd
            .snapshot()
            .prepare(
                containerd_services::snapshots::PrepareSnapshotRequest {
                    snapshotter: SNAPSHOTTER.to_owned(),
                    key: mutable_snapshot.clone(),
                    parent: (*node.state.snapshot).to_owned(),
                    labels: HashMap::new(),
                }
                .with_lease(lease),
            )
            .await
            .into_diagnostic()
            .with_context(|| format!("preparing snapshot {mutable_snapshot}"))?
            .into_inner()
            .mounts;
        log::trace!("Mounts: {mounts:?}");

        let (container, process_spec) = self
            .create_container(
                &node.state,
                node.get_cmd().to_owned(),
                &network_namespace.path,
                hosts,
                mounts.clone().into_boxed_slice(),
                lease,
            )
            .await?;

        let (stdout_path, stdout) = self.sidecar.fifo_pipe().await?;

        let log_id = node.get_cmd().to_owned();
        let exec_task = self.reporter.start_task(TaskKind::Exec, log_id.clone());
        let task_id = exec_task.id();

        let stdout = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(Self::read_stdout(
            stdout,
            log_id,
            task_id,
            self.reporter.clone(),
        )));

        log::debug!("Creating task in {container}");
        self.containerd
            .tasks()
            .create(
                containerd_services::CreateTaskRequest {
                    container_id: container.clone(),
                    rootfs: mounts,
                    terminal: false,
                    stdin: String::new(),
                    stdout: stdout_path.display().to_string(),
                    stderr: stdout_path.display().to_string(),
                    checkpoint: None,
                    options: None,
                    runtime_path: String::new(),
                }
                .with_lease(lease),
            )
            .await
            .into_diagnostic()
            .with_context(|| format!("creating a task in {container}"))?
            .into_inner();

        log::debug!("Starting {:?} in {container}", node.get_cmd());
        // A empty `exec_id` signifies the main process of a container
        self.containerd
            .tasks()
            .start(containerd_services::StartRequest {
                container_id: container.clone(),
                exec_id: String::new(),
            })
            .await
            .into_diagnostic()
            .with_context(|| format!("starting the task in {container}"))?;

        if let container_config::ContainerLike::Service(service) = &node.state {
            let (healthcheck, timeout) = &service.get_service_config().healthcheck;
            self.wait_for_command_success(
                container.clone(),
                process_spec,
                Arc::clone(healthcheck),
                *timeout,
                lease,
            )
            .await?;
        }

        Ok(network::Topology::with_children(
            ContainerHandle {
                id: container,
                stdout,
                node,
                snapshot: mutable_snapshot,
                exec_task,
            },
            service_handles,
        ))
    }

    /// Run the given healthcheck command until either timeout time has passed or it returns exit
    /// code 0;
    async fn wait_for_command_success(
        &self,
        container_id: String,
        mut base_process: oci_spec::runtime::Process,
        command: Arc<str>,
        timeout: std::time::Duration,
        lease: &str,
    ) -> miette::Result<()> {
        base_process.set_args(Some(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            command.to_string(),
        ]));

        let task = self
            .reporter
            .start_task(TaskKind::Status, format!("[healthcheck] {command}"));

        let start_time = std::time::Instant::now();
        loop {
            if start_time.elapsed() > timeout {
                return Err(HealthcheckTimeout {
                    check: command.to_string(),
                    timeout,
                }
                .into());
            }

            let exec_id = uuid::Uuid::new_v4().to_string();

            let (stdout_path, stdout) = self.sidecar.fifo_pipe().await?;
            tokio::spawn(Self::read_stdout(
                stdout,
                format!("[healthcheck] {command}"),
                task.id(),
                self.reporter.clone(),
            ));

            log::debug!("Running healthcheck command {command} in {container_id}");
            self.containerd
                .tasks()
                .exec(
                    containerd_services::ExecProcessRequest {
                        container_id: container_id.clone(),
                        exec_id: exec_id.clone(),
                        terminal: false,
                        stdin: String::new(),
                        stdout: stdout_path.display().to_string(),
                        stderr: stdout_path.display().to_string(),
                        spec: Some(prost_types::Any {
                            type_url: "types.containerd.io/opencontainers/runtime-spec/1/Process"
                                .to_owned(),
                            value: serde_json::to_vec(&base_process)
                                .wrap_internal("healthcheck process spec is not serializable")?,
                        }),
                    }
                    .with_lease(lease),
                )
                .await
                .into_diagnostic()
                .with_context(|| format!("running healthcheck {command} in {container_id}"))?;
            self.containerd
                .tasks()
                .start(containerd_services::StartRequest {
                    container_id: container_id.clone(),
                    exec_id: exec_id.clone(),
                })
                .await
                .into_diagnostic()
                .with_context(|| format!("starting healthcheck {command} in {container_id}"))?;

            let exit_code = tokio::select! {
                exit = self.wait_for_exit(container_id.clone(), exec_id.clone()) => {
                    exit?
                }
                () = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                    log::warn!("Healthcheck command {command} is taking a while.");
                    255
                }
            };

            log::debug!("Healthcheck command exited with code {exit_code}");
            if exit_code == 0 {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    /// Create a container according to the given container state and the given command and returns
    /// its id
    #[expect(clippy::too_many_lines, reason = "Tightly coupled linear task")]
    async fn create_container(
        &self,
        state: &container_config::ContainerState,
        cmd: String,
        network_namespace: &str,
        hosts: Vec<(Arc<str>, std::net::Ipv4Addr)>,
        mounts: Box<[containerd_client::types::Mount]>,
        lease: &str,
    ) -> miette::Result<(String, oci_spec::runtime::Process)> {
        let container = uuid::Uuid::new_v4().to_string();
        self.register_dangling(super::DanglingResource::Task(container.clone().into()));

        let mut root = oci_spec::runtime::Root::default();
        root.set_path("rootfs".into());
        root.set_readonly(Some(false));

        let (user, home_dir) = if let Some(user_string) = &state.config.user {
            self.construct_spec_user(mounts, user_string).await?
        } else {
            (oci_spec::runtime::User::default(), "/root".into())
        };

        let mut process = oci_spec::runtime::Process::default();
        process.set_args(Some(vec!["/bin/sh".to_owned(), "-c".to_owned(), cmd]));
        process.set_env(Some(
            state
                .config
                .env
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .chain(std::iter::once(format!("HOME={home_dir}")))
                .collect(),
        ));
        process.set_cwd(
            String::from_utf8_lossy(state.config.working_dir.as_bytes())
                .into_owned()
                .into(),
        );

        process.set_user(user);

        // Use Docker's default capabilities
        let caps: oci_spec::runtime::Capabilities = [
            oci_spec::runtime::Capability::AuditWrite,
            oci_spec::runtime::Capability::Chown,
            oci_spec::runtime::Capability::DacOverride,
            oci_spec::runtime::Capability::Fowner,
            oci_spec::runtime::Capability::Fsetid,
            oci_spec::runtime::Capability::Kill,
            oci_spec::runtime::Capability::Mknod,
            oci_spec::runtime::Capability::NetBindService,
            oci_spec::runtime::Capability::NetRaw,
            oci_spec::runtime::Capability::Setfcap,
            oci_spec::runtime::Capability::Setgid,
            oci_spec::runtime::Capability::Setpcap,
            oci_spec::runtime::Capability::Setuid,
            oci_spec::runtime::Capability::SysChroot,
        ]
        .into_iter()
        .collect();
        #[expect(clippy::expect_used, reason = "Hardcoded values.")]
        let linux_caps = oci_spec::runtime::LinuxCapabilitiesBuilder::default()
            .bounding(caps.clone())
            .effective(caps.clone())
            .inheritable(caps.clone())
            .permitted(caps.clone())
            // .ambient(caps)
            .build()
            .expect("capabilities should be valid");

        process.set_capabilities(Some(linux_caps));

        let mut linux = oci_spec::runtime::Linux::default();
        if let Some(namespaces) = linux.namespaces_mut()
            && let Some(namespace) = namespaces
                .iter_mut()
                .find(|namespace| namespace.typ() == oci_spec::runtime::LinuxNamespaceType::Network)
        {
            namespace.set_path(Some(network_namespace.into()));
        }

        let mut spec = oci_spec::runtime::Spec::default();

        let mut dns_mount = oci_spec::runtime::Mount::default();
        dns_mount
            .set_typ(Some("bind".to_owned()))
            .set_source(Some("/etc/resolv.conf".into()))
            .set_destination("/etc/resolv.conf".into())
            .set_options(Some(vec!["ro".to_owned(), "bind".to_owned()]));
        spec.mounts_mut().get_or_insert_default().push(dns_mount);

        let hosts_mount = self.write_hosts_file(hosts).await?;
        spec.mounts_mut().get_or_insert_default().push(hosts_mount);

        spec.set_root(Some(root))
            .set_process(Some(process.clone()))
            .set_linux(Some(linux));

        if let Ok(json) = serde_json::to_string(&spec) {
            log::trace!("SPEC: {json}");
        }

        log::debug!("Creating container {container}");
        self.containerd
            .containers()
            .create(
                containerd_services::CreateContainerRequest {
                    container: Some(containerd_services::Container {
                        id: container.clone(),
                        snapshotter: SNAPSHOTTER.to_owned(),
                        snapshot_key: (*state.snapshot).to_owned(),
                        runtime: Some(containerd_services::container::Runtime {
                            name: "io.containerd.runc.v2".to_owned(),
                            options: None,
                        }),
                        spec: Some(prost_types::Any {
                            type_url: "types.containerd.io/opencontainers/runtime-spec/1/Spec"
                                .to_owned(),
                            value: serde_json::to_vec(&spec)
                                .wrap_internal("container spec is not serializable")?,
                        }),
                        sandbox: String::new(),
                        updated_at: None,
                        labels: HashMap::new(),
                        image: String::new(),
                        created_at: None,
                        extensions: HashMap::new(),
                    }),
                }
                .with_lease(lease),
            )
            .await
            .into_diagnostic()
            .with_context(|| format!("creating container {container}"))?;

        Ok((container, process))
    }

    /// Read the stdout to a String, returns `Err` if encountered non-utf (containing the output
    /// without those lines), and `Ok` if all data was utf-8
    async fn read_stdout(
        stdout: impl AsyncRead + Unpin + Send + 'static,
        log_id: String,
        task_id: TaskId,
        reporter: Reporter,
    ) -> Result<String, String> {
        let mut stdout = tokio::io::BufReader::new(stdout).lines();
        let mut result = String::new();
        let mut success = true;

        loop {
            match stdout.next_line().await {
                Ok(None) => break,
                Ok(Some(line)) => {
                    let line = strip_ansi_escapes::strip_str(line);

                    log::trace!("{log_id}: {line}");
                    reporter.task_line(task_id, line.clone().into());

                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str(&line);
                }
                Err(err) => {
                    log::error!("Error reading stdout: {err:?}");
                    success = false;

                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str("<<NON_UTF8_ON_LINE>>");
                }
            }
        }

        if success { Ok(result) } else { Err(result) }
    }

    /// read the /etc/passwd file to supplement the info given by the container config and
    /// construct a full user object.
    ///
    /// Also returns the home directory.
    async fn construct_spec_user(
        &self,
        mounts: Box<[containerd_client::types::Mount]>,
        user: &str,
    ) -> miette::Result<(oci_spec::runtime::User, Box<str>)> {
        let Ok(passwd) = self
            .read_file(mounts.clone(), UnixPathBuf::from("/etc/passwd"))
            .await
            .context("reading /etc/passwd from the container")?
            .parse();
        let Ok(groups) = self
            .read_file(mounts, UnixPathBuf::from("/etc/group"))
            .await
            .context("reading /etc/group from the container")?
            .parse();

        let Ok(user): Result<userdb::OciUser, _> = user.parse();
        let (user, home_dir) = user.resolve(passwd, &groups)?;

        Ok((user, home_dir))
    }

    /// Write the hosts file to the sidecar and return a appropriate bind mount for it.
    async fn write_hosts_file(
        &self,
        hosts: Vec<(Arc<str>, std::net::Ipv4Addr)>,
    ) -> miette::Result<oci_spec::runtime::Mount> {
        let hosts_content = hosts
            .into_iter()
            .map(|(hostname, ip)| format!("{ip}\t{hostname}"))
            .chain(["127.0.0.1 localhost".to_owned(), "::1 localhost".to_owned()])
            .collect::<Vec<_>>()
            .join("\n");

        let file_name = format!("hosts-{}", uuid::Uuid::new_v4());

        // We re-use the sidecars "write into mount" functionality to write a file into the sidecar
        // itself.
        let temp_dir_mount = containerd_client::types::Mount {
            r#type: "bind".to_owned(),
            source: "/run/serpentine".to_owned(),
            target: String::new(),
            options: vec!["rw".to_owned(), "bind".to_owned()],
        };
        self.write_file(
            vec![temp_dir_mount],
            UnixPathBuf::from(&file_name),
            hosts_content.as_bytes(),
        )
        .await
        .with_context(|| format!("writing the hosts file {file_name}"))?;

        let mut mount = oci_spec::runtime::Mount::default();
        mount
            .set_typ(Some("bind".to_owned()))
            .set_source(Some(format!("/run/serpentine/{file_name}").into()))
            .set_destination("/etc/hosts".into())
            .set_options(Some(vec!["ro".to_owned(), "bind".to_owned()]));
        Ok(mount)
    }

    /// Wait for the given container handle to exit.
    ///
    /// Returns the processes exit code.
    async fn wait_for_exit(&self, container_id: String, exec_id: String) -> miette::Result<u32> {
        log::debug!("Waiting for {container_id}/{exec_id} to exit.");

        let exit_code = self
            .containerd
            .tasks()
            .wait(containerd_services::WaitRequest {
                container_id,
                exec_id,
            })
            .await
            .into_diagnostic()
            .context("waiting for the task to exit")?
            .into_inner()
            .exit_status;

        Ok(exit_code)
    }

    /// Spin down a given topology of running containers.
    ///
    /// Takes the name to commit snapshots under, this should be unique, so ideally CAC.
    /// Services will be committed under `{snapshot_name}/{hostname}`
    async fn spindown_topology(
        &self,
        containers: network::Topology<ContainerHandle>,
        snapshot_name: String,
    ) -> miette::Result<(container_config::ContainerLike, Result<String, String>)> {
        const SIGINT: u32 = 2;
        const SIGKILL: u32 = 9;

        let (handle, children) = containers.into_parts();

        self.send_signal(handle.id.clone(), SIGINT).await;
        let exit_code = tokio::select! {
            result = self.wait_for_exit(handle.id.clone(), String::new()) => {
                log::debug!("{} exited gracefully.", handle.id);
                result?
            }
            () = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                log::debug!("{} did not exit after 10 seconds, sending SIGKILL.", handle.id);
                self.send_signal(handle.id.clone(), SIGKILL).await;
                self.wait_for_exit(handle.id.clone(), String::new()).await?
            }
        };

        drop(handle.exec_task);

        let commit = self
            .containerd
            .snapshot()
            .commit(containerd_services::snapshots::CommitSnapshotRequest {
                snapshotter: SNAPSHOTTER.to_owned(),
                name: snapshot_name.clone(),
                key: handle.snapshot.clone(),
                labels: HashMap::from([(CONTAINERD_GC_ROOT_LABEL.to_owned(), "1".to_owned())]),
            })
            .await;
        if let Err(status) = commit
            && !Self::is_already_exists(&status)
        {
            return Err(status)
                .into_diagnostic()
                .with_context(|| format!("committing snapshot {snapshot_name}"));
        }

        let stdout = handle
            .stdout
            .await
            .wrap_internal("stdout reader task panicked")?;

        if exit_code != 0 {
            if matches!(
                handle.node.state,
                container_config::ContainerLike::Service(_)
            ) {
                log::warn!(
                    "Service {} exited with code {exit_code}, this may be expected if the service didnt shutdown in time.",
                    handle.id
                );
            } else {
                let stdout = stdout.unwrap_or_else(|err| err);

                return Err(CommandFailed {
                    code: exit_code,
                    command: handle.node.get_cmd().to_owned(),
                    output: stdout,
                }
                .into());
            }
        }

        let mut services = BTreeMap::new();
        for child in children {
            let Some(hostname) = &child.get_data().node.hostname else {
                return Err(internal("Child container missing hostname".to_owned()));
            };
            let hostname = Arc::clone(hostname);

            let (child_container, _) =
                Box::pin(self.spindown_topology(child, format!("{snapshot_name}/{hostname}")))
                    .await?;

            if let container_config::ContainerLike::Service(service) = child_container {
                services.insert(hostname, service);
            } else {
                return Err(internal("Expected a service".to_owned()));
            }
        }

        let mut container = handle.node.state;
        container.snapshot = snapshot_name.into();
        let container = container.update_config(move |config| {
            config.services = services;
        });

        Ok((container, stdout))
    }

    /// Send the given signal to the specified task id.
    ///
    /// This ignores any errors with terminating the task
    async fn send_signal(&self, container_id: String, signal: u32) {
        log::debug!("Sending {signal} to {container_id}");

        let res = self
            .containerd
            .tasks()
            .kill(containerd_services::KillRequest {
                container_id,
                exec_id: String::new(),
                signal,
                all: false,
            })
            .await;

        if let Err(err) = res {
            log::error!("Failed to send signal: {err}");
        }
    }
}

#[cfg(test)]
#[cfg(feature = "_test_docker")]
mod integration_tests {
    use rstest::rstest;
    use typed_path::UnixPath;

    use crate::engine::containerd::Client;
    use crate::engine::containerd::test_support::{TEST_IMAGE, containerd_client};

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn exec_in_container(
        #[future] containerd_client: miette::Result<Client>,
    ) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let image = containerd_client.pull_image(TEST_IMAGE).await?;
        containerd_client
            .exec(&image, "echo hello world".to_owned())
            .await?;

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn exec_in_container_fail(
        #[future] containerd_client: miette::Result<Client>,
    ) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let image = containerd_client.pull_image(TEST_IMAGE).await?;

        let res = containerd_client
            .exec(&image, "cat hello.txt".to_owned())
            .await;
        assert!(res.is_err(), "Expected exec to fail");

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn exec_cmd_not_found(
        #[future] containerd_client: miette::Result<Client>,
    ) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let image = containerd_client.pull_image(TEST_IMAGE).await?;

        let res = containerd_client
            .exec(&image, "I_AM_NOT_REAL".to_owned())
            .await;
        assert!(res.is_err(), "Expected exec to fail");

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn chained_exec(
        #[future] containerd_client: miette::Result<Client>,
    ) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let image = containerd_client.pull_image(TEST_IMAGE).await?;

        let image = containerd_client
            .exec(&image, "touch /tmp/hello".to_owned())
            .await?;

        containerd_client
            .exec(&image, "cat /tmp/hello".to_owned())
            .await?;

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn forked_image(
        #[future] containerd_client: miette::Result<Client>,
    ) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let image = containerd_client.pull_image(TEST_IMAGE).await?;

        let image = containerd_client
            .exec(&image, "touch /tmp/hello".to_owned())
            .await?;

        containerd_client
            .exec(&image, "rm /tmp/hello".to_owned())
            .await?;

        containerd_client
            .exec(&image, "cat /tmp/hello".to_owned())
            .await?;

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn exec_output(
        #[future] containerd_client: miette::Result<Client>,
    ) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let image = containerd_client.pull_image(TEST_IMAGE).await?;
        let output = containerd_client
            .exec_get_output(&image, "echo -n hello world".to_owned())
            .await?;

        assert_eq!(output, "hello world");

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn exec_output_has_writable_filesystem(
        #[future] containerd_client: miette::Result<Client>,
    ) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let image = containerd_client.pull_image(TEST_IMAGE).await?;
        let output = containerd_client
            .exec_get_output(&image, "echo hello world > hello.txt".to_owned())
            .await?;
        assert_eq!(output, "");

        // Ensure we didnt modify the filesystem in `image`
        let result = containerd_client
            .exec(&image, "cat hello.txt".to_owned())
            .await;
        if result.is_ok() {
            miette::bail!("File was created in filesystem when it shouldnt have been");
        }

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn exec_non_utf8(
        #[future] containerd_client: miette::Result<Client>,
    ) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let image = containerd_client.pull_image(TEST_IMAGE).await?;
        containerd_client
            .exec(&image, r"printf '\xff\xfe\xfa'".to_owned())
            .await?;

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn exec_output_non_utf8(
        #[future] containerd_client: miette::Result<Client>,
    ) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let image = containerd_client.pull_image(TEST_IMAGE).await?;
        let output = containerd_client
            .exec_get_output(&image, r"printf '\xff\xfe\xfa'".to_owned())
            .await;

        assert!(
            output.is_err(),
            "No way to represent the non-utf8 data, so should be a error"
        );

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn set_working_dir(
        #[future] containerd_client: miette::Result<Client>,
    ) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let image = containerd_client.pull_image(TEST_IMAGE).await?;
        let image = containerd_client
            .exec(&image, "mkdir -p /foo/bar".to_owned())
            .await?;
        let image = image.update_config(|config| config.set_working_dir(UnixPath::new("/foo")));
        containerd_client.exec(&image, "ls bar".to_owned()).await?;

        let image = image.update_config(|config| config.set_working_dir(UnixPath::new("./bar")));
        let working_dir_pwd = containerd_client
            .exec_get_output(&image, "pwd".to_owned())
            .await?;
        assert_eq!(
            working_dir_pwd.trim(),
            "/foo/bar".to_owned(),
            "pwd reported wrong working directory"
        );

        let image = image.update_config(|config| config.set_working_dir(UnixPath::new("/app")));
        let working_absolute_dir_pwd = containerd_client
            .exec_get_output(&image, "pwd".to_owned())
            .await?;
        assert_eq!(
            working_absolute_dir_pwd.trim(),
            "/app".to_owned(),
            "pwd reported wrong working directory"
        );

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn set_env_var(
        #[future] containerd_client: miette::Result<Client>,
    ) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let image = containerd_client.pull_image(TEST_IMAGE).await?;
        let image =
            image.update_config(|config| config.set_env_var("HELLO".into(), "WORLD".into()));
        let exec = containerd_client
            .exec_get_output(&image, "echo -n $HELLO".to_owned())
            .await?;
        let get_env = image
            .get_config()
            .get_env_var("HELLO")
            .ok_or_else(|| miette::miette!("HELLO not set in the image config"))?;

        assert_eq!(exec, "WORLD", "echo $HELLO");
        assert_eq!(get_env.as_ref(), "WORLD", "get_env");

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn network_access(
        #[future] containerd_client: miette::Result<Client>,
    ) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let image = containerd_client.pull_image(TEST_IMAGE).await?;
        containerd_client
            .exec(&image, "curl 1.1.1.1".to_owned())
            .await?;

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn dns_access(#[future] containerd_client: miette::Result<Client>) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let image = containerd_client.pull_image(TEST_IMAGE).await?;
        containerd_client
            .exec(&image, "curl https://google.com".to_owned())
            .await?;

        Ok(())
    }
}
