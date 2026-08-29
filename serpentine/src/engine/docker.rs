//! Spin up containerd using the docker api.

use std::collections::HashMap;
use std::process::Command;

use bollard::API_DEFAULT_VERSION;
use bollard::query_parameters::{RemoveContainerOptionsBuilder, RemoveVolumeOptionsBuilder};
use futures_util::StreamExt;
use miette::{Context, Diagnostic, IntoDiagnostic};
use thiserror::Error;

use crate::engine::{acquire_file_lock, sidecar_client};
use crate::events::{Reporter, TaskKind};

/// Error establishing connection to docker/podman
#[derive(Debug, Error, Diagnostic)]
#[error("Docker/Podman not found")]
#[diagnostic(code(docker_not_found))]
#[diagnostic(help(
    "If docker or podman is installed try setting `DOCKER_HOST` environment variable explicitly."
))]
struct DockerNotFound {
    /// Why each strategy did not reach a daemon
    #[related]
    failures: Vec<StrategyFailure>,
}

/// A connection strategy that did not reach a daemon.
#[derive(Debug, Error, Diagnostic)]
enum StrategyFailure {
    /// The strategy could not build a client at all.
    #[error("{name}: not available")]
    Unavailable {
        /// The strategy that was tried
        name: &'static str,
    },

    /// The daemon the strategy found did not answer a ping.
    #[error("{name}: ping failed")]
    Ping {
        /// The strategy that was tried
        name: &'static str,
        /// The daemon's response
        #[source]
        error: bollard::errors::Error,
    },
}

/// The name of the containerd daemon serpentine spawns.
const CONTAINER_NAME: &str = "serpent-tools.containerd";

/// The name of the docker volume container
const CONTAINER_VOLUME: &str = "serpent-tools.containerd-data";

/// The containerd tag version to use
pub(crate) const CONTAINERD_IMAGE_TAG: &str =
    if cfg!(debug_assertions) || cfg!(test) || cfg!(feature = "_bench") {
        "dev"
    } else {
        env!("CARGO_PKG_VERSION")
    };

/// The container image to use for containerd
const CONTAINERD_IMAGE: &str = "serpent-tools/containerd";

/// Create a new containerd client, by either connecting to an existing container or spinning up a
/// new one.
///
/// Returns the name of the runtime that answered alongside the client.
pub async fn connect(
    reporter: &Reporter,
) -> miette::Result<(&'static str, sidecar_client::Client)> {
    let (runtime, docker) = connect_docker().await?;
    let containerd_addr = spin_up_containerd(docker, reporter).await?;
    Ok((runtime, sidecar_client::Client::new(containerd_addr)))
}

/// A named strategy for connecting to a Docker-compatible daemon.
type ConnectionStrategy = (&'static str, fn() -> Option<bollard::Docker>);

/// Connection strategies, tried in order.
const STRATEGIES: &[ConnectionStrategy] = &[
    ("defaults", try_defaults),
    ("docker CLI context", try_docker_cli),
    ("podman", try_podman),
];

/// Attempt to connect to docker or podman, trying each strategy in order.
///
/// Returns the name of the runtime that answered alongside the client.
async fn connect_docker() -> miette::Result<(&'static str, bollard::Docker)> {
    log::info!("Connecting to Docker daemon");
    log::debug!("DOCKER_HOST={:?}", std::env::var("DOCKER_HOST"));

    let mut failures = Vec::new();
    for &(name, strategy) in STRATEGIES {
        let Some(client) = strategy() else {
            log::info!("{name}: not available");
            failures.push(StrategyFailure::Unavailable { name });
            continue;
        };

        match client.ping().await {
            Ok(_) => {
                log::info!("{name}: connected");
                let runtime = detect_runtime(&client).await.unwrap_or(name);
                return Ok((runtime, client));
            }
            Err(error) => {
                log::warn!("{name}: ping failed: {error}");
                failures.push(StrategyFailure::Ping { name, error });
            }
        }
    }

    Err(DockerNotFound { failures }.into())
}

/// Ask the daemon which runtime it is, returning `None` when it does not say.
///
/// The strategy that reached the daemon cannot answer this, since both bollard's defaults and
/// `DOCKER_HOST` reach either runtime. Podman names itself in the platform and component fields of
/// its version response, so anything else is treated as docker.
async fn detect_runtime(client: &bollard::Docker) -> Option<&'static str> {
    let version = client.version().await.ok()?;

    let platform = version
        .platform
        .iter()
        .map(|platform| platform.name.as_str());
    let components = version
        .components
        .iter()
        .flatten()
        .map(|component| component.name.as_str());

    let is_podman = platform
        .chain(components)
        .any(|name| name.to_ascii_lowercase().contains("podman"));

    Some(if is_podman { "podman" } else { "docker" })
}

