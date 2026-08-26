//! Wrapper around containerd API client and other container related operations

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use containerd_client::services::v1 as containerd_services;
use containerd_client::tonic::{IntoRequest, Request};
use futures_util::{StreamExt, TryStreamExt};
use serpentine_internal::{FileSystemEntryHeader, network};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use typed_path::{UnixPath, UnixPathBuf};

use crate::engine::cache::{CacheBackend, CacheHash, CacheScope};
// use crate::engine::cache::ExternalCache;
use crate::engine::filesystem::{FileSystem, FileSystemProvider};
use crate::engine::{BoxedReader, RuntimeError, acquire_file_lock, sidecar_client, userdb};
use crate::events::{Lifecycle, Reporter, TaskHandle, TaskId, TaskKind};

/// The snapshotter to use for containers.
// WARN: While most of the code is snapshotter agnostic, the sidecar export layer implementation uses
// overlayfs specific knowledge to efficiently produce filesystem diffs.
// If we change snapshotter in the future that code will need to be updated.
const SNAPSHOTTER: &str = "overlayfs";

/// Field generators for fuzzing the container config types.
#[cfg(test)]
mod fuzz {
    use bolero::ValueGenerator as _;

    use super::*;
    pub(super) use crate::engine::data_model::fuzz::arc_str;

    /// Generator for an optional user string.
    pub(super) fn opt_arc_str() -> impl bolero::ValueGenerator<Output = Option<Arc<str>>> {
        bolero::produce::<Option<String>>().map_gen(|value| value.map(Arc::from))
    }

    /// Generator for small environment maps.
    pub(super) fn env_map() -> impl bolero::ValueGenerator<Output = BTreeMap<Arc<str>, Arc<str>>> {
        bolero::produce::<Vec<(String, String)>>()
            .with()
            .len(0..10_usize)
            .map_gen(|entries| {
                entries
                    .into_iter()
                    .map(|(key, value)| (Arc::from(key), Arc::from(value)))
                    .collect()
            })
    }

    /// Generator for arbitrary unix paths.
    pub(super) fn unix_path() -> impl bolero::ValueGenerator<Output = UnixPathBuf> {
        bolero::produce::<Vec<u8>>().map_gen(|bytes| UnixPath::new(&bytes).to_path_buf())
    }

    /// Generator for healthchecks, using whole seconds so the cache roundtrip is lossless.
    pub(super) fn healthcheck() -> impl bolero::ValueGenerator<Output = (Arc<str>, Duration)> {
        (
            arc_str(),
            bolero::produce::<u64>().map_gen(Duration::from_secs),
        )
    }
}

/// Configuration for the container
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(test, derive(bolero::TypeGenerator))]
pub struct ContainerConfig {
    /// Environment
    #[cfg_attr(test, generator(fuzz::env_map()))]
    env: BTreeMap<Arc<str>, Arc<str>>,
    /// The working directory
    #[cfg_attr(test, generator(fuzz::unix_path()))]
    #[serde(with = "serpentine_internal::TypedPathBufRemote")]
    working_dir: UnixPathBuf,
    /// The user to spawn the process as.
    ///
    /// This is stored in the same format as the oci image spec for linux.
    /// > `user`, `uid`, `user:group`, `uid:gid`, `uid:group`, `user:gid`
    #[cfg_attr(test, generator(fuzz::opt_arc_str()))]
    user: Option<Arc<str>>,
    /// The services to attach to this container.
    // TODO: Should this generate services?
    #[cfg_attr(test, generator(bolero::constant(BTreeMap::new())))]
    services: BTreeMap<Arc<str>, ServiceState>,
}

impl ContainerConfig {
    /// Get an environment variable in the container
    pub fn get_env_var(&self, env: &str) -> Option<&Arc<str>> {
        self.env.get(env)
    }

    /// Set the working directory of the container
    pub fn set_working_dir(&mut self, dir: &UnixPath) {
        self.working_dir = self.working_dir.join(dir);
    }

    /// Set an environment variable in the container
    pub fn set_env_var(&mut self, env: Arc<str>, value: Arc<str>) {
        self.env.insert(env, value);
    }

    /// Update the user config for the container
    pub fn set_user(&mut self, user: Arc<str>) {
        self.user = Some(user);
    }

    /// Attach service to this container
    pub fn with_service(&mut self, service: ServiceState, hostname: Arc<str>) {
        self.services.insert(hostname, service);
    }
}

impl From<oci_client::config::Config> for ContainerConfig {
    fn from(config: oci_client::config::Config) -> Self {
        let env = config
            .env
            .unwrap_or_default()
            .into_iter()
            .filter_map(|env| {
                env.split_once('=')
                    .map(|(key, value)| (Arc::from(key), Arc::from(value)))
            })
            .collect();

        Self {
            env,
            working_dir: config.working_dir.map_or_else(
                || UnixPath::new("/").to_path_buf(),
                |dir| UnixPath::new(&dir).to_path_buf(),
            ),
            user: config.user.map(Arc::from),
            services: BTreeMap::new(),
        }
    }
}

/// Extra config values for services
#[derive(Clone, Eq, PartialEq, Debug, Hash, serde::Serialize, serde::Deserialize)]
#[cfg_attr(test, derive(bolero::TypeGenerator))]
pub struct ServiceConfig {
    /// The service entry point
    #[cfg_attr(test, generator(fuzz::arc_str()))]
    entrypoint: Arc<str>,
    /// Command to run in the same container as the service and which should return a 0 exit code
    /// before spawning parents.
    #[cfg_attr(test, generator(fuzz::healthcheck()))]
    healthcheck: (Arc<str>, Duration),
}

impl From<oci_client::config::Config> for ServiceConfig {
    fn from(config: oci_client::config::Config) -> Self {
        let entrypoint = config
            .entrypoint
            .unwrap_or_default()
            .into_iter()
            .chain(config.cmd.unwrap_or_default())
            .collect::<Vec<_>>();
        let entrypoint = shell_words::join(entrypoint);

        Self {
            entrypoint: format!("exec {entrypoint}").into(),
            healthcheck: ("exit 0".into(), Duration::from_secs(1)),
        }
    }
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            entrypoint: "while true; do sleep 1; done".into(),
            healthcheck: ("exit 0".into(), Duration::from_secs(1)),
        }
    }
}

impl ServiceConfig {
    /// Set the healthcheck command and timeout for this service.
    pub fn set_healthcheck(&mut self, command: Arc<str>, timeout: Duration) {
        self.healthcheck = (command, timeout);
    }
}

/// A services state
#[derive(Clone, Eq, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
#[cfg_attr(test, derive(bolero::TypeGenerator))]
pub struct ServiceState {
    /// The underlying container
    container: ContainerState,
    /// The service-specific config.
    service_config: ServiceConfig,
}

impl ServiceState {
    /// Get a reference to the service config.
    pub fn get_service_config(&self) -> &ServiceConfig {
        &self.service_config
    }

