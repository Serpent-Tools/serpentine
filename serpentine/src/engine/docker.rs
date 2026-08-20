//! Spin up containerd using the docker api.

use std::collections::HashMap;
use std::process::Command;

use bollard::API_DEFAULT_VERSION;
use futures_util::StreamExt;

use crate::engine::{RuntimeError, acquire_file_lock, sidecar_client};
use crate::events::{Reporter, TaskKind};

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
) -> Result<(&'static str, sidecar_client::Client), RuntimeError> {
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
/// Returns the name of the strategy that succeeded alongside the client.
async fn connect_docker() -> Result<(&'static str, bollard::Docker), RuntimeError> {
    log::info!("Connecting to Docker daemon");
    log::debug!("DOCKER_HOST={:?}", std::env::var("DOCKER_HOST"));

    for (name, strategy) in STRATEGIES {
        let Some(client) = strategy() else {
            log::info!("{name}: not available");
            continue;
        };

        match client.ping().await {
            Ok(_) => {
                log::info!("{name}: connected");
                return Ok((name, client));
            }
            Err(err) => {
                log::warn!("{name}: ping failed: {err}");
            }
        }
    }

    Err(RuntimeError::DockerNotFound {
        inner: Box::new(miette::MietteDiagnostic::new(
            "no working Docker or Podman connection found",
        )),
    })
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

/// Spin up a containerd instance using the given docker client.
///
/// Returns the URI to connect to
async fn spin_up_containerd(
    docker: bollard::Docker,
    reporter: &Reporter,
) -> Result<std::net::SocketAddr, RuntimeError> {
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
                        auto_remove: Some(false),
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
            return Err(err.into());
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
            return Err(err.into());
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
async fn wait_for_host_port(docker: &bollard::Docker) -> Result<u16, RuntimeError> {
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(30);
    let port_key = format!("{}/tcp", serpentine_internal::sidecar::PORT);
    loop {
        let container_details = docker
            .inspect_container(
                CONTAINER_NAME,
                Some(bollard::query_parameters::InspectContainerOptions::default()),
            )
            .await?;

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
                .map_err(|_| RuntimeError::internal("Port wasn't a number"));
        }

        if start.elapsed() >= timeout {
            return Err(RuntimeError::internal(
                "Timed out waiting for host port binding on containerd container",
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Ensure the containerd data volume exists
async fn create_containerd_volume(docker: &bollard::Docker) -> Result<&'static str, RuntimeError> {
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
            .await?;
    }

    Ok(CONTAINER_VOLUME)
}

/// Ensure the `containerd` image is downloaded
async fn ensure_containerd_image(
    docker: &bollard::Docker,
    reporter: &Reporter,
) -> Result<Box<str>, RuntimeError> {
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
