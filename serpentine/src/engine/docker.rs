//! Spin up containerd using the docker api.

use std::collections::HashMap;
use std::process::Command;

use bollard::API_DEFAULT_VERSION;
use futures_util::StreamExt;

use crate::engine::{RuntimeError, sidecar_client};

/// The name of the containerd daemon serpentine spawns.
const CONTAINER_NAME: &str = "serpent-tools.containerd";

/// The name of the docker volume container
const CONTAINER_VOLUME: &str = "serpent-tools.containerd-data";

/// The containerd tag version to use
const CONTAINERD_IMAGE_TAG: &str = if cfg!(debug_assertions) {
    "dev"
} else {
    env!("CARGO_PKG_VERSION")
};

/// The container image to use for containerd
const CONTAINERD_IMAGE: &str = "serpent-tools/containerd";

/// Create a new containerd client, by either connecting to an existing container or spinning up a
/// new one.
pub async fn connect() -> Result<sidecar_client::Client, RuntimeError> {
    let docker = connect_docker().await?;
    let containerd_addr = spin_up_containerd(docker).await?;
    Ok(sidecar_client::Client::new(containerd_addr))
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
async fn connect_docker() -> Result<bollard::Docker, RuntimeError> {
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
                return Ok(client);
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

/// Spin up a containerd instance using the given docker client.
///
/// Returns the URI to connect to
async fn spin_up_containerd(docker: bollard::Docker) -> Result<std::net::SocketAddr, RuntimeError> {
    let volume = create_containerd_volume(&docker).await?;
    let image = ensure_containerd_image(&docker).await?;

    if docker
        .inspect_container(
            CONTAINER_NAME,
            Some(bollard::query_parameters::InspectContainerOptionsBuilder::new().build()),
        )
        .await
        .is_err()
    {
        log::info!("Creating containerd container with name {CONTAINER_NAME}");

        docker
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
            .await?;

        docker
            .start_container(
                CONTAINER_NAME,
                Some(bollard::query_parameters::StartContainerOptionsBuilder::new().build()),
            )
            .await?;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    let container_details = docker
        .inspect_container(
            CONTAINER_NAME,
            Some(bollard::query_parameters::InspectContainerOptions::default()),
        )
        .await?;

    let serpentine_port = container_details
        .network_settings
        .ok_or_else(|| RuntimeError::internal("No network settings for container"))?
        .ports
        .ok_or_else(|| RuntimeError::internal("No port settings for container"))?
        .get(&format!("{}/tcp", serpentine_internal::sidecar::PORT))
        .ok_or_else(|| RuntimeError::internal("No port settings for port in container"))?
        .as_ref()
        .ok_or_else(|| RuntimeError::internal("No port settings for port in container"))?
        .first()
        .ok_or_else(|| RuntimeError::internal("No port settings for port in container"))?
        .host_port
        .as_ref()
        .ok_or_else(|| RuntimeError::internal("No port settings for port in container"))?
        .parse()
        .map_err(|_| RuntimeError::internal("Port wasn't a number"))?;

    Ok(std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        serpentine_port,
    ))
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
async fn ensure_containerd_image(docker: &bollard::Docker) -> Result<Box<str>, RuntimeError> {
    let image_name = format!("{CONTAINERD_IMAGE}:{CONTAINERD_IMAGE_TAG}").into_boxed_str();

    if docker.inspect_image(&image_name).await.is_err() {
        log::info!("Pulling image {image_name}");
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
            .for_each(|_| async {})
            .await;
    }

    Ok(image_name)
}
