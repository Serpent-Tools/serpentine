//! Wrapper around bollard Docker API client

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use containerd_client::services::v1 as containerd_services;
use futures_util::{StreamExt, TryStreamExt};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::engine::RuntimeError;
use crate::engine::data_model::FileSystem;
use crate::tui::TuiSender;

/// The snapshoter to use for containers.
const SNAPSHOTER: &str = "overlayfs";

/// A reference to a specific state of a container.
#[derive(Clone, Hash, PartialEq, Eq, Debug, bincode::Encode, bincode::Decode)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub struct ContainerState(Rc<str>);

/// Return the goarch of the current system.
fn system_goarch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "i686" | "i386" => "386",
        "arm" => "arm",
        "s390x" => "s390x",
        "powerpc64" | "powerpc64le" => "ppc64le",
        arch => {
            log::warn!("Unknown arch {arch}");
            arch
        }
    }
}

/// Return wether the given Oci platform object is compatible with the current system.
fn platform_resolver(manifests: &[oci_client::manifest::ImageIndexEntry]) -> Option<String> {
    manifests
        .iter()
        .find(|manifest| match &manifest.platform {
            None => false,
            Some(platform) => platform.os == "linux" && platform.architecture == system_goarch(),
        })
        .map(|manifest| manifest.digest.clone())
}

/// Thin wrapper around `containerd_client::Client` to apply namespace interceptor
struct ContainerdRootClient(containerd_client::Client);

/// Injects the serpentine namespace into all requests
#[expect(clippy::expect_used, reason = "constant value")]
#[expect(
    clippy::result_large_err,
    clippy::unnecessary_wraps,
    reason = "This is the signature needed by tonic"
)]
fn inject_namespace(
    mut request: containerd_client::tonic::Request<()>,
) -> containerd_client::tonic::Result<containerd_client::tonic::Request<()>> {
    request.metadata_mut().insert(
        "containerd-namespace",
        "serpentine".parse().expect("Invalid namespace"),
    );
    Ok(request)
}

/// Generate the getter wrappers for `ContainerdRootClient`
macro_rules! sub_client_wrapper {
    ($method:ident, $($type:ident)::+) => {
        #[must_use]
        fn $method(
            &self,
        ) -> containerd_services::$($type)::+<
            containerd_client::tonic::service::interceptor::InterceptedService<
                containerd_client::tonic::transport::Channel,
                impl containerd_client::tonic::service::interceptor::Interceptor,
            >,
        > {
            containerd_services::$($type)::+::with_interceptor(self.0.channel(), inject_namespace)
        }
    };
}

impl ContainerdRootClient {
    sub_client_wrapper!(containers, containers_client::ContainersClient);
    sub_client_wrapper!(content, content_client::ContentClient);
    sub_client_wrapper!(snapshot, snapshots::snapshots_client::SnapshotsClient);
    sub_client_wrapper!(diff, diff_client::DiffClient);
    sub_client_wrapper!(tasks, tasks_client::TasksClient);
}

/// A docker client wrapper
pub struct Client {
    /// containerd client
    containerd: ContainerdRootClient,
    /// Container registery client
    oci: oci_client::Client,
    /// Sender to the TUI
    tui: TuiSender,
    /// Limiter on the amount of exec jobs running at once
    exec_lock: tokio::sync::Semaphore,
}

impl Client {
    /// Create a new Docker client
    pub async fn new(tui: TuiSender, exec_permits: usize) -> Result<Self, RuntimeError> {
        let oci = oci_client::Client::new(oci_client::client::ClientConfig {
            user_agent: concat!("serpentine/", env!("CARGO_PKG_VERSION")),
            platform_resolver: Some(Box::new(platform_resolver)),
            ..Default::default()
        });

        Ok(Self {
            containerd: ContainerdRootClient(crate::engine::docker::connect().await?),
            oci,
            tui,
            exec_lock: tokio::sync::Semaphore::new(exec_permits),
        })
    }

