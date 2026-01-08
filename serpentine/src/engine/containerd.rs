//! Wrapper around containerd API client and other container related operations

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::PathBuf;
use std::rc::Rc;

use containerd_client::services::v1 as containerd_services;
use containerd_client::tonic::{IntoRequest, Request};
use futures_util::{StreamExt, TryStreamExt};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::engine::cache::{CacheData, CacheReader, CacheWriter, ExternalCache};
use crate::engine::data_model::FileSystem;
use crate::engine::{RuntimeError, sidecar_client};
use crate::tui::TuiSender;

/// The snapshoter to use for containers.
const SNAPSHOTTER: &str = "overlayfs";

/// Connfiguration for the container
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub struct ContainerConfig {
    /// Environment
    env: HashMap<Rc<str>, Rc<str>>,
    /// The working directory
    working_dir: PathBuf,
}

impl Hash for ContainerConfig {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u8(0);
    }
}

impl From<oci_client::config::Config> for ContainerConfig {
    fn from(config: oci_client::config::Config) -> Self {
        Self {
            env: config
                .env
                .unwrap_or_default()
                .into_iter()
                .filter_map(|env| {
                    env.split_once('=')
                        .map(|(key, value)| (Rc::from(key), Rc::from(value)))
                })
                .collect(),
            working_dir: config.working_dir.unwrap_or("/".to_owned()).into(),
        }
    }
}

impl CacheData for ContainerConfig {
    async fn write(
        &self,
        writer: &mut CacheWriter<impl AsyncWrite + Unpin + Send>,
    ) -> Result<(), RuntimeError> {
        writer
            .write_u64_variable_length(self.env.len() as u64)
            .await?;
        for (key, value) in &self.env {
            key.write(writer).await?;
            value.write(writer).await?;
        }

        let working_dir = self.working_dir.as_os_str().to_string_lossy();
        writer
            .write_u64_variable_length(working_dir.len() as u64)
            .await?;
        writer.write_all(working_dir.as_bytes()).await?;

        Ok(())
    }

    async fn read(
        reader: &mut CacheReader<impl AsyncRead + Unpin + Send>,
    ) -> Result<Self, RuntimeError> {
        let mut env = HashMap::new();
        let items = reader.read_u64_length_encoded().await?;
        for _ in 0..items {
            let key = Rc::<str>::read(reader).await?;
            let value = Rc::<str>::read(reader).await?;
            env.insert(key, value);
        }

        let length = reader
            .read_u64_length_encoded()
            .await?
            .try_into()
            .map_err(|_| RuntimeError::internal("Path length overflows usize for platform"))?;

        let mut working_dir = vec![0; length];
        reader.read_exact(&mut working_dir).await?;
        let working_dir =
            String::from_utf8(working_dir).map_err(|_| RuntimeError::internal("non-utf8 path"))?;
        let working_dir = PathBuf::from(working_dir);

        Ok(Self { env, working_dir })
    }

    fn content_hash(&self, hasher: &mut blake3::Hasher) {
        for (key, value) in &self.env {
            key.content_hash(hasher);
            value.content_hash(hasher);
        }

        let working_dir = self.working_dir.as_os_str().to_string_lossy();
        hasher.update(&(working_dir.len() as u64).to_le_bytes());
        hasher.update(working_dir.as_bytes());
    }
}

/// A reference to a specific state of a container.
#[derive(Clone, Hash, Eq, PartialEq, Debug)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub struct ContainerState {
    /// The snapshot to use for the container
    snapshot: Rc<str>,
    /// The container config
    config: Rc<ContainerConfig>,
}

impl CacheData for ContainerState {
    async fn write(
        &self,
        writer: &mut CacheWriter<impl AsyncWrite + Unpin + Send>,
    ) -> Result<(), RuntimeError> {
        self.snapshot.write(writer).await?;
        self.config.write(writer).await?;
        Ok(())
    }

    async fn read(
        reader: &mut CacheReader<impl AsyncRead + Unpin + Send>,
    ) -> Result<Self, RuntimeError> {
        let snapshot = Rc::<str>::read(reader).await?;
        let config = Rc::<ContainerConfig>::read(reader).await?;
        Ok(Self { snapshot, config })
    }