    /// Update this states service config using a closure.
    ///
    /// This does not change the input state but instead returns a new one.
    pub fn update_service_config(&self, update: impl FnOnce(&mut ServiceConfig)) -> Self {
        let mut service_config = self.service_config.clone();
        update(&mut service_config);
        ServiceState {
            container: self.container.clone(),
            service_config,
        }
    }

    /// Convert this service into a container topology
    fn into_topology(mut self, hostname: Arc<str>) -> network::Topology<ContainerTopologyNode> {
        let mut services = Vec::with_capacity(self.container.config.services.len());

        self.container = self.container.update_config(|config| {
            for (service_hostname, service) in &config.services {
                services.push(service.clone().into_topology(Arc::clone(service_hostname)));
            }
        });

        let this = ContainerTopologyNode {
            state: ContainerLike::Service(self),
            hostname: Some(hostname),
            cmd: None,
        };

        network::Topology::with_children(this, services)
    }
}

impl std::ops::Deref for ServiceState {
    type Target = ContainerState;
    fn deref(&self) -> &ContainerState {
        &self.container
    }
}

impl std::ops::DerefMut for ServiceState {
    fn deref_mut(&mut self) -> &mut ContainerState {
        &mut self.container
    }
}

/// A node in a container topology.
///
/// The root node will be a `ContainerState` and the children will be `ServiceState`s.
/// This is used to represent the full state of a container with all its attached services.
struct ContainerTopologyNode {
    /// The service state
    state: ContainerLike,
    /// The hostname of this service.
    hostname: Option<Arc<str>>,
    /// The command provided to the root node.
    cmd: Option<Box<str>>,
}

impl ContainerTopologyNode {
    /// Get the hostname to use for this container.
    fn get_hostname(&self) -> Arc<str> {
        self.hostname.clone().unwrap_or_else(|| "step".into())
    }

    /// Get the command to execute
    fn get_cmd(&self) -> &str {
        match (&self.state, &self.cmd) {
            (ContainerLike::Container(_), Some(cmd)) => cmd.as_ref(),
            (ContainerLike::Service(service), None) => &service.service_config.entrypoint,
            _ => {
                debug_assert!(
                    false,
                    "Only the root node should have a cmd, and it should be set"
                );
                log::error!("Invalid container topology: root node has no cmd");
                "/bin/sh"
            }
        }
    }
}

/// A reference to a specific state of a container.
#[derive(Clone, Eq, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
#[cfg_attr(test, derive(bolero::TypeGenerator))]
pub struct ContainerState {
    /// The snapshot to use for the container
    #[cfg_attr(test, generator(fuzz::arc_str()))]
    snapshot: Arc<str>,
    /// The container config
    config: ContainerConfig,
}

impl ContainerState {
    /// Get a reference to the config.
    pub fn get_config(&self) -> &ContainerConfig {
        &self.config
    }

    /// Collect the snapshot keys referenced by this container, including its attached services.
    pub(crate) fn collect_snapshots(&self, out: &mut Vec<Arc<str>>) {
        out.push(Arc::clone(&self.snapshot));
        for service in self.config.services.values() {
            service.collect_snapshots(out);
        }
    }

    /// Update this states config using a closure.
    ///
    /// This does not change the input state but instead returns a new one.
    pub fn update_config(&self, update: impl FnOnce(&mut ContainerConfig)) -> Self {
        let mut config = self.config.clone();
        update(&mut config);
        ContainerState {
            snapshot: Arc::clone(&self.snapshot),
            config,
        }
    }

    /// Convert this container into a service
    pub fn into_service(self, entrypoint: Arc<str>) -> ServiceState {
        ServiceState {
            container: self,
            service_config: ServiceConfig {
                entrypoint,
                ..ServiceConfig::default()
            },
        }
    }

    /// Convert this into a container topology
    fn into_topology(mut self, cmd: Box<str>) -> network::Topology<ContainerTopologyNode> {
        let mut services = Vec::with_capacity(self.config.services.len());

        self = self.update_config(|config| {
            for (service_hostname, service) in &config.services {
                services.push(service.clone().into_topology(Arc::clone(service_hostname)));
            }
        });

        let this = ContainerTopologyNode {
            state: ContainerLike::Container(self),
            hostname: None,
            cmd: Some(cmd),
        };

        network::Topology::with_children(this, services)
    }
}

#[cfg(test)]
impl ContainerState {
    /// Build a state from exact field values, for tests that need a fixed constant rather than a
    /// generated one.
    pub(crate) fn from_parts(snapshot: Arc<str>, config: ContainerConfig) -> Self {
        Self { snapshot, config }
    }
}

/// Either a container or a service.
///
/// Many operations (env, working dir, user, etc.) apply equally to both containers and services.
/// This type allows those operations to be written once and work on either.
#[derive(Clone, Eq, PartialEq, Debug)]
pub enum ContainerLike {
    /// A container
    Container(ContainerState),
    /// A service
    Service(ServiceState),
}

impl ContainerLike {
    /// Update this states config using a closure.
    ///
    /// This does not change the input state but instead returns a new one.
    pub fn update_config(&self, update: impl FnOnce(&mut ContainerConfig)) -> Self {
        match self {
            Self::Container(container) => Self::Container(container.update_config(update)),
            Self::Service(service) => Self::Service(ServiceState {
                container: service.container.update_config(update),
                service_config: service.service_config.clone(),
            }),
        }
    }
}

impl std::ops::Deref for ContainerLike {
    type Target = ContainerState;
    fn deref(&self) -> &ContainerState {
        match self {
            Self::Container(container) => container,
            Self::Service(service) => service,
        }
    }
}

impl std::ops::DerefMut for ContainerLike {
    fn deref_mut(&mut self) -> &mut ContainerState {
        match self {
            Self::Container(container) => container,
            Self::Service(service) => &mut *service,
        }
    }
}

/// Return whether the given Oci platform object is compatible with the current system.
fn platform_resolver(manifests: &[oci_client::manifest::ImageIndexEntry]) -> Option<String> {
    manifests
        .iter()
        .find(|manifest| match &manifest.platform {
            None => false,
            Some(platform) => {
                platform.os == oci_client::config::Os::Linux
                    && platform.architecture == oci_client::config::Architecture::default()
            }
        })
        .map(|manifest| manifest.digest.clone())
}

/// Thin wrapper around `containerd_client::Client` to apply namespace interceptor
struct ContainerdRootClient {
    /// The underlying containerd client
    client: containerd_client::Client,
    /// The containerd namespace all requests through this client are scoped to
    namespace: String,
}

/// Build an interceptor that injects the given namespace into all requests
fn inject_namespace(
    namespace: String,
) -> impl containerd_client::tonic::service::Interceptor + Clone {
    move |mut request: containerd_client::tonic::Request<()>| {
        request.metadata_mut().insert(
            "containerd-namespace",
            namespace.parse().map_err(|_err| {
                containerd_client::tonic::Status::invalid_argument("Invalid namespace")
            })?,
        );
        Ok(request)
    }
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
            containerd_services::$($type)::+::with_interceptor(
                self.client.channel(),
                inject_namespace(self.namespace.clone()),
            )
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

/// Extension trait for easily attaching a lease to requests
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
            lease.parse().expect("Invalid metadata value"),
        );
        this
    }
}