    /// Check if a state exists
    pub async fn exists(&self, snapshot: &ContainerState) -> bool {
        self.containerd
            .snapshot()
            .stat(containerd_services::snapshots::StatSnapshotRequest {
                snapshotter: SNAPSHOTER.to_owned(),
                key: (*snapshot.0).to_owned(),
            })
            .await
            .is_ok()
    }

    /// download the given image if necessary
    pub async fn pull_image(&self, image: &str) -> Result<ContainerState, RuntimeError> {
        let image = oci_client::Reference::try_from(image)?;
        let auth = oci_client::secrets::RegistryAuth::Anonymous;

        let final_container_state = ContainerState(image.to_string().into());
        if self.exists(&final_container_state).await {
            log::debug!("image {image} already exists");
            return Ok(final_container_state);
        }

        log::debug!("Pulling image {image}");
        let (manifest, _) = self.oci.pull_image_manifest(&image, &auth).await?;

        futures_util::future::try_join_all(
            manifest
                .layers
                .iter()
                .map(|layer| self.pull_layer(&image, layer)),
        )
        .await?;

        log::debug!("Uploaded all layers to containerd");
        log::debug!("Creating snapshot from image");

        let key = uuid::Uuid::new_v4().to_string();
        let mounts = self
            .containerd
            .snapshot()
            .prepare(containerd_services::snapshots::PrepareSnapshotRequest {
                key: key.clone(),
                snapshotter: SNAPSHOTER.to_owned(),
                labels: HashMap::from([("containerd.io/gc.root".to_owned(), "1".to_owned())]),
                parent: String::new(),
            })
            .await?
            .into_inner()
            .mounts;
        for layer in manifest.layers {
            log::debug!("Applying layer {}", layer.digest);

            self.containerd
                .diff()
                .apply(containerd_services::ApplyRequest {
                    diff: Some(containerd_client::types::Descriptor {
                        media_type: layer.media_type,
                        digest: layer.digest,
                        size: layer.size,
                        annotations: HashMap::new(),
                    }),
                    mounts: mounts.clone(),
                    payloads: HashMap::new(),
                    sync_fs: false,
                })
                .await?;
        }

        log::debug!("Commiting {key} to {image}");
        self.containerd
            .snapshot()
            .commit(containerd_services::snapshots::CommitSnapshotRequest {
                snapshotter: SNAPSHOTER.to_owned(),
                name: image.to_string(),
                key: key.clone(),
                labels: HashMap::from([("containerd.io/gc.root".to_owned(), "1".to_owned())]),
            })
            .await?;

        log::debug!("Created snapshot {image}");
        Ok(final_container_state)
    }

    /// Pull the given layer into containerd.
    async fn pull_layer(
        &self,
        image: &oci_client::Reference,
        layer: &oci_client::manifest::OciDescriptor,
    ) -> Result<(), RuntimeError> {
        if self
            .containerd
            .content()
            .read(containerd_services::ReadContentRequest {
                digest: layer.digest.clone(),
                offset: 0,
                size: 1,
            })
            .await
            .is_ok()
        {
            log::debug!("layer {} already exists", layer.digest);
            return Ok(());
        }

        log::debug!("Pulling layer {layer}");

        let layer_stream = self.oci.pull_blob_stream(image, &layer).await?;
        let total_size = layer_stream
            .content_length
            .and_then(|len| len.try_into().ok())
            .unwrap_or(0);
        let upload_ref = uuid::Uuid::new_v4().to_string();
        let upload_ref_clone = upload_ref.clone();
        let digest = layer.digest.clone();
        let digest_clone = digest.clone();

        self.containerd
            .content()
            .write(
                layer_stream
                    .filter_map(async |layer_data| layer_data.ok())
                    .scan(0_usize, move |current_offset, layer_data| {
                        let write = containerd_services::WriteContentRequest {
                            action: containerd_services::WriteAction::Write.into(),
                            r#ref: upload_ref.clone(),
                            total: total_size,
                            expected: digest.clone(),
                            offset: (*current_offset).try_into().unwrap_or(0),
                            data: layer_data.to_vec(),
                            labels: HashMap::new(),
                        };
                        *current_offset = current_offset.saturating_add(layer_data.len());
                        futures_util::future::ready(Some(write))
                    }),
            )
            .await?
            .into_inner()
            .try_for_each(async |_| Ok(()))
            .await?;

        log::debug!("Finished pulling {digest_clone}.");
        self.containerd
            .content()
            .write(futures_util::stream::iter(std::iter::once(
                containerd_services::WriteContentRequest {
                    action: containerd_services::WriteAction::Commit.into(),
                    r#ref: upload_ref_clone,
                    total: total_size,
                    expected: digest_clone,
                    offset: total_size,
                    data: Vec::new(),
                    labels: HashMap::new(),
                },
            )))
            .await?
            .into_inner()
            .try_for_each(async |_| Ok(()))
            .await?;

        Ok::<_, RuntimeError>(())
    }