    fn content_hash(&self, hasher: &mut blake3::Hasher) {
        self.snapshot.content_hash(hasher);

        hasher.update(&(self.config.env.len() as u64).to_le_bytes());
        self.config.content_hash(hasher);
    }
}

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
    sub_client_wrapper!(leases, leases_client::LeasesClient);
}

/// Extension trait for easially attaching a lease to requests
trait WithLease<T>: IntoRequest<T> {
    /// Attach a lease to this request
    fn with_lease(self, lease: &str) -> Request<T>;
}

impl<S, T> WithLease<T> for S
where
    S: IntoRequest<T>,
{
    #[expect(clippy::expect_used, reason = "constant value")]
    fn with_lease(self, lease: &str) -> Request<T> {
        let mut this = self.into_request();
        this.metadata_mut().insert(
            "containerd-lease",
            lease.parse().expect("Invalid metadta value"),
        );
        this
    }
}

/// A resource that might be left hanging on operation abort, should be cleared out at shutdown
#[derive(PartialEq, Eq, Hash)]
enum DanglingResource {
    /// A lease, this dangling would lead to gc holding onto uneeded data
    Lease(Box<str>),
    /// A task, this danlging would leave processes running that arent useful anymore.
    /// This holds the container id
    Task(Box<str>),
}

/// A docker client wrapper
pub struct Client {
    /// containerd client
    containerd: ContainerdRootClient,
    /// Client to the sidecar
    sidecar: sidecar_client::Client,
    /// Container registery client
    oci: oci_client::Client,
    /// Sender to the TUI
    tui: TuiSender,
    /// Limiter on the amount of exec jobs running at once
    exec_lock: tokio::sync::Semaphore,
    /// Dangling resources
    dangling: Mutex<HashSet<DanglingResource>>,
}

impl Client {
    /// Create a new containerd client
    pub async fn new(tui: TuiSender, exec_permits: usize) -> Result<Self, RuntimeError> {
        let oci = oci_client::Client::new(oci_client::client::ClientConfig {
            user_agent: concat!("serpentine/", env!("CARGO_PKG_VERSION")),
            platform_resolver: Some(Box::new(platform_resolver)),
            ..Default::default()
        });

        let sidecar = crate::engine::docker::connect().await?;
        let containerd =
            containerd_client::tonic::transport::Endpoint::from_static("http://[::]:0")
                .connect_with_connector(tower::service_fn(move |_| async move {
                    sidecar
                        .containerd()
                        .await
                        .map_err(std::io::Error::other)
                        .map(hyper_util::rt::TokioIo::new)
                }))
                .await?;
        let containerd = containerd_client::Client::from(containerd);

        Ok(Self {
            sidecar,
            containerd: ContainerdRootClient(containerd),
            oci,
            tui,
            exec_lock: tokio::sync::Semaphore::new(exec_permits),
            dangling: Mutex::new(HashSet::new()),
        })
    }

    /// Check if a state exists
    pub async fn healthcheck(&self, snapshot: &ContainerState) -> bool {
        self.containerd
            .snapshot()
            .stat(containerd_services::snapshots::StatSnapshotRequest {
                snapshotter: SNAPSHOTTER.to_owned(),
                key: (*snapshot.snapshot).to_owned(),
            })
            .await
            .is_ok()
    }

    /// Create a new lease
    async fn new_lease(&self) -> Result<String, RuntimeError> {
        let lease = uuid::Uuid::new_v4().to_string();
        self.dangling
            .lock()
            .await
            .insert(DanglingResource::Lease(lease.clone().into()));

        self.containerd
            .leases()
            .create(containerd_services::CreateRequest {
                id: lease.clone(),
                labels: HashMap::new(),
            })
            .await?;
        Ok(lease)
    }