/// A resource that might be left hanging on operation abort, should be cleared out at shutdown
#[derive(PartialEq, Eq)]
enum DanglingResource {
    /// A lease, this dangling would lead to gc holding onto unneeded data
    Lease(Box<str>),
    /// A task, this dangling would leave processes running that arent useful anymore.
    /// This holds the container id
    Task(Box<str>),
    /// A container network
    Network(network::ConcreteTopology),
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
    node: ContainerTopologyNode,
    /// Tracks this container's output as a task; finished when the handle is dropped.
    exec_task: TaskHandle,
}

/// A docker client wrapper
pub struct Client {
    /// Containerd client
    containerd: ContainerdRootClient,
    /// Client to the sidecar
    sidecar: sidecar_client::Client,
    /// Container registry client
    oci: oci_client::Client,
    /// Channel run events are reported through
    reporter: Reporter,
    /// Caching backend for storing snapshots in
    cache: Arc<dyn CacheBackend + Send + Sync>,
    /// Limiter on the amount of exec jobs running at once
    exec_lock: tokio::sync::Semaphore,
    /// Should snapshots be exported to the serpentine cache as well.
    ///
    /// This is required for portabiltiy.
    export_snapshots: bool,
    /// Dangling resources
    dangling: Mutex<Vec<DanglingResource>>,
    /// Networks that arent currently in use.
    free_networks: Mutex<HashMap<network::AbstractTopology, Vec<network::ConcreteTopology>>>,
}

