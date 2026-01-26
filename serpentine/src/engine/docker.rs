//! Spin up containerd using the docker api.

use std::collections::HashMap;
use std::process::Command;

use bollard::API_DEFAULT_VERSION;
use futures_util::StreamExt;

use crate::engine::{RuntimeError, sidecar_client};

/// The name of the containerd daemon serpetnine spawns.
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

/// Create a new containerd client, by either connecting to a esisting container or spinning up a
/// new one.
pub async fn connect() -> Result<sidecar_client::Client, RuntimeError> {
    let docker = connect_docker()
        .await
        .map_err(|err| RuntimeError::DockerNotFound {
            inner: Box::new(err),
        })?;

    let containerd_addr = spin_up_containerd(docker).await?;
    Ok(sidecar_client::Client::new(containerd_addr))
}

/// Attempt to connect to docker
async fn connect_docker() -> Result<bollard::Docker, RuntimeError> {
    log::info!("Connecting to Docker daemon");
    log::debug!("DOCKER_HOST={:?}", std::env::var("DOCKER_HOST"));
    let client = match bollard::Docker::connect_with_defaults() {
        Ok(client) => client,
        Err(bollard::errors::Error::SocketNotFoundError(err)) => {
            // Fallback to podman
            log::info!("Docker socket {err} not found, trying podman");
            return try_podman_connection();
        }
        Err(err) => return Err(err.into()),
    };

    match client.ping().await {
        Ok(_) => {
            log::info!("Docker connection successful");
            Ok(client)
        }
        Err(err) => {
            // Connection worked but ping failed (permission denied, daemon down, etc.)
            log::warn!("Docker ping failed: {err}, trying podman");
            try_podman_connection()
        }
    }
}

/// utility function to find podman socket and connect to it
fn try_podman_connection() -> Result<bollard::Docker, RuntimeError> {
    let podman_socket_output = Command::new("podman")
        .args(["info", "--format", "{{.Host.RemoteSocket.Path}}"])
        .output()?;

    let podman_socket_path = String::from_utf8(podman_socket_output.stdout)
        .map_err(|err| {
            RuntimeError::internal(format!("Failed to parse podman socket path: {err}").as_str())
        })?
        .trim()
        .to_owned();

    let client =
        bollard::Docker::connect_with_socket(&podman_socket_path, 120, API_DEFAULT_VERSION)?;
    Ok(client)
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
                bollard::secret::ContainerCreateBody {
                    image: Some(image.into_string()),
                    tty: Some(false),
                    open_stdin: Some(false),
                    host_config: Some(bollard::secret::HostConfig {
                        auto_remove: Some(true),
                        privileged: Some(true),
                        binds: Some(vec![format!("{volume}:/var/lib/containerd")]),
                        log_config: Some(bollard::secret::HostConfigLogConfig {
                            typ: Some("json-file".to_owned()),
                            config: None,
                        }),
                        port_bindings: Some(HashMap::from([(
                            "8000/tcp".to_owned(),
                            Some(vec![bollard::secret::PortBinding {
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
        .get("8000/tcp")
        .ok_or_else(|| RuntimeError::internal("No port settings for port 8000 in container"))?
        .as_ref()
        .ok_or_else(|| RuntimeError::internal("No port settings for port 8000 in container"))?
        .first()
        .ok_or_else(|| RuntimeError::internal("No port settings for port 8000 in container"))?
        .host_port
        .as_ref()
        .ok_or_else(|| RuntimeError::internal("No port settings for port 8000 in container"))?
        .parse()
        .map_err(|_| RuntimeError::internal("Port wasnt a number"))?;

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
            .create_volume(bollard::secret::VolumeCreateOptions {
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