    /// Drop the given lease, freeing up any not referenced elsewhere.
    async fn drop_lease(&self, lease: String) -> Result<(), RuntimeError> {
        self.containerd
            .leases()
            .delete(containerd_services::DeleteRequest {
                id: lease.clone(),
                sync: false,
            })
            .await?;
        self.dangling
            .lock()
            .await
            .remove(&DanglingResource::Lease(lease.into()));
        Ok(())
    }

    /// download the given image if necessary
    pub async fn pull_image(&self, image: &str) -> Result<ContainerState, RuntimeError> {
        let image = oci_client::Reference::try_from(image)?;
        let auth = oci_client::secrets::RegistryAuth::Anonymous;

        log::debug!("Pulling image {image} manifest");
        let (manifest, _, config) = self.oci.pull_manifest_and_config(&image, &auth).await?;

        let last_layer_digest = manifest
            .layers
            .last()
            .map(|layer| layer.digest.clone())
            .unwrap_or_default();
        let image_exists = self
            .containerd
            .snapshot()
            .stat(containerd_services::snapshots::StatSnapshotRequest {
                snapshotter: SNAPSHOTTER.to_owned(),
                key: last_layer_digest.clone(),
            })
            .await
            .is_ok();

        if image_exists {
            log::debug!("image {image} already exists");
        } else {
            let lease = self.new_lease().await?;

            log::debug!("Pulling image {image}");
            futures_util::future::try_join_all(
                manifest
                    .layers
                    .iter()
                    .map(|layer| self.pull_layer(&image, layer, &lease)),
            )
            .await?;

            log::debug!("Uploaded all layers to containerd");

            log::debug!("Creating snapshot from image");
            self.create_snapshots(manifest, &lease).await?;

            self.drop_lease(lease).await?;
        }

        let config: oci_client::config::Config =
            serde_json::from_str(&config).map_err(|err| RuntimeError::internal(err.to_string()))?;
        let config = ContainerConfig::from(config);

        Ok(ContainerState {
            snapshot: last_layer_digest.into(),
            config: Rc::new(config),
        })
    }

    /// Create layer snapshots from the manifest, this assumes the layer content is in the content
    /// store
    async fn create_snapshots(
        &self,
        manifest: oci_client::manifest::OciImageManifest,
        lease: &str,
    ) -> Result<(), RuntimeError> {
        let mut parent = String::new();
        let layer_count = manifest.layers.len();

        for (index, layer) in manifest.layers.into_iter().enumerate() {
            let layer_exists = self
                .containerd
                .snapshot()
                .stat(containerd_services::snapshots::StatSnapshotRequest {
                    snapshotter: SNAPSHOTTER.to_owned(),
                    key: layer.digest.clone(),
                })
                .await
                .is_ok();

            if layer_exists {
                log::debug!("Snapshot {} already exists.", layer.digest);
            } else {
                let key = uuid::Uuid::new_v4().to_string();
                log::debug!("Applying layer {} to {key}", layer.digest);
                let mounts = self
                    .containerd
                    .snapshot()
                    .prepare(
                        containerd_services::snapshots::PrepareSnapshotRequest {
                            key: key.clone(),
                            snapshotter: SNAPSHOTTER.to_owned(),
                            labels: HashMap::new(),
                            parent: parent.clone(),
                        }
                        .with_lease(lease),
                    )
                    .await?
                    .into_inner()
                    .mounts;

                self.containerd
                    .diff()
                    .apply(containerd_services::ApplyRequest {
                        diff: Some(containerd_client::types::Descriptor {
                            media_type: layer.media_type,
                            digest: layer.digest.clone(),
                            size: layer.size,
                            annotations: HashMap::new(),
                        }),
                        mounts: mounts.clone(),
                        payloads: HashMap::new(),
                        sync_fs: false,
                    })
                    .await?;

                log::debug!("Commiting {key} to {}", layer.digest);
                let labels = if index == layer_count.saturating_sub(1) {
                    HashMap::from([("containerd.io/gc.root".to_owned(), "1".to_owned())])
                } else {
                    HashMap::new()
                };
                self.containerd
                    .snapshot()
                    .commit(
                        containerd_services::snapshots::CommitSnapshotRequest {
                            snapshotter: SNAPSHOTTER.to_owned(),
                            name: layer.digest.clone(),
                            key,
                            labels,
                        }
                        .with_lease(lease),
                    )
                    .await?;
            }

            parent = layer.digest;
        }

        Ok(())
    }