impl Client {
    /// Poll the containerd version endpoint until it responds.
    ///
    /// Right after the sidecar container starts, the proxy is reachable before containerd inside
    /// the container has finished initializing its unix socket. Any gRPC call in that window fails
    /// with a "broken pipe" transport error. Wait for a successful round-trip before returning.
    async fn wait_for_containerd_ready(
        client: &containerd_client::Client,
    ) -> Result<(), RuntimeError> {
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(30);
        loop {
            match client.version().version(()).await {
                Ok(_) => return Ok(()),
                Err(err) => {
                    if start.elapsed() >= timeout {
                        return Err(err.into());
                    }
                    log::debug!("containerd not ready yet: {err}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Create a new containerd client
    ///
    /// `namespace` scopes every containerd request this client makes (snapshots, content,
    /// images, leases, ...) so that clients using different namespaces never observe each
    /// other's state, even when talking to the same containerd daemon.
    pub async fn new(
        reporter: Reporter,
        cache: Arc<dyn CacheBackend + Send + Sync>,
        exec_permits: usize,
        namespace: impl Into<String>,
        export_snapshots: bool,
    ) -> Result<Self, RuntimeError> {
        let oci = oci_client::Client::new(oci_client::client::ClientConfig {
            user_agent: concat!("serpentine/", env!("CARGO_PKG_VERSION")),
            platform_resolver: Some(Box::new(platform_resolver)),
            ..Default::default()
        });

        let (runtime, sidecar) = crate::engine::docker::connect(&reporter).await?;
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

        Self::wait_for_containerd_ready(&containerd).await?;
        reporter.lifecycle(Lifecycle::EngineReady {
            runtime: runtime.into(),
            image_tag: crate::engine::docker::CONTAINERD_IMAGE_TAG.into(),
        });

        Ok(Self {
            sidecar,
            containerd: ContainerdRootClient {
                client: containerd,
                namespace: namespace.into(),
            },
            oci,
            reporter,
            cache,
            export_snapshots,
            exec_lock: tokio::sync::Semaphore::new(exec_permits),
            dangling: Mutex::new(Vec::new()),
            free_networks: Mutex::new(HashMap::new()),
        })
    }

    /// Export the given snapshot to the caching backend
    // TODO: This is slow and blocks further execution, which it really shouldnt. its hard to clone
    // the self from here tho...
    async fn export_snapshot(&self, snapshot: &str) -> Result<(), RuntimeError> {
        if !self.export_snapshots {
            return Ok(());
        }

        log::debug!("Exporting snapshot {snapshot} to cache");

        let hash = CacheHash::from_data(CacheScope::Snapshot, snapshot).await?;
        let Some(mut writer) = self.cache.write_key(hash).await else {
            log::debug!("Snapshot {snapshot} already exists in cache");
            return Ok(());
        };

        let task = self.reporter.start_task(TaskKind::Exec, "exporting layer");

        let view_name = format!("{snapshot}/view/{}", uuid::Uuid::new_v4());
        let lease = self.new_lease().await?;

        let mounts = self
            .containerd
            .snapshot()
            .view(
                containerd_services::snapshots::ViewSnapshotRequest {
                    snapshotter: SNAPSHOTTER.into(),
                    key: view_name,
                    parent: snapshot.into(),
                    labels: HashMap::new(),
                }
                .with_lease(&lease),
            )
            .await?
            .into_inner()
            .mounts;

        debug_assert!(
            mounts.len() == 1,
            "Expected overlayfs mounts to only have one mount returned"
        );
        let mount = mounts
            .into_iter()
            .next()
            .ok_or_else(|| RuntimeError::internal("No mounts returned for snapshoter"))?;

        let parent = self
            .containerd
            .snapshot()
            .stat(containerd_services::snapshots::StatSnapshotRequest {
                snapshotter: SNAPSHOTTER.into(),
                key: snapshot.into(),
            })
            .await?
            .into_inner()
            .info
            .ok_or_else(|| RuntimeError::internal("snapshot didnt have any info"))?
            .parent;

        serpentine_internal::write_postcard_frame(&parent, &mut writer).await?;
        let mut tar_stream = self.sidecar.export_layer(mount).await?;
        tokio::io::copy(&mut tar_stream, &mut writer).await?;

        log::debug!("Finished exporting layer");
        self.drop_lease(lease).await?;
        drop(task);

        if !parent.is_empty() {
            Box::pin(self.export_snapshot(&parent)).await?;
        }

        Ok(())
    }

    /// Attempt to load the given snapshot from the cache backend.
    ///
    /// returns whether the snapshot was imported
    async fn import_layer(&self, snapshot: &str) -> Result<bool, RuntimeError> {
        log::debug!("Attempting to import {snapshot}");

        let hash = CacheHash::from_data(CacheScope::Snapshot, snapshot).await?;
        let Some(mut reader) = self.cache.read_key(hash).await else {
            log::debug!("Snapshot {snapshot} not in cache backend");
            return Ok(false);
        };

        let parent: String = serpentine_internal::read_postcard_frame(&mut reader).await?;

        if !parent.is_empty() {
            let was_found = Box::pin(self.import_layer(&parent)).await?;
            if !was_found {
                return Ok(false);
            }
        }

        let lease = self.new_lease().await?;

        let task = self.reporter.start_task(TaskKind::Exec, "importing layer");
        log::debug!("Importing {snapshot} into content store");
        let (total_size, digest) = self
            .import_reader_into_content_store(reader, &lease)
            .await?;

        let temp_snapshot = uuid::Uuid::new_v4().to_string();
        log::debug!("Creating temporary snapshot {temp_snapshot} from {parent}");
        let mounts = self
            .containerd
            .snapshot()
            .prepare(
                containerd_services::snapshots::PrepareSnapshotRequest {
                    snapshotter: SNAPSHOTTER.into(),
                    key: temp_snapshot.clone(),
                    parent,
                    labels: HashMap::new(),
                }
                .with_lease(&lease),
            )
            .await?
            .into_inner()
            .mounts;

        log::debug!("Applying layer diff {digest} to {temp_snapshot}");
        let descriptor = containerd_client::types::Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar".into(),
            digest,
            size: total_size.try_into().unwrap_or(0),
            annotations: HashMap::new(),
        };
        self.containerd
            .diff()
            .apply(containerd_services::ApplyRequest {
                mounts,
                diff: Some(descriptor),
                payloads: HashMap::new(),
                sync_fs: true,
            })
            .await?;
        log::debug!("Diff applied, committing snapshot to {snapshot}");
        self.containerd
            .snapshot()
            .commit(containerd_services::snapshots::CommitSnapshotRequest {
                snapshotter: SNAPSHOTTER.into(),
                key: temp_snapshot,
                name: snapshot.to_owned(),
                labels: HashMap::from([("containerd.io/gc.root".to_owned(), "1".to_owned())]),
            })
            .await?;

        self.drop_lease(lease).await?;
        drop(task);

        Ok(true)
    }

    /// Import data from the given reader into the content store (under the given lease.)
    ///
    /// returns the total size in bytes, as well as the digest.
    async fn import_reader_into_content_store(
        &self,
        reader: BoxedReader,
        lease: &str,
    ) -> Result<(usize, String), RuntimeError> {
        let upload_ref = uuid::Uuid::new_v4().to_string();
        let upload_ref_clone = upload_ref.clone();
        // Give it a 1MiB buffer instead of the default 4KiB because h2 has a ddos protection that
        // a lot of small writes trips.
        let reader = tokio_util::io::ReaderStream::with_capacity(reader, 1024 * 1024);
        let current_offset = Arc::new(AtomicUsize::new(0));
        let current_offset_clone = Arc::clone(&current_offset);
        self.containerd
            .content()
            .write(
                reader
                    .filter_map(async |layer_data| layer_data.ok())
                    .map(move |layer_data| {
                        let previous_offset =
                            current_offset_clone.fetch_add(layer_data.len(), Ordering::Relaxed);

                        // log::trace!("Writing {layer_data:?} at {previous_offset}");
                        containerd_services::WriteContentRequest {
                            action: containerd_services::WriteAction::Write.into(),
                            r#ref: upload_ref_clone.clone(),
                            total: 0,
                            expected: String::new(),
                            offset: previous_offset.try_into().unwrap_or(0),
                            data: layer_data.to_vec(),
                            labels: HashMap::new(),
                        }
                    })
                    .with_lease(lease),
            )
            .await?
            .into_inner()
            .try_for_each(async |_| Ok(()))
            .await?;
        let total_size = current_offset.load(Ordering::Relaxed);
        log::debug!("Committing {total_size} bytes to the store");
        let digest = self
            .containerd
            .content()
            .write(
                futures_util::stream::once(async move {
                    containerd_services::WriteContentRequest {
                        action: containerd_services::WriteAction::Commit.into(),
                        r#ref: upload_ref,
                        total: total_size.try_into().unwrap_or(0),
                        expected: String::new(),
                        offset: (total_size).try_into().unwrap_or(0),
                        data: Vec::new(),
                        labels: HashMap::new(),
                    }
                })
                .with_lease(lease),
            )
            .await?
            .into_inner()
            .try_next()
            .await?
            .ok_or_else(|| RuntimeError::internal("No response for commit"))?
            .digest;
        Ok((total_size, digest))
    }

    /// Checks if the snapshot exists in containerd and if not tries to import it from the cache.
    ///
    /// Returns whether the snapshot exists after this.
    async fn ensure_snapshot(&self, snapshot: &str) -> bool {
        log::debug!("Ensuring {snapshot} exists");

        let lock = crate::engine::acquire_file_lock(&format!("import/{snapshot}")).await;

        if self
            .containerd
            .snapshot()
            .stat(containerd_services::snapshots::StatSnapshotRequest {
                snapshotter: SNAPSHOTTER.to_owned(),
                key: snapshot.into(),
            })
            .await
            .is_ok()
        {
            log::debug!("{snapshot} exists");
            drop(lock);
            true
        } else {
            let result = self.import_layer(snapshot).await;

            drop(lock);
            match result {
                Ok(imported) => imported,
                Err(err) => {
                    log::error!("Failed to import layer: {err}");
                    debug_assert!(false, "Failed to import layer");
                    false
                }
            }
        }
    }

    /// Check if a state and all its services exist
    pub async fn healthcheck_value(&self, config: &ContainerState) -> bool {
        if !self.ensure_snapshot(&config.snapshot).await {
            return false;
        }

        for service in config.config.services.values() {
            if !Box::pin(self.healthcheck_value(service)).await {
                return false;
            }
        }

        true
    }

    /// Create a new lease
    async fn new_lease(&self) -> Result<String, RuntimeError> {
        let lease = uuid::Uuid::new_v4().to_string();
        self.dangling
            .lock()
            .await
            .push(DanglingResource::Lease(lease.clone().into()));

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
        Ok(())
    }

    /// Download the given image and return a normal `ContainerState` representing it.
    pub async fn pull_image(&self, image_name: &str) -> Result<ContainerState, RuntimeError> {
        let (config, snapshot_name) = self.fetch_image(image_name).await?;

        let config = if let Some(config) = config.config {
            ContainerConfig::from(config)
        } else {
            ContainerConfig::default()
        };

        Ok(ContainerState {
            snapshot: snapshot_name.into(),
            config,
        })
    }

    /// Download the given image and return a `ServiceState` representing it.
    pub async fn pull_service(&self, image_name: &str) -> Result<ServiceState, RuntimeError> {
        let (config, snapshot_name) = self.fetch_image(image_name).await?;

        let (service_config, config) = if let Some(config) = config.config {
            (
                ServiceConfig::from(config.clone()),
                ContainerConfig::from(config),
            )
        } else {
            (ServiceConfig::default(), ContainerConfig::default())
        };

        Ok(ServiceState {
            container: ContainerState {
                snapshot: snapshot_name.into(),
                config,
            },
            service_config,
        })
    }

    /// Whether a containerd error is a benign "created concurrently by another pull" outcome.
    ///
    /// Every object serpentine writes is content addressed, so an `AlreadyExists` returned from a
    /// racing pull means some other process produced byte identical content. That is safe to treat
    /// as success rather than an error.
    fn is_already_exists(status: &containerd_client::tonic::Status) -> bool {
        status.code() == containerd_client::tonic::Code::AlreadyExists
    }

    /// Pull the given image from the registry and return both the snapshot name and config.
    async fn fetch_image(
        &self,
        image_name: &str,
    ) -> Result<(oci_client::config::ConfigFile, String), RuntimeError> {
        let image = oci_client::Reference::try_from(image_name)?;
        let auth = oci_client::secrets::RegistryAuth::Anonymous;

        log::debug!("Pulling image {image} manifest");
        let (manifest, manifest_digest, config) =
            self.oci.pull_manifest_and_config(&image, &auth).await?;
        let pull_guard = acquire_file_lock(&manifest_digest).await?;
        let lease = self.new_lease().await?;

        log::debug!("Pulling image {image}");
        let snapshot_name = self
            .create_snapshots(&image, image_name, manifest, &lease)
            .await?;
        self.drop_lease(lease).await?;

        pull_guard.unlock();

        let config: oci_client::config::ConfigFile =
            serde_json::from_str(&config).map_err(|err| RuntimeError::internal(err.to_string()))?;

        Ok((config, snapshot_name))
    }

    /// Create layer snapshots from the manifest, this assumes the layer content is in the content
    /// store
    async fn create_snapshots(
        &self,
        image: &oci_client::Reference,
        image_name: &str,
        manifest: oci_client::manifest::OciImageManifest,
        lease: &str,
    ) -> Result<String, RuntimeError> {
        let mut parent = String::new();
        let layer_count = manifest.layers.len();

        let task = self.reporter.start_task(TaskKind::Pull, image_name);
        self.reporter.task_layer_progress(task.id(), 0, layer_count);

        let mut layer_stack_hash = blake3::Hasher::new();
        let mut snapshot_name = String::new();

        for (index, layer) in manifest.layers.into_iter().enumerate() {
            layer_stack_hash.update(layer.digest.as_bytes());
            snapshot_name = layer_stack_hash.finalize().to_hex().to_string();

            let pull_guard = acquire_file_lock(&snapshot_name).await?;

            let layer_exists = self
                .containerd
                .snapshot()
                .stat(containerd_services::snapshots::StatSnapshotRequest {
                    snapshotter: SNAPSHOTTER.to_owned(),
                    key: snapshot_name.clone(),
                })
                .await
                .is_ok();

            let is_final_layer = index == layer_count.saturating_sub(1);

            if layer_exists {
                log::debug!("Snapshot {snapshot_name} already exists.");
            } else {
                self.pull_layer(image, &layer, lease, task.id()).await?;

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

                log::debug!("Committing {key} to {snapshot_name}");
                let labels = if is_final_layer {
                    HashMap::from([("containerd.io/gc.root".to_owned(), "1".to_owned())])
                } else {
                    HashMap::new()
                };
                let commit = self
                    .containerd
                    .snapshot()
                    .commit(
                        containerd_services::snapshots::CommitSnapshotRequest {
                            snapshotter: SNAPSHOTTER.to_owned(),
                            name: snapshot_name.clone(),
                            key,
                            labels,
                        }
                        .with_lease(lease),
                    )
                    .await;
                if let Err(status) = commit
                    && !Self::is_already_exists(&status)
                {
                    return Err(status.into());
                }
            }

            pull_guard.unlock();

            self.reporter
                .task_layer_progress(task.id(), index.saturating_add(1), layer_count);
            parent = snapshot_name.clone();
        }

        Ok(snapshot_name)
    }

    /// Pull the given layer into containerd.
    async fn pull_layer(
        &self,
        image: &oci_client::Reference,
        layer: &oci_client::manifest::OciDescriptor,
        lease: &str,
        task_id: TaskId,
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
        let total_size: i64 = layer_stream
            .content_length
            .and_then(|len| len.try_into().ok())
            .unwrap_or(0);
        let upload_ref = uuid::Uuid::new_v4().to_string();
        let upload_ref_clone = upload_ref.clone();
        let digest = layer.digest.clone();
        let digest_clone = digest.clone();

        self.reporter
            .task_bytes(task_id, 0, total_size.cast_unsigned());
        let reporter = self.reporter.clone();

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
                        reporter.task_bytes(
                            task_id,
                            *current_offset as u64,
                            total_size.cast_unsigned(),
                        );
                        futures_util::future::ready(Some(write))
                    })
                    .with_lease(lease),
            )
            .await?
            .into_inner()
            .try_for_each(async |_| Ok(()))
            .await?;

        log::debug!("Finished pulling {digest_clone}.");
        let commit = self
            .containerd
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
            .await;
        if let Err(status) = commit
            && !Self::is_already_exists(&status)
        {
            return Err(status.into());
        }

        Ok::<_, RuntimeError>(())
    }

    /// Execute a command on top of a given state and return a new state representing the result
    pub async fn exec(
        &self,
        state: &ContainerState,
        cmd: String,
    ) -> Result<ContainerState, RuntimeError> {
        let lease = self.new_lease().await?;
        let (container, _) = self.exec_internal(state.clone(), cmd, &lease).await?;
        self.drop_lease(lease).await?;

        Ok(container)
    }

    /// Execute a command return its stdout and stderr.
    pub async fn exec_get_output(
        &self,
        state: &ContainerState,
        cmd: String,
    ) -> Result<String, RuntimeError> {
        let lease = self.new_lease().await?;
        let stdout = self
            .exec_internal(state.clone(), cmd, &lease)
            .await?
            .1
            .map_err(|output| RuntimeError::NonUtf8Capture { output })?;
        self.drop_lease(lease).await?;

        Ok(stdout)
    }

    /// Retrieve a `ConcreteTopology` matching the given `AbstractTopology` from the free network pool, or create a new one if none are available.
    async fn get_network(
        &self,
        topology: network::AbstractTopology,
    ) -> Result<network::ConcreteTopology, RuntimeError> {
        if let Some(concrete_topology) = self
            .free_networks
            .lock()
            .await
            .get_mut(&topology)
            .and_then(Vec::pop)
        {
            log::debug!("Reusing network {concrete_topology:?} for topology {topology:?}");
            Ok(concrete_topology)
        } else {
            log::debug!("Creating new network for topology {topology:?}");
            let concrete_topology = self.sidecar.create_network(topology.clone()).await?;
            self.dangling
                .lock()
                .await
                .push(DanglingResource::Network(concrete_topology.clone()));
            Ok(concrete_topology)
        }
    }

    /// Execute a command on the given mutable snapshot, returning its stdout and stderr
    /// The stdout will be wrapped in `Ok` if all the data was UTF-8, `Err` if not.
    async fn exec_internal(
        &self,
        state: ContainerState,
        cmd: String,
        lease: &str,
    ) -> Result<(ContainerState, Result<String, String>), RuntimeError> {
        let exec_lock = self.exec_lock.acquire().await;
        log::debug!("Preparing to execute {cmd:?} in {state:?}");
        let container_topology = state.into_topology(cmd.into());
        let abstract_topology = container_topology.map_data_ref(|_| ());
        let network_topology = self.get_network(abstract_topology.clone()).await?;
        let complete_topology = container_topology.zip(network_topology.clone());

        let running_topology = self.spinup_topology(complete_topology, lease).await?;
        let handle = running_topology.get_data();

        self.wait_for_exit(handle.id.clone(), String::new()).await?;
        let (container, stdout) = self.spindown_topology(running_topology).await?;
        self.free_networks
            .lock()
            .await
            .entry(abstract_topology)
            .or_default()
            .push(network_topology);

        let container = match container {
            ContainerLike::Container(container) => container,
            ContainerLike::Service(_) => {
                log::error!("Root of topology was a service, this should never happen");
                return Err(RuntimeError::internal(
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
        topology: network::Topology<(ContainerTopologyNode, network::Namespace)>,
        lease: &str,
    ) -> Result<network::Topology<ContainerHandle>, RuntimeError> {
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
            .await?
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
            .await?
            .into_inner();

        log::debug!("Starting {:?} in {container}", node.get_cmd());
        // A empty `exec_id` signifies the main process of a container
        self.containerd
            .tasks()
            .start(containerd_services::StartRequest {
                container_id: container.clone(),
                exec_id: String::new(),
            })
            .await?;

        if let ContainerLike::Service(service) = &node.state {
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
    ) -> Result<(), RuntimeError> {
        base_process.set_args(Some(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            command.to_string(),
        ]));

        let task = self
            .reporter
            .start_task(TaskKind::Exec, format!("[healthcheck] {command}"));

        let start_time = std::time::Instant::now();
        loop {
            if start_time.elapsed() > timeout {
                return Err(RuntimeError::HealthcheckTimeout {
                    check: command.to_string(),
                    timeout,
                });
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
                                .map_err(|err| RuntimeError::internal(format!("{err}")))?,
                        }),
                    }
                    .with_lease(lease),
                )
                .await?;
            self.containerd
                .tasks()
                .start(containerd_services::StartRequest {
                    container_id: container_id.clone(),
                    exec_id: exec_id.clone(),
                })
                .await?;

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
        state: &ContainerState,
        cmd: String,
        network_namespace: &str,
        hosts: Vec<(Arc<str>, std::net::Ipv4Addr)>,
        mounts: Box<[containerd_client::types::Mount]>,
        lease: &str,
    ) -> Result<(String, oci_spec::runtime::Process), RuntimeError> {
        let container = uuid::Uuid::new_v4().to_string();
        self.dangling
            .lock()
            .await
            .push(DanglingResource::Task(container.clone().into()));

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

    /// read the /etc/passwd file to supplmenet the info given by the container config and
    /// construct a full user object.
    ///
    /// Also returns the home directory.
    async fn construct_spec_user(
        &self,
        mounts: Box<[containerd_client::types::Mount]>,
        user: &str,
    ) -> Result<(oci_spec::runtime::User, Box<str>), RuntimeError> {
        let Ok(passwd) = self
            .read_file(mounts.clone(), UnixPathBuf::from("/etc/passwd"))
            .await?
            .parse();
        let Ok(groups) = self
            .read_file(mounts, UnixPathBuf::from("/etc/group"))
            .await?
            .parse();

        let Ok(user): Result<userdb::OciUser, _> = user.parse();
        let (user, home_dir) = user.resolve(passwd, &groups)?;

        Ok((user, home_dir))
    }

    /// Read the given file from the given mounts
    ///
    /// This intended for known good paths and should not be used for user specific files.
    ///
    /// This requires the file to be UTF-8.
    async fn read_file(
        &self,
        mounts: impl IntoIterator<Item = containerd_client::types::Mount>,
        file: UnixPathBuf,
    ) -> Result<String, RuntimeError> {
        let mut file_system_stream = self.sidecar.export_files(mounts, file).await?;

        let FileSystemEntryHeader::File { .. } =
            serpentine_internal::read_postcard_frame(&mut file_system_stream).await?
        else {
            return Err(RuntimeError::internal("Expected file entry header"));
        };

        let mut passwd = String::new();
        file_system_stream.read_to_string(&mut passwd).await?;
        Ok(passwd)
    }

    /// Write the given file into the given mounts
    async fn write_file(
        &self,
        mounts: impl IntoIterator<Item = containerd_client::types::Mount>,
        file: UnixPathBuf,
        content: &[u8],
    ) -> Result<(), RuntimeError> {
        let mut stream = self.sidecar.import_files(mounts, file).await?;

        let header = serpentine_internal::FileSystemEntryHeader::File {
            name: Box::default(),
            length: content.len() as u64,
        };
        serpentine_internal::write_postcard_frame(&header, &mut stream).await?;
        stream.write_all(content).await?;

        Ok(())
    }

    /// Write the hosts file to the sidecar and return a appropriate bind mount for it.
    async fn write_hosts_file(
        &self,
        hosts: Vec<(Arc<str>, std::net::Ipv4Addr)>,
    ) -> Result<oci_spec::runtime::Mount, RuntimeError> {
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
        .await?;

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
    async fn wait_for_exit(
        &self,
        container_id: String,
        exec_id: String,
    ) -> Result<u32, RuntimeError> {
        log::debug!("Waiting for {container_id}/{exec_id} to exit.");

        let exit_code = self
            .containerd
            .tasks()
            .wait(containerd_services::WaitRequest {
                container_id,
                exec_id,
            })
            .await?
            .into_inner()
            .exit_status;

        Ok(exit_code)
    }

    /// Spin down a given topology of running containers.
    async fn spindown_topology(
        &self,
        containers: network::Topology<ContainerHandle>,
    ) -> Result<(ContainerLike, Result<String, String>), RuntimeError> {
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

        let final_snapshot = uuid::Uuid::new_v4().to_string();
        self.containerd
            .snapshot()
            .commit(containerd_services::snapshots::CommitSnapshotRequest {
                snapshotter: SNAPSHOTTER.to_owned(),
                name: final_snapshot.clone(),
                key: handle.snapshot.clone(),
                labels: HashMap::from([("containerd.io/gc.root".to_owned(), "1".to_owned())]),
            })
            .await?;

        if let Err(err) = self.export_snapshot(&final_snapshot).await {
            log::error!("Failed to export snapshot: {err}");
            debug_assert!(false, "Failed to export snapshot");
        }

        let stdout = handle
            .stdout
            .await
            .map_err(|_| RuntimeError::internal("Failed to join task"))?;

        if exit_code != 0 {
            if matches!(handle.node.state, ContainerLike::Service(_)) {
                log::warn!(
                    "Service {} exited with code {exit_code}, this may be expected if the service didnt shutdown in time.",
                    handle.id
                );
            } else {
                let stdout = stdout.unwrap_or_else(|err| err);

                return Err(RuntimeError::CommandExecution {
                    code: exit_code,
                    command: handle.node.get_cmd().to_owned(),
                    output: stdout,
                });
            }
        }

        let mut services = BTreeMap::new();
        for child in children {
            let Some(hostname) = &child.get_data().node.hostname else {
                return Err(RuntimeError::internal(
                    "Child container missing hostname".to_owned(),
                ));
            };
            let hostname = Arc::clone(hostname);

            let (child_container, _) = Box::pin(self.spindown_topology(child)).await?;

            if let ContainerLike::Service(service) = child_container {
                services.insert(hostname, service);
            } else {
                return Err(RuntimeError::internal("Expected a service".to_owned()));
            }
        }

        let mut container = handle.node.state;
        container.snapshot = final_snapshot.into();
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

    /// Copy the given file/directory into the container
    pub async fn copy_fs_into_container(
        &self,
        state: &ContainerState,
        src: FileSystem,
        dest: &UnixPath,
    ) -> Result<ContainerState, RuntimeError> {
        let snapshot = uuid::Uuid::new_v4().to_string();
        let lease = self.new_lease().await?;

        let dest = if dest.as_bytes() == b"." {
            UnixPath::new("")
        } else {
            dest
        };

        let mounts = self
            .containerd
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
            .await?
            .into_inner()
            .mounts;

        log::debug!("Copying filesystem into container at {dest}");
        let dest = state.config.working_dir.join(dest);

        let mut src = src.get_reader().await?;
        let mut dest = self.sidecar.import_files(mounts, dest).await?;
        tokio::io::copy(&mut src, &mut dest).await?;

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

        if let Err(err) = self.export_snapshot(&new_snapshot).await {
            log::error!("Failed to export snapshot: {err}");
            debug_assert!(false, "Failed to export snapshot");
        }

        Ok(ContainerState {
            snapshot: new_snapshot.into(),
            config: state.config.clone(),
        })
    }

    /// Export the given path from the container into a `FileSystem`
    pub async fn export_path(
        &self,
        state: &ContainerState,
        docker_path: &UnixPath,
    ) -> Result<FileSystem, RuntimeError> {
        log::debug!("Creating file system provider for {state:?} at {docker_path}");
        let snapshot = format!("{}/view/{}", state.snapshot, uuid::Uuid::new_v4());
        let docker_path = if docker_path.as_bytes() == b"." {
            UnixPath::new("")
        } else {
            docker_path
        };

        let lease = self.new_lease().await?;
        let mounts = self
            .containerd
            .snapshot()
            .view(
                containerd_services::snapshots::ViewSnapshotRequest {
                    snapshotter: SNAPSHOTTER.into(),
                    parent: state.snapshot.to_string(),
                    key: snapshot,
                    labels: HashMap::new(),
                }
                .with_lease(&lease),
            )
            .await?
            .into_inner()
            .mounts;

        let docker_path = state.config.working_dir.join(docker_path);

        Ok(ContainerFileExport {
            sidecar: self.sidecar,
            mounts: mounts.into(),
            path: docker_path,
        }
        .into())
    }

    /// Shutdown any dangling references
    pub async fn shutdown(self) {
        for dangling in self.dangling.lock().await.drain(..) {
            match dangling {
                DanglingResource::Lease(lease) => {
                    log::debug!("Deleting dangling lease");
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
                    log::debug!("Stopping dangling task");
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
                DanglingResource::Network(network) => {
                    log::debug!("Stopping dangling network namespace");
                    let _ = self.sidecar.delete_network(network).await;
                }
            }
        }
    }

    // TEST: That this works, hard to do as its simply asking containerd to delete it at a later
    // time.

    /// Mark the given snapshot for garbage collection.
    pub async fn delete(&self, snapshot: &str) -> Result<(), RuntimeError> {
        self.containerd
            .snapshot()
            .update(containerd_services::snapshots::UpdateSnapshotRequest {
                snapshotter: SNAPSHOTTER.to_owned(),
                info: Some(containerd_services::snapshots::Info {
                    name: snapshot.to_owned(),
                    labels: HashMap::from([("containerd.io/gc.root".to_owned(), "0".to_owned())]),
                    ..Default::default()
                }),
                update_mask: Some(prost_types::FieldMask {
                    paths: vec!["labels".to_owned()],
                }),
            })
            .await?;

        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let dangling_count = self.dangling.get_mut().len();
        if dangling_count != 0 {
            log::warn!("Leaving {dangling_count} dangling resources running in containerd.");
        }
    }
}

/// A file system provider for a file/folder in a container
#[derive(Clone)]
struct ContainerFileExport {
    /// The sidecar client to use
    sidecar: sidecar_client::Client,
    /// The mounts to use
    mounts: Arc<[containerd_client::types::Mount]>,
    /// The path to export
    path: UnixPathBuf,
}

impl FileSystemProvider for ContainerFileExport {
    fn get_reader<'this>(
        &'this self,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<BoxedReader, RuntimeError>> + Send + 'this>>
    {
        Box::pin(async move {
            log::debug!("Creating reader for {} in container", self.path.display());
            let reader = self
                .sidecar
                .export_files(self.mounts.to_vec(), self.path.clone())
                .await?;
            Ok(BoxedReader::new(reader))
        })
    }

    fn dyn_clone(&self) -> Box<dyn FileSystemProvider> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config-only change must keep referencing the same snapshot keys, so the cache cleanup
    /// does not delete a snapshot still in use.
    #[test]
    fn config_edit_keeps_snapshot_keys() {
        bolero::check!()
            .with_type()
            .for_each(|container: &ContainerState| {
                let edited = container.update_config(|config| {
                    config.set_env_var("SERPENTINE_TEST".into(), "1".into());
                });

                let mut original = Vec::new();
                container.collect_snapshots(&mut original);
                let mut edited_keys = Vec::new();
                edited.collect_snapshots(&mut edited_keys);

                assert_eq!(original, edited_keys);
            });
    }

    /// Attached services contribute their snapshot keys, so their snapshots are cleaned up once
    /// orphaned instead of leaking.
    #[test]
    fn service_snapshots_are_collected() {
        bolero::check!().with_type().for_each(
            |(container, service): &(ContainerState, ContainerState)| {
                let with_service = container.update_config(|config| {
                    config.with_service(service.clone().into_service("entry".into()), "db".into());
                });

                let mut keys = Vec::new();
                with_service.collect_snapshots(&mut keys);
                let mut expected = Vec::new();
                container.collect_snapshots(&mut expected);
                service.collect_snapshots(&mut expected);

                assert_eq!(keys, expected);
            },
        );
    }
}

#[cfg(test)]
#[cfg(feature = "_test_docker")]
#[expect(clippy::expect_used, reason = "Tests")]
mod integration_tests {
    use rstest::{fixture, rstest};
    use typed_path::PlatformPathBuf;

    use super::*;

    const TEST_IMAGE: &str = "quay.io/toolbx-images/alpine-toolbox:3.21@sha256:ff9f4d34ce354d6be4c8fc551ebb1bb57c5941df4b42c970b9852f3744fb6bf0";

    #[fixture]
    async fn containerd_client() -> Client {
        Client::new(
            Reporter::none(),
            Arc::new(crate::engine::cache::NoneCacheBackend),
            1,
            "serpentine-test",
            false,
        )
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
    async fn exec_output_has_writable_filesystem(#[future] containerd_client: Client) {
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
                "Exec failed on non-utf8 stdout, even tho we werent explicitly capturing it here. ",
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
            .export_path(&from, UnixPath::new("/tmp/hello.txt"))
            .await
            .expect("Export failed");

        let to = containerd_client
            .copy_fs_into_container(&base, file, UnixPath::new("nice.txt"))
            .await
            .expect("Failed to copy into container");

        containerd_client
            .exec(&to, "ls".to_owned())
            .await
            .expect("Exec failed");

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
            .export_path(&from, UnixPath::new("/tmp/foo"))
            .await
            .expect("Export failed");

        let to = containerd_client
            .copy_fs_into_container(&base, file, UnixPath::new("hello"))
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
        let base = base.update_config(|config| config.set_working_dir(UnixPath::new("/testing")));

        let from = containerd_client
            .exec(&base, "mkdir -p ./foo/bar/baz".to_owned())
            .await
            .expect("Exec failed");

        let from = containerd_client
            .exec(&from, "echo hello > ./foo/bar/baz/nice.txt".to_owned())
            .await
            .expect("Exec failed");

        let file = containerd_client
            .export_path(&from, UnixPath::new("./foo"))
            .await
            .expect("Export failed");

        let to = containerd_client
            .copy_fs_into_container(&base, file, UnixPath::new("./hello"))
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
    async fn copy_folder_between_containers_relative_paths_dot(
        #[future] containerd_client: Client,
    ) {
        let containerd_client = containerd_client.await;
        let base = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let base = base.update_config(|config| config.set_working_dir(UnixPath::new("/testing")));

        let from = containerd_client
            .exec(&base, "mkdir -p ./foo/bar/baz".to_owned())
            .await
            .expect("Exec failed");

        let from = containerd_client
            .exec(&from, "echo hello > ./foo/bar/baz/nice.txt".to_owned())
            .await
            .expect("Exec failed");

        let file = containerd_client
            .export_path(&from, UnixPath::new("."))
            .await
            .expect("Export failed");

        let to = containerd_client
            .copy_fs_into_container(&base, file, UnixPath::new("."))
            .await
            .expect("Failed to copy into container");

        containerd_client
            .exec(&to, "ls ./foo/bar/baz".to_owned())
            .await
            .expect("Exec failed");

        containerd_client
            .exec(
                &to,
                "grep -q hello ./foo/bar/baz/nice.txt || exit 1".to_owned(),
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

        let fs = containerd_client
            .export_path(&base, UnixPath::new("i_am_not_real.txt"))
            .await
            .expect("Export only creates lazy reader");

        let result = containerd_client
            .copy_fs_into_container(&base, fs, UnixPath::new("huh.txt"))
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
        let image = image.update_config(|config| config.set_working_dir(UnixPath::new("/foo")));
        containerd_client
            .exec(&image, "ls bar".to_owned())
            .await
            .expect("Exec failed");

        let image = image.update_config(|config| config.set_working_dir(UnixPath::new("./bar")));
        let working_dir_pwd = containerd_client
            .exec_get_output(&image, "pwd".to_owned())
            .await
            .expect("Exec failed");
        assert_eq!(
            working_dir_pwd.trim(),
            "/foo/bar".to_owned(),
            "pwd reported wrong working directory"
        );

        let image = image.update_config(|config| config.set_working_dir(UnixPath::new("/app")));
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
        let image =
            image.update_config(|config| config.set_env_var("HELLO".into(), "WORLD".into()));
        let exec = containerd_client
            .exec_get_output(&image, "echo -n $HELLO".to_owned())
            .await
            .expect("Exec failed");
        let get_env = image
            .get_config()
            .get_env_var("HELLO")
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

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn dns_access(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let image = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        containerd_client
            .exec(&image, "curl https://google.com".to_owned())
            .await
            .expect("Exec failed");
    }

    #[tokio::test]
    #[test_log::test]
    async fn export_import_cache() {
        let caching_dir = tempfile::TempDir::new().unwrap();
        let caching_dir = PlatformPathBuf::from(caching_dir.path().as_os_str().as_encoded_bytes());
        let cache = crate::engine::cache::LocalCacheBackend::new(caching_dir)
            .await
            .unwrap();
        let cache = Arc::new(cache) as Arc<dyn CacheBackend + Send + Sync>;

        let first_client = Client::new(
            Reporter::none(),
            Arc::clone(&cache),
            1,
            uuid::Uuid::new_v4().to_string(),
            true,
        )
        .await
        .unwrap();

        let image = first_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let first_layer = first_client
            .exec(
                &image,
                String::from(
                    "
mkdir -p foo/bar &&
touch foo/bar/test1.txt &&
touch foo/bar/test2.txt &&
ln -s foo bar_sym &&
ln foo/bar/test1.txt test1.txt &&

mkdir -p mov_source &&
touch mov_source/test1.txt &&

mkdir -p copy_source &&
touch copy_source/test1.txt &&

mkdir -p whiteout &&
touch whiteout/test1.txt &&

mkdir -p opaque &&
touch opaque/test1.txt
",
                ),
            )
            .await
            .expect("Failed to run exec");

        let second_layer = first_client
            .exec(
                &first_layer,
                String::from(
                    "
ln -s foo parent_sym &&
ln foo/bar/test1.txt parent_hard &&
rm foo/bar/test2.txt &&

mv mov_source mov_parent &&
cp -r copy_source copy_parent &&
rm -r whiteout &&

rm -r opaque &&
mkdir -p opaque &&
touch opaque/test2.txt
",
                ),
            )
            .await
            .expect("Failed to run exec");

        let test_command = String::from(
            "
! cat foo/bar/test2.txt &&
cat foo/bar/test1.txt &&
cat bar_sym/bar/test1.txt &&
cat test1.txt &&
cat parent_sym/bar/test1.txt &&
cat parent_hard &&

! ls mov_source &&
cat mov_parent/test1.txt &&

cat copy_source/test1.txt &&
cat copy_parent/test1.txt &&

! ls whiteout &&

! cat opaque/test1.txt &&
cat opaque/test2.txt
",
        );

        first_client
            .exec(&second_layer, test_command.clone())
            .await
            .expect("Failed to run test command on original containerd.");

        let second_client = Client::new(
            Reporter::none(),
            Arc::clone(&cache),
            1,
            uuid::Uuid::new_v4().to_string(),
            false,
        )
        .await
        .unwrap();

        second_client.healthcheck_value(&second_layer).await;
        second_client
            .exec(&second_layer, test_command.clone())
            .await
            .expect("Failed to run test command on new containerd.");
    }
}
