//! Wrapper around containerd API client and other container related operations
#![expect(
    clippy::field_scoped_visibility_modifiers,
    reason = "these sub modules are tightly coupled and move together."
)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use backon::Retryable;
use containerd_client::services::v1 as containerd_services;
use containerd_client::tonic::{IntoRequest, Request};
use miette::{Context, IntoDiagnostic};
use serpentine_internal::network;

use crate::engine::cache::CacheBackend;
use crate::engine::sidecar_client;
use crate::events::{Lifecycle, Reporter};

mod container_config;
mod exec;
mod files;
mod layers;

#[cfg(test)]
pub use container_config::ContainerConfig;
pub use container_config::{ContainerLike, ContainerState, ServiceState}; // Some fuzz tests want to construct these more directly.

/// Label that when set to `1` makes containerd not clean up a resource.
const CONTAINERD_GC_ROOT_LABEL: &str = "containerd.io/gc.root";

/// The snapshotter to use for containers.
// WARN: While most of the code is snapshotter agnostic, the sidecar export layer implementation uses
// overlayfs specific knowledge to efficiently produce filesystem diffs.
// If we change snapshotter in the future that code will need to be updated.
const SNAPSHOTTER: &str = "overlayfs";

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
enum DanglingResource {
    /// A lease, this dangling would lead to gc holding onto unneeded data
    Lease(Box<str>),
    /// A task, this dangling would leave processes running that arent useful anymore.
    /// This holds the container id
    Task(Box<str>),
    /// A container network
    Network(network::ConcreteTopology),
}

/// A containerd client wrapper
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
    /// Dangling resources
    dangling: Mutex<Vec<DanglingResource>>,
    /// Networks that arent currently in use.
    free_networks: Mutex<HashMap<network::AbstractTopology, Vec<network::ConcreteTopology>>>,
}

impl Client {
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
    ) -> miette::Result<Self> {
        let oci = oci_client::Client::new(oci_client::client::ClientConfig {
            user_agent: concat!("serpentine/", env!("CARGO_PKG_VERSION")),
            platform_resolver: Some(Box::new(layers::platform_resolver)),
            ..Default::default()
        });

        let (runtime, sidecar) = crate::engine::docker::connect(&reporter).await?;

        let containerd = (async || -> miette::Result<containerd_client::Client> {
            let channel =
                containerd_client::tonic::transport::Endpoint::from_static("http://[::]:0")
                    .connect_with_connector(tower::service_fn(move |_| async move {
                        sidecar
                            .containerd()
                            .await
                            .map_err(std::io::Error::other)
                            .map(hyper_util::rt::TokioIo::new)
                    }))
                    .await
                    .into_diagnostic()
                    .context("connecting to containerd through the sidecar")?;
            let client = containerd_client::Client::from(channel);
            client
                .version()
                .version(())
                .await
                .into_diagnostic()
                .context("probing containerd version through the sidecar")?;

            Ok(client)
        })
        .retry(
            backon::ExponentialBuilder::default()
                .without_max_delay()
                .with_total_delay(Some(Duration::from_secs(30))),
        )
        .sleep(tokio::time::sleep)
        .await?;

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
            exec_lock: tokio::sync::Semaphore::new(exec_permits),
            dangling: Mutex::new(Vec::new()),
            free_networks: Mutex::new(HashMap::new()),
        })
    }

    /// Push something into the dangling resources
    fn register_dangling(&self, resource: DanglingResource) {
        if let Ok(mut dangling) = self.dangling.lock() {
            dangling.push(resource);
        } else {
            log::warn!("Failed to get dangling resource lock");
        }
    }

    /// Create a new lease
    async fn new_lease(&self) -> miette::Result<String> {
        let lease = uuid::Uuid::new_v4().to_string();
        self.register_dangling(DanglingResource::Lease(lease.clone().into()));

        self.containerd
            .leases()
            .create(containerd_services::CreateRequest {
                id: lease.clone(),
                labels: HashMap::new(),
            })
            .await
            .into_diagnostic()
            .context("creating a containerd lease")?;
        Ok(lease)
    }

    /// Drop the given lease, freeing up any not referenced elsewhere.
    async fn drop_lease(&self, lease: String) -> miette::Result<()> {
        self.containerd
            .leases()
            .delete(containerd_services::DeleteRequest {
                id: lease.clone(),
                sync: false,
            })
            .await
            .into_diagnostic()
            .with_context(|| format!("dropping containerd lease {lease}"))?;
        Ok(())
    }

    /// Whether a containerd error is a benign "created concurrently by another pull" outcome.
    ///
    /// Every object serpentine writes is content addressed, so an `AlreadyExists` returned from a
    /// racing pull means some other process produced byte identical content. That is safe to treat
    /// as success rather than an error.
    fn is_already_exists(status: &containerd_client::tonic::Status) -> bool {
        status.code() == containerd_client::tonic::Code::AlreadyExists
    }

    /// Shutdown any dangling references
    #[expect(
        clippy::await_holding_lock,
        reason = "shutdown, only thing wanting it."
    )]
    pub async fn shutdown(self) {
        if let Ok(mut dangling_resources) = self.dangling.lock() {
            for dangling in dangling_resources.drain(..) {
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
        } else {
            log::warn!("Failed to get dangling resources in shutdown");
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let Ok(dangling) = self.dangling.get_mut() else {
            log::warn!("Failed to get dangling resources in drop");
            return;
        };

        if !dangling.is_empty() {
            log::warn!(
                "Leaving {} dangling resources running in containerd.",
                dangling.len()
            );
        }
    }
}

/// Fixtures shared by the integration tests of the sibling modules.
#[cfg(test)]
#[cfg(feature = "_test_docker")]
mod test_support {
    use std::sync::Arc;

    use rstest::fixture;

    use super::Client;
    use crate::engine::cache::NoneCacheBackend;
    use crate::events::Reporter;

    /// The image the integration tests run against.
    pub(super) const TEST_IMAGE: &str = "quay.io/toolbx-images/alpine-toolbox:3.22@sha256:deff987a572bcbbe3059a644d60d6dd1cfbda9b99f9e5a948ee4e5f0d0206cbe";

    /// A client talking to the live containerd, with caching disabled.
    #[fixture]
    pub(super) async fn containerd_client() -> miette::Result<Client> {
        Client::new(
            Reporter::none(),
            Arc::new(NoneCacheBackend),
            1,
            "serpentine-test",
        )
        .await
    }
}