    /// Pull the given layer into containerd.
    async fn pull_layer(
        &self,
        image: &oci_client::Reference,
        layer: &oci_client::manifest::OciDescriptor,
        lease: &str,
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
        if self
            .containerd
            .snapshot()
            .stat(containerd_services::snapshots::StatSnapshotRequest {
                snapshotter: SNAPSHOTTER.to_owned(),
                key: layer.digest.clone(),
            })
            .await
            .is_ok()
        {
            log::debug!(
                "Content for layer {} not found, but snapshot was.",
                layer.digest
            );
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
                    })
                    .with_lease(lease),
            )
            .await?
            .into_inner()
            .try_for_each(async |_| Ok(()))
            .await?;

        log::debug!("Finished pulling {digest_clone}.");
        self.containerd
            .content()
            .write(
                futures_util::stream::iter(std::iter::once(
                    containerd_services::WriteContentRequest {
                        action: containerd_services::WriteAction::Commit.into(),
                        r#ref: upload_ref_clone,
                        total: total_size,
                        expected: digest_clone,
                        offset: total_size,
                        data: Vec::new(),
                        labels: HashMap::new(),
                    },
                ))
                .with_lease(lease),
            )
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
        let lease = self.new_lease().await?;

        self.containerd
            .snapshot()
            .prepare(
                containerd_services::snapshots::PrepareSnapshotRequest {
                    snapshotter: SNAPSHOTTER.to_owned(),
                    key: snapshot.clone(),
                    parent: (*state.snapshot).to_owned(),
                    labels: HashMap::new(),
                }
                .with_lease(&lease),
            )
            .await?;

        let _ = self
            .exec_internal(
                &ContainerState {
                    snapshot: Rc::from(snapshot.clone()),
                    config: Rc::clone(&state.config),
                },
                cmd,
                &lease,
            )
            .await?;

        let new_snapshot = uuid::Uuid::new_v4().to_string();
        self.containerd
            .snapshot()
            .commit(containerd_services::snapshots::CommitSnapshotRequest {
                snapshotter: SNAPSHOTTER.to_owned(),
                name: new_snapshot.clone(),
                key: snapshot.clone(),
                labels: HashMap::from([("containerd.io/gc.root".to_owned(), "1".to_owned())]),
            })
            .await?;
        self.drop_lease(lease).await?;

        Ok(ContainerState {
            snapshot: new_snapshot.into(),
            config: Rc::clone(&state.config),
        })
    }

    /// Execute a command return its stdout and stderr.
    pub async fn exec_get_output(
        &self,
        state: &ContainerState,
        cmd: String,
    ) -> Result<String, RuntimeError> {
        let snapshot = uuid::Uuid::new_v4().to_string();
        let lease = self.new_lease().await?;

        self.containerd
            .snapshot()
            .prepare(
                containerd_services::snapshots::PrepareSnapshotRequest {
                    snapshotter: SNAPSHOTTER.to_owned(),
                    key: snapshot.clone(),
                    parent: (*state.snapshot).to_owned(),
                    labels: HashMap::new(),
                }
                .with_lease(&lease),
            )
            .await?;

        let output = self
            .exec_internal(
                &ContainerState {
                    snapshot: Rc::from(snapshot.clone()),
                    config: Rc::clone(&state.config),
                },
                cmd,
                &lease,
            )
            .await?
            .map_err(|output| RuntimeError::NonUtf8Capture { output })?;
        self.drop_lease(lease).await?;

        Ok(output)
    }

    /// Execute a command on the given mutable snapshot, returning its stdout and stderr
    /// The stdout will be wrapeed in `Ok` is all the data was utf-8, `Err` if not.
    async fn exec_internal(
        &self,
        state: &ContainerState,
        cmd: String,
        lease: &str,
    ) -> Result<Result<String, String>, RuntimeError> {
        log::debug!("Prepearing to execute {cmd:?} in {state:?}");
        let container = self.create_container(state, cmd.clone(), lease).await?;

        log::debug!("Loading mounts for {:?}", state.snapshot);
        let mounts = self
            .containerd
            .snapshot()
            .mounts(containerd_services::snapshots::MountsRequest {
                snapshotter: SNAPSHOTTER.to_owned(),
                key: (*state.snapshot).to_owned(),
            })
            .await?
            .into_inner()
            .mounts;

        let (stdout_path, stdout) = self.sidecar.fifo_pipe().await?;
        let log_id = cmd.clone();
        let stdout = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(Self::read_stdout(
            stdout, log_id,
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
                    stdout: stdout_path.clone(),
                    stderr: stdout_path.clone(),
                    checkpoint: None,
                    options: None,
                    runtime_path: String::new(),
                }
                .with_lease(lease),
            )
            .await?
            .into_inner();

        let exec_lock = self.exec_lock.acquire().await;

        log::debug!("Starting {cmd:?} in {container}");
        // A empty `exec_id` signifies the main process of a container
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

        drop(exec_lock);
        self.dangling
            .lock()
            .await
            .remove(&DanglingResource::Task(container.into()));

        let stdout = stdout
            .await
            .map_err(|_| RuntimeError::internal("Failed to join task"))?;

        log::debug!("Got exit code {exit_code}");
        if exit_code == 0 {
            Ok(stdout)
        } else {
            Err(RuntimeError::CommandExecution {
                code: exit_code.into(),
                command: cmd,
                output: stdout.unwrap_or_else(|data| data),
            })
        }
    }

    /// Create a container according to the given container state and the given command and returns
    /// its id
    async fn create_container(
        &self,
        state: &ContainerState,
        cmd: String,
        lease: &str,
    ) -> Result<String, RuntimeError> {
        let container = uuid::Uuid::new_v4().to_string();
        self.dangling
            .lock()
            .await
            .insert(DanglingResource::Task(container.clone().into()));

        let mut root = oci_spec::runtime::Root::default();
        root.set_path(PathBuf::from("rootfs"));
        root.set_readonly(Some(false));

        let mut process = oci_spec::runtime::Process::default();
        process.set_args(Some(vec!["/bin/sh".to_owned(), "-c".to_owned(), cmd]));
        process.set_env(Some(
            state
                .config
                .env
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect(),
        ));
        process.set_cwd(state.config.working_dir.clone());

        let network_namespace = self.sidecar.create_network_namespace().await?;
        let mut linux = oci_spec::runtime::Linux::default();
        if let Some(namespaces) = linux.namespaces_mut()
            && let Some(namespace) = namespaces
                .iter_mut()
                .find(|namespace| namespace.typ() == oci_spec::runtime::LinuxNamespaceType::Network)
        {
            namespace.set_path(Some(network_namespace.into()));
        }

        let mut spec = oci_spec::runtime::Spec::default();
        spec.set_root(Some(root))
            .set_process(Some(process))
            .set_linux(Some(linux));

        if let Ok(json) = serde_json::to_string_pretty(&spec) {
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
                                .map_err(|err| RuntimeError::internal(format!("{err}")))?,
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
            .await?;

        Ok(container)
    }

    /// Read the stdout to a String, returns `Err` if encountered non-utf (containg the output
    /// without those lines), and `Ok` if all data was utf-8
    async fn read_stdout(
        stdout: impl AsyncRead + Unpin + Send + 'static,
        log_id: String,
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
    #[expect(
        clippy::unused_self,
        reason = "This struct is the api surface for this operation"
    )]
    pub fn set_working_dir(&self, state: &ContainerState, dir: &str) -> ContainerState {
        let mut config = (*state.config).clone();
        config.working_dir = config.working_dir.join(dir);
        ContainerState {
            snapshot: Rc::clone(&state.snapshot),
            config: Rc::new(config),
        }
    }

    /// Set a environment variable in the container
    #[expect(
        clippy::unused_self,
        reason = "This struct is the api surface for this operation"
    )]
    pub fn set_env_var(
        &self,
        state: &ContainerState,
        env: Rc<str>,
        value: Rc<str>,
    ) -> ContainerState {
        let mut config = (*state.config).clone();
        config.env.insert(env, value);
        ContainerState {
            snapshot: Rc::clone(&state.snapshot),
            config: Rc::new(config),
        }
    }

    /// Get a environment variable in the container
    #[expect(
        clippy::unused_self,
        reason = "This struct is the api surface for this operation"
    )]
    pub fn get_env_var<'state>(
        &self,
        state: &'state ContainerState,
        env: &str,
    ) -> Option<&'state Rc<str>> {
        state.config.env.get(env)
    }

    /// Shutdown any dangling references
    pub async fn shutdown(self) {
        for dangling in &*self.dangling.lock().await {
            match dangling {
                DanglingResource::Lease(lease) => {
                    let _ = self
                        .containerd
                        .leases()
                        .delete(containerd_services::DeleteRequest {
                            id: lease.to_string(),
                            sync: false,
                        })
                        .await;
                }
                DanglingResource::Task(container) => {
                    let _ = self
                        .containerd
                        .tasks()
                        .kill(containerd_services::KillRequest {
                            container_id: container.to_string(),
                            exec_id: String::new(),
                            signal: 9, // kill
                            all: true,
                        })
                        .await;
                }
            }
        }
    }
}