    /// Execute a command on top of a given state and reuturn a new state representing the result
    pub async fn exec(
        &self,
        state: &ContainerState,
        cmd: String,
    ) -> Result<ContainerState, RuntimeError> {
        let snapshot = uuid::Uuid::new_v4().to_string();
        self.containerd
            .snapshot()
            .prepare(containerd_services::snapshots::PrepareSnapshotRequest {
                snapshotter: SNAPSHOTER.to_owned(),
                key: snapshot.clone(),
                parent: (*state.0).to_owned(),
                labels: HashMap::from([("containerd.io/gc.root".to_owned(), "1".to_owned())]),
            })
            .await?;

        self.exec_internal(&ContainerState(Rc::from(snapshot.clone())), cmd)
            .await?;

        let new_snapshot = uuid::Uuid::new_v4().to_string();
        self.containerd
            .snapshot()
            .commit(containerd_services::snapshots::CommitSnapshotRequest {
                snapshotter: SNAPSHOTER.to_owned(),
                name: new_snapshot.clone(),
                key: snapshot.clone(),
                labels: HashMap::from([("containerd.io/gc.root".to_owned(), "1".to_owned())]),
            })
            .await?;

        Ok(ContainerState(new_snapshot.into()))
    }

    /// Execute a command return its stdout and stderr.
    pub async fn exec_get_output(
        &self,
        state: &ContainerState,
        cmd: String,
    ) -> Result<String, RuntimeError> {
        let snapshot = uuid::Uuid::new_v4().to_string();
        self.containerd
            .snapshot()
            .prepare(containerd_services::snapshots::PrepareSnapshotRequest {
                snapshotter: SNAPSHOTER.to_owned(),
                key: snapshot.clone(),
                parent: (*state.0).to_owned(),
                labels: HashMap::new(),
            })
            .await?;

        let output = self
            .exec_internal(&ContainerState(Rc::from(snapshot.clone())), cmd)
            .await?;
        self.containerd
            .snapshot()
            .remove(containerd_services::snapshots::RemoveSnapshotRequest {
                snapshotter: SNAPSHOTER.to_owned(),
                key: snapshot,
            })
            .await?;

        Ok(output)
    }

    /// Execute a command on the given mutable snapshot, returning its stdout and stderr
    async fn exec_internal(
        &self,
        snapshot: &ContainerState,
        cmd: String,
    ) -> Result<String, RuntimeError> {
        log::debug!("Prepearing to execute {cmd:?} in {snapshot:?}");
        let container = uuid::Uuid::new_v4().to_string();

        let mut root = oci_spec::runtime::Root::default();
        root.set_path(PathBuf::from("rootfs"));
        root.set_readonly(Some(false));

        let mut process = oci_spec::runtime::Process::default();
        process.set_args(Some(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            cmd.clone(),
        ]));

        log::debug!("Loading mounts for {:?}", snapshot.0);
        let mounts_proto = self
            .containerd
            .snapshot()
            .mounts(containerd_services::snapshots::MountsRequest {
                snapshotter: SNAPSHOTER.to_owned(),
                key: (*snapshot.0).to_owned(),
            })
            .await?
            .into_inner()
            .mounts;