/// Try connecting via bollard's defaults (respects `DOCKER_HOST` env var).
fn try_defaults() -> Option<bollard::Docker> {
    bollard::Docker::connect_with_defaults().ok()
}

/// Try discovering the Docker socket via the Docker CLI's active context.
fn try_docker_cli() -> Option<bollard::Docker> {
    let output = Command::new("docker")
        .args([
            "context",
            "inspect",
            "--format",
            "{{.Endpoints.docker.Host}}",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let docker_host = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    if docker_host.is_empty() {
        return None;
    }

    log::debug!("Docker CLI reports host: {docker_host}");
    bollard::Docker::connect_with_host(&docker_host).ok()
}

/// Try discovering the Podman socket via the Podman CLI.
fn try_podman() -> Option<bollard::Docker> {
    let output = Command::new("podman")
        .args(["info", "--format", "{{.Host.RemoteSocket.Path}}"])
        .output()
        .ok()?;

    let socket_path = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    bollard::Docker::connect_with_socket(&socket_path, 120, API_DEFAULT_VERSION).ok()
}

/// Whether a docker error carries the given HTTP status code.
///
/// Used to treat a benign create or start race, where another serpentine process already brought
/// the shared containerd container up, as success rather than an error.
fn is_docker_status(err: &bollard::errors::Error, status: u16) -> bool {
    matches!(
        err,
        bollard::errors::Error::DockerResponseServerError { status_code, .. }
            if *status_code == status
    )
}

/// Delete the container and docker volume in order to fully reset the sidecar
pub async fn delete_container_and_volume() -> miette::Result<()> {
    let (runtime, docker) = connect_docker().await?;
    log::info!("Cleaning out serpentine container from {runtime}");

    log::info!("Deleting container {CONTAINER_NAME}");
    let res_container = docker
        .remove_container(
            CONTAINER_NAME,
            Some(RemoveContainerOptionsBuilder::default().force(true).build()),
        )
        .await;

    if let Err(err) = res_container {
        log::error!("Failed to delete container: {err}");
    }

    log::info!("Deleting volume {CONTAINER_VOLUME}");
    let res_volume = docker
        .remove_volume(
            CONTAINER_VOLUME,
            Some(RemoveVolumeOptionsBuilder::default().force(true).build()),
        )
        .await;

    if let Err(err) = res_volume {
        log::error!("Failed to delete volume: {err}");
    }

    log::info!("Deleted serpentine container states");

    Ok(())
}

/// Spin up a containerd instance using the given docker client.
///
/// Returns the address to connect to
async fn spin_up_containerd(
    docker: bollard::Docker,
    reporter: &Reporter,
) -> miette::Result<std::net::SocketAddr> {
    let _setup_guard = acquire_file_lock(CONTAINER_NAME).await?;
    let volume = create_containerd_volume(&docker).await?;
    let image = ensure_containerd_image(&docker, reporter).await?;

    if docker
        .inspect_container(
            CONTAINER_NAME,
            Some(bollard::query_parameters::InspectContainerOptionsBuilder::new().build()),
        )
        .await
        .is_err()
    {
        log::info!("Creating containerd container with name {CONTAINER_NAME}");

        let created = docker
            .create_container(
                Some(
                    bollard::query_parameters::CreateContainerOptionsBuilder::new()
                        .name(CONTAINER_NAME)
                        .build(),
                ),
                bollard::plugin::ContainerCreateBody {
                    image: Some(image.into_string()),
                    tty: Some(false),
                    open_stdin: Some(false),
                    host_config: Some(bollard::plugin::HostConfig {
                        auto_remove: Some(true),
                        privileged: Some(true),
                        pids_limit: Some(-1),
                        binds: Some(vec![format!("{volume}:/var/lib/containerd")]),
                        log_config: Some(bollard::plugin::HostConfigLogConfig {
                            typ: Some("json-file".to_owned()),
                            config: None,
                        }),
                        port_bindings: Some(HashMap::from([(
                            format!("{}/tcp", serpentine_internal::sidecar::PORT),
                            Some(vec![bollard::plugin::PortBinding {
                                host_ip: Some("127.0.0.1".to_owned()),
                                host_port: None,
                            }]),
                        )])),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await;
        if let Err(err) = created
            && !is_docker_status(&err, 409)
        {
            return Err(err)
                .into_diagnostic()
                .with_context(|| format!("creating the {CONTAINER_NAME} container"));
        }

        let started = docker
            .start_container(
                CONTAINER_NAME,
                Some(bollard::query_parameters::StartContainerOptionsBuilder::new().build()),
            )
            .await;
        if let Err(err) = started
            && !is_docker_status(&err, 304)
        {
            return Err(err)
                .into_diagnostic()
                .with_context(|| format!("starting the {CONTAINER_NAME} container"));
        }
    }

    let serpentine_port = wait_for_host_port(&docker).await?;

    Ok(std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        serpentine_port,
    ))
}

/// Poll `inspect_container` until docker has populated the host port binding for the sidecar.
///
/// Immediately after `start_container` returns, the port binding may still be empty for a brief
/// window before docker reconciles it. Retrying avoids racing on that gap.
async fn wait_for_host_port(docker: &bollard::Docker) -> miette::Result<u16> {
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(30);
    let port_key = format!("{}/tcp", serpentine_internal::sidecar::PORT);
    loop {
        let container_details = docker
            .inspect_container(
                CONTAINER_NAME,
                Some(bollard::query_parameters::InspectContainerOptions::default()),
            )
            .await
            .into_diagnostic()
            .with_context(|| format!("inspecting the {CONTAINER_NAME} container"))?;

        let host_port = container_details
            .network_settings
            .as_ref()
            .and_then(|net| net.ports.as_ref())
            .and_then(|ports| ports.get(&port_key))
            .and_then(Option::as_ref)
            .and_then(|bindings| bindings.first())
            .and_then(|binding| binding.host_port.as_ref());

        if let Some(host_port) = host_port {
            return host_port
                .parse()
                .into_diagnostic()
                .with_context(|| format!("docker reported host port {host_port:?}"));
        }

        if start.elapsed() >= timeout {
            return Err(miette::miette!(
                "timed out after {timeout:?} waiting for docker to bind a host port for {CONTAINER_NAME}"
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Ensure the containerd data volume exists
async fn create_containerd_volume(docker: &bollard::Docker) -> miette::Result<&'static str> {
    if docker.inspect_volume(CONTAINER_VOLUME).await.is_err() {
        log::info!("Creating volume {CONTAINER_VOLUME}");
        docker
            .create_volume(bollard::plugin::VolumeCreateRequest {
                name: Some(CONTAINER_VOLUME.into()),
                driver: Some("local".into()),
                driver_opts: None,
                labels: Some(HashMap::from([(
                    "serpentine.version".into(),
                    env!("CARGO_PKG_VERSION").into(),
                )])),
                cluster_volume_spec: None,
            })
            .await
            .into_diagnostic()
            .with_context(|| format!("creating the {CONTAINER_VOLUME} volume"))?;
    }

    Ok(CONTAINER_VOLUME)
}

/// Ensure the `containerd` image is downloaded
async fn ensure_containerd_image(
    docker: &bollard::Docker,
    reporter: &Reporter,
) -> miette::Result<Box<str>> {
    let image_name = format!("{CONTAINERD_IMAGE}:{CONTAINERD_IMAGE_TAG}").into_boxed_str();

    if docker.inspect_image(&image_name).await.is_err() {
        log::info!("Pulling image {image_name}");
        let task = reporter.start_task(TaskKind::Pull, format!("engine {CONTAINERD_IMAGE_TAG}"));
        let mut layer_state: HashMap<String, bool> = HashMap::new();
        let mut done_count: usize = 0;
        docker
            .create_image(
                Some(
                    bollard::query_parameters::CreateImageOptionsBuilder::new()
                        .from_image(CONTAINERD_IMAGE)
                        .tag(CONTAINERD_IMAGE_TAG)
                        .build(),
                ),
                None,
                None,
            )
            .for_each(|update| {
                if let Ok(ref info) = update {
                    if let (Some(id), Some(status)) = (info.id.as_deref(), info.status.as_deref())
                        && !id.is_empty()
                    {
                        let is_done = matches!(
                            status,
                            "Pull complete" | "Already exists" | "Download complete"
                        );
                        let entry = layer_state.entry(id.to_owned()).or_insert(false);
                        if is_done && !*entry {
                            done_count = done_count.saturating_add(1);
                        }
                        *entry |= is_done;
                    }
                    if let Some(detail) = &info.progress_detail
                        && let (Some(current), Some(total)) = (detail.current, detail.total)
                        && total > 0
                    {
                        reporter.task_bytes(
                            task.id(),
                            current.cast_unsigned(),
                            total.cast_unsigned(),
                        );
                    }
                    if !layer_state.is_empty() {
                        reporter.task_layer_progress(task.id(), done_count, layer_state.len());
                    }
                }
                async {}
            })
            .await;
    }

    Ok(image_name)
}