impl ExternalCache for Client {
    async fn cleanup(&self, data: super::data_model::Data) {
        todo!()
    }

    async fn export(
        &self,
        values: impl IntoIterator<Item = &super::data_model::Data>,
        file: &mut (impl AsyncWrite + Unpin + Send),
    ) -> Result<(), RuntimeError> {
        todo!()
    }

    async fn import(&self, file: &mut (impl AsyncRead + Send + Unpin)) -> Result<(), RuntimeError> {
        todo!()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let dangling_count = self.dangling.get_mut().len();
        if dangling_count != 0 {
            log::warn!("Leaving {dangling_count} dangling resources running in contained.")
        }
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
    async fn exec_non_utf8(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let image = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        containerd_client
            .exec(&image, r"printf '\xff\xfe\xfa'".to_owned())
            .await
            .expect(
                "Exec failed on non-utf8 stdout, even tho we werent explictly capturing it here. ",
            );
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn exec_output_non_utf8(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let image = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let output = containerd_client
            .exec_get_output(&image, r"printf '\xff\xfe\xfa'".to_owned())
            .await;

        assert!(
            output.is_err(),
            "No way to represent the non-utf8 data, so should be a error"
        );
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
        let base = containerd_client.set_working_dir(&base, "/testing");

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
        let image = containerd_client.set_working_dir(&image, "/foo");
        containerd_client
            .exec(&image, "ls bar".to_owned())
            .await
            .expect("Exec failed");

        let image = containerd_client.set_working_dir(&image, "./bar");
        let working_dir_pwd = containerd_client
            .exec_get_output(&image, "pwd".to_owned())
            .await
            .expect("Exec failed");
        assert_eq!(
            working_dir_pwd.trim(),
            "/foo/bar".to_owned(),
            "pwd reported wrong working directory"
        );

        let image = containerd_client.set_working_dir(&image, "/app");
        let working_absolute_dir_pwd = containerd_client
            .exec_get_output(&image, "pwd".to_owned())
            .await
            .expect("Exec failed");
        assert_eq!(
            working_absolute_dir_pwd.trim(),
            "/app".to_owned(),
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
        let image = containerd_client.set_env_var(&image, "HELLO".into(), "WORLD".into());
        let exec = containerd_client
            .exec_get_output(&image, "echo -n $HELLO".to_owned())
            .await
            .expect("Exec failed");
        let get_env = containerd_client
            .get_env_var(&image, "HELLO")
            .expect("Env var not found");

        assert_eq!(exec, "WORLD", "echo $HELLO");
        assert_eq!(get_env.as_ref(), "WORLD", "get_env");
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn network_access(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let image = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        containerd_client
            .exec(&image, "curl 1.1.1.1".to_owned())
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