        log::debug!("Creating container {container}");
        self.containerd
            .containers()
            .create(containerd_services::CreateContainerRequest {
                container: Some(containerd_services::Container {
                    id: container.clone(),
                    snapshotter: SNAPSHOTER.to_owned(),
                    snapshot_key: (*snapshot.0).to_owned(),
                    runtime: Some(containerd_services::container::Runtime {
                        name: "io.containerd.runc.v2".to_owned(),
                        options: None,
                    }),
                    spec: Some(prost_types::Any {
                        type_url: "types.containerd.io/opencontainers/runtime-spec/1/Spec"
                            .to_owned(),
                        value: serde_json::to_vec(
                            &oci_spec::runtime::Spec::default()
                                .set_root(Some(root))
                                .set_process(Some(process)),
                        )
                        .map_err(|err| RuntimeError::internal(format!("{err}")))?,
                    }),
                    sandbox: String::new(),
                    updated_at: None,
                    labels: HashMap::new(),
                    image: String::new(),
                    created_at: None,
                    extensions: HashMap::new(),
                }),
            })
            .await?;

        let stdout = PathBuf::from("pipes")
            .join(uuid::Uuid::new_v4().to_string())
            .with_extension("pipe");
        let (host_stdout, container_stdout) = crate::engine::docker::path_pair(stdout);
        log::trace!(
            "Creating fifo at {} ({} in container)",
            host_stdout.display(),
            container_stdout.display()
        );
        tokio::fs::create_dir_all(host_stdout.parent().unwrap_or(&host_stdout)).await?;
        nix::unistd::mkfifo(
            &host_stdout,
            nix::sys::stat::Mode::S_IRWXU | nix::sys::stat::Mode::S_IRWXO,
        )
        .map_err(std::io::Error::other)?;
        let stdout_future = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(
            tokio::fs::read_to_string(host_stdout),
        ));

        log::debug!("Creating task in {container}");
        self.containerd
            .tasks()
            .create(containerd_services::CreateTaskRequest {
                container_id: container.clone(),
                rootfs: mounts_proto,
                terminal: false,
                stdin: String::new(),
                stdout: container_stdout.display().to_string(),
                stderr: container_stdout.display().to_string(),
                checkpoint: None,
                options: None,
                runtime_path: String::new(),
            })
            .await?
            .into_inner();

        // A empty `exec_id` signifies the main process of a container
        log::debug!("Starting {cmd:?} in {container}");
        self.containerd
            .tasks()
            .start(containerd_services::StartRequest {
                container_id: container.clone(),
                exec_id: String::new(),
            })
            .await?;

        let exit_code = self
            .containerd
            .tasks()
            .wait(containerd_services::WaitRequest {
                container_id: container.clone(),
                exec_id: String::new(),
            })
            .await?
            .into_inner()
            .exit_status;

        let stdout_result = stdout_future
            .await
            .map_err(|err| RuntimeError::internal(err.to_string()))??;

        self.containerd
            .tasks()
            .delete(containerd_services::DeleteTaskRequest {
                container_id: container.clone(),
            })
            .await?;
        self.containerd
            .containers()
            .delete(containerd_services::DeleteContainerRequest { id: container })
            .await?;

        log::debug!("Got exit code {exit_code}");
        if exit_code == 0 {
            Ok(stdout_result)
        } else {
            Err(RuntimeError::CommandExecution {
                code: exit_code.into(),
                command: cmd,
                output: stdout_result,
            })
        }
    }

    /// Copy the given file/directory into the container
    pub async fn copy_fs_into_container(
        &self,
        container: &ContainerState,
        src: FileSystem,
        dest: &str,
    ) -> Result<ContainerState, RuntimeError> {
        todo!()
    }

    /// Export the given path from the container into a `FileSystem`
    pub async fn export_path(
        &self,
        image: &ContainerState,
        docker_path: &str,
    ) -> Result<FileSystem, RuntimeError> {
        todo!()
    }

    /// Set the working directory of the container
    pub async fn set_working_dir(
        &self,
        image: &ContainerState,
        dir: &str,
    ) -> Result<ContainerState, RuntimeError> {
        todo!()
    }

    /// Set a environment variable in the container
    pub async fn set_env_var(
        &self,
        image: &ContainerState,
        env: &str,
        value: &str,
    ) -> Result<ContainerState, RuntimeError> {
        todo!()
    }

    /// Delete a image.
    /// This should only be used during cleanup.
    ///
    /// This ignores error (just logs them), as its intended for cleanup.
    pub async fn delete_state(&self, image: &ContainerState) {
        todo!()
    }

    /// Export the given images to the target file
    pub async fn export<Target>(
        &self,
        images: impl Iterator<Item = &ContainerState>,
        mut target: Target,
    ) -> Result<(), RuntimeError>
    where
        Target: AsyncWrite + Unpin,
    {
        todo!()
    }

    /// Import the given docker export to this docker client.
    pub async fn import(
        &self,
        mut file: impl AsyncRead + Send + Unpin + 'static,
    ) -> Result<(), RuntimeError> {
        todo!()
    }
}

#[cfg(test)]
#[cfg(feature = "_test_docker")]
#[expect(clippy::expect_used, reason = "Tests")]
mod tests {
    use rstest::{fixture, rstest};

    use super::*;

    const TEST_IMAGE: &str = "quay.io/toolbx-images/alpine-toolbox:latest";

    #[fixture]
    async fn containerd_client() -> Client {
        Client::new(TuiSender(None), 1)
            .await
            .expect("Failed to create Docker client")
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn pull_image(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn exec_in_container(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let image = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        containerd_client
            .exec(&image, "echo hello world".to_owned())
            .await
            .expect("Failed to exec in container");
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn exec_in_container_fail(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let image = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");

        let res = containerd_client
            .exec(&image, "cat hello.txt".to_owned())
            .await;
        assert!(res.is_err(), "Expected exec to fail");
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn exec_cmd_not_found(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let image = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");

        let res = containerd_client
            .exec(&image, "I_AM_NOT_REAL".to_owned())
            .await;
        assert!(res.is_err(), "Expected exec to fail");
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn chained_exec(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let image = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");

        let image = containerd_client
            .exec(&image, "touch /tmp/hello".to_owned())
            .await
            .expect("Exec failed");

        containerd_client
            .exec(&image, "cat /tmp/hello".to_owned())
            .await
            .expect("Exec failed");
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn forked_image(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let image = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");

        let image = containerd_client
            .exec(&image, "touch /tmp/hello".to_owned())
            .await
            .expect("Exec failed");

        containerd_client
            .exec(&image, "rm /tmp/hello".to_owned())
            .await
            .expect("Exec failed");

        containerd_client
            .exec(&image, "cat /tmp/hello".to_owned())
            .await
            .expect("Exec failed");
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn exec_output(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let image = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let output = containerd_client
            .exec_get_output(&image, "echo -n hello world".to_owned())
            .await
            .expect("Failed to exec in container");

        assert_eq!(output, "hello world");
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn exec_output_has_writale_filesystem(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let image = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let output = containerd_client
            .exec_get_output(&image, "echo hello world > hello.txt".to_owned())
            .await
            .expect("Failed to exec in container");
        assert_eq!(output, "");

        // Ensure we didnt modify the filesystem in `image`
        containerd_client
            .exec(&image, "cat hello.txt".to_owned())
            .await
            .expect_err("File was created in filesystem when it shouldnt have been");
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn copy_file_between_containers(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let base = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");

        let from = containerd_client
            .exec(&base, "echo hello > /tmp/hello.txt".to_owned())
            .await
            .expect("Exec failed");

        let file = containerd_client
            .export_path(&from, "/tmp/hello.txt")
            .await
            .expect("Export failed");

        let to = containerd_client
            .copy_fs_into_container(&base, file, "nice.txt")
            .await
            .expect("Failed to copy into container");

        containerd_client
            .exec(&to, "grep -q hello nice.txt || exit 1".to_owned())
            .await
            .expect("Exec failed");
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn copy_folder_between_containers(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let base = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");

        let from = containerd_client
            .exec(&base, "mkdir -p /tmp/foo/bar/baz".to_owned())
            .await
            .expect("Exec failed");

        let from = containerd_client
            .exec(&from, "echo hello > /tmp/foo/bar/baz/nice.txt".to_owned())
            .await
            .expect("Exec failed");

        let file = containerd_client
            .export_path(&from, "/tmp/foo")
            .await
            .expect("Export failed");

        let to = containerd_client
            .copy_fs_into_container(&base, file, "hello")
            .await
            .expect("Failed to copy into container");

        containerd_client
            .exec(&to, "ls hello/bar/baz".to_owned())
            .await
            .expect("Exec failed");

        containerd_client
            .exec(
                &to,
                "grep -q hello hello/bar/baz/nice.txt || exit 1".to_owned(),
            )
            .await
            .expect("Exec failed");
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn copy_folder_between_containers_relative_paths(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let base = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let base = containerd_client
            .set_working_dir(&base, "/testing")
            .await
            .expect("Failed to set working dir");

        let from = containerd_client
            .exec(&base, "mkdir -p ./foo/bar/baz".to_owned())
            .await
            .expect("Exec failed");

        let from = containerd_client
            .exec(&from, "echo hello > ./foo/bar/baz/nice.txt".to_owned())
            .await
            .expect("Exec failed");

        let file = containerd_client
            .export_path(&from, "./foo")
            .await
            .expect("Export failed");

        let to = containerd_client
            .copy_fs_into_container(&base, file, "./hello")
            .await
            .expect("Failed to copy into container");

        containerd_client
            .exec(&to, "ls ./hello/bar/baz".to_owned())
            .await
            .expect("Exec failed");

        containerd_client
            .exec(
                &to,
                "grep -q hello ./hello/bar/baz/nice.txt || exit 1".to_owned(),
            )
            .await
            .expect("Exec failed");
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn export_path_not_found(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let base = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");

        let result = containerd_client
            .export_path(&base, "i_am_not_real.txt")
            .await;
        assert!(
            result.is_err(),
            "Expected reading non-existent path to fail"
        );
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn set_working_dir(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let image = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let image = containerd_client
            .exec(&image, "mkdir -p /foo/bar".to_owned())
            .await
            .expect("Exec failed");
        let image = containerd_client
            .set_working_dir(&image, "/foo")
            .await
            .expect("Failed to set working dir");
        containerd_client
            .exec(&image, "ls bar".to_owned())
            .await
            .expect("Exec failed");

        let working_dir_pwd = containerd_client
            .exec_get_output(&image, "pwd".to_owned())
            .await
            .expect("Exec failed");
        assert_eq!(
            working_dir_pwd,
            "/foo".to_owned(),
            "pwd reported wrong working directory"
        );
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn set_env_var(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let image = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let image = containerd_client
            .set_env_var(&image, "HELLO", "WORLD")
            .await
            .expect("Failed to set env");
        containerd_client
            .exec(&image, "echo $HELLO | grep -q WORLD".to_owned())
            .await
            .expect("Exec failed");
    }

    // #[rstest]
    // #[tokio::test]
    // #[test_log::test]
    // async fn export_import(#[future] containerd_client: DockerClient) {
    //     let containerd_client = containerd_client.await;
    //     let image = containerd_client
    //         .pull_image(TEST_IMAGE)
    //         .await
    //         .expect("Failed to create image");
    //     let image = containerd_client
    //         .exec(&image, "touch hello.txt".to_owned())
    //         .await
    //         .expect("Failed to create file");
    //
    //     let mut data = tokio::io::join;
    //     containerd_client
    //         .export(std::iter::once(&image), &mut data)
    //         .await
    //         .expect("Failed to export");
    //     containerd_client.delete_image(&image).await;
    //     containerd_client
    //         .import(std::io::Cursor::new(data))
    //         .await
    //         .expect("Failed to import");
    //
    //     containerd_client
    //         .exec(&image, "cat hello.txt".to_owned())
    //         .await
    //         .expect("Failed to find file");
    //
    // }
}
