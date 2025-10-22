//! Wrapper around bollard Docker API client

use std::{cell::RefCell, collections::HashMap, process::Command, rc::Rc};

use bollard::API_DEFAULT_VERSION;
use futures_util::TryStreamExt;
use tokio::io::AsyncBufReadExt;

use crate::engine::RuntimeError;

/// A docker client wrapper
pub struct DockerClient {
    /// The underlying bollard Docker client
    client: bollard::Docker,
    /// Cache of containers at the current image state
    containers: RefCell<HashMap<ContainerState, Container>>,
    /// List of all containers created by this client, never removed from to ensure cleanup
    cleanup_list: RefCell<Vec<Container>>,
}

impl DockerClient {
    /// Create a new Docker client
    pub async fn new() -> Result<Self, RuntimeError> {
        log::info!("Connecting to Docker daemon");

        let client = match bollard::Docker::connect_with_defaults() {
            Ok(client) => client,
            Err(bollard::errors::Error::SocketNotFoundError(_)) => {
                // Fallback to podman
                log::info!("Docker socket not found, trying podman");
                return Self::try_podman_connection();
            }
            Err(e) => return Err(e.into()),
        };

        match client.ping().await {
            Ok(_) => {
                log::info!("Docker connection successful");
                Ok(Self {
                    client,
                    containers: RefCell::new(HashMap::new()),
                    cleanup_list: RefCell::new(Vec::new()),
                })
            }
            Err(e) => {
                // Connection worked but ping failed (permission denied, daemon down, etc.)
                log::warn!("Docker ping failed: {}, trying podman", e);
                return Self::try_podman_connection();
            }
        }
    }

    fn try_podman_connection() -> Result<Self, RuntimeError> {
        let podman_socket_output = Command::new("podman")
            .args(&["info", "--format", "{{.Host.RemoteSocket.Path}}"])
            .output()?;

        let podman_socket_path = String::from_utf8(podman_socket_output.stdout)
            .expect("Failed to parse podman socket path")
            .trim()
            .to_string();

        let client =
            bollard::Docker::connect_with_socket(&podman_socket_path, 120, API_DEFAULT_VERSION)?;

        Ok(Self {
            client,
            containers: RefCell::new(HashMap::new()),
            cleanup_list: RefCell::new(Vec::new()),
        })
    }

    /// Stop and remove all containers created by this client
    pub async fn shutdown(&self) {
        for container in self.cleanup_list.take() {
            log::debug!("Stopping and removing container {}", container.0);

            if let Err(err) = self
                .client
                .remove_container(
                    &container.0,
                    Some(
                        bollard::query_parameters::RemoveContainerOptionsBuilder::new()
                            .force(true)
                            .build(),
                    ),
                )
                .await
            {
                log::warn!("Failed to remove container {}: {err}", container.0);
            }
        }
    }

    /// Create a new container, download the image if necessary
    pub async fn pull_image(&self, image: &str) -> Result<ContainerState, RuntimeError> {
        log::trace!("Checking if image {image} exists");
        let exists = self.client.inspect_image(image).await.is_ok();

        if exists {
            log::trace!("Image {image} already exists, reusing");
            Ok(ContainerState(Rc::from(image)))
        } else {
            log::info!("Pulling image {image}");
            self.client
                .create_image(
                    Some(
                        bollard::query_parameters::CreateImageOptionsBuilder::new()
                            .from_image(image)
                            .build(),
                    ),
                    None,
                    None,
                )
                .try_collect::<Vec<_>>()
                .await?;

            Ok(ContainerState(Rc::from(image)))
        }
    }

    /// Commit the current state of a container and return a reference to it
    async fn commit_container(&self, container: Container) -> Result<ContainerState, RuntimeError> {
        let id = self
            .client
            .commit_container(
                bollard::query_parameters::CommitContainerOptionsBuilder::new()
                    .container(&container.0)
                    .repo("serpentine-worker-commit")
                    .tag(uuid::Uuid::new_v4().to_string().as_str())
                    .build(),
                bollard::secret::ContainerConfig::default(),
            )
            .await?
            .id;

        log::trace!("Committed container {} to image {}", container.0, id);

        let state = ContainerState(Rc::from(id));
        self.containers
            .borrow_mut()
            .insert(state.clone(), container);
        Ok(state)
    }

    /// Get a container in the given state (image), creating it if necessary.
    /// This removes the container from the cache of containers at that given image,
    /// with the assumption that its about to be modified.
    async fn get_state(&self, state: &ContainerState) -> Result<Container, RuntimeError> {
        if let Some(container) = self.containers.borrow_mut().remove(state) {
            log::trace!("Reusing existing container {}", container.0);
            Ok(container)
        } else {
            log::trace!("Creating container from image {}", state.0);
            let id = self
                .client
                .create_container(
                    Some(
                        bollard::query_parameters::CreateContainerOptionsBuilder::new()
                            .name(&format!("serpentine-worker-{}", uuid::Uuid::new_v4()))
                            .build(),
                    ),
                    bollard::secret::ContainerCreateBody {
                        image: Some(state.0.to_string()),
                        cmd: Some(vec!["sleep".into(), "infinity".into()]),
                        tty: Some(false),
                        open_stdin: Some(false),
                        host_config: Some(bollard::secret::HostConfig {
                            init: Some(true),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )
                .await?
                .id;

            self.cleanup_list
                .borrow_mut()
                .push(Container(id.clone().into_boxed_str()));

            log::trace!("Starting container {id}");
            self.client
                .start_container(
                    &id,
                    Some(bollard::query_parameters::StartContainerOptionsBuilder::new().build()),
                )
                .await?;

            let container = Container(id.into_boxed_str());
            Ok(container)
        }
    }

    /// Execute a command in a container
    pub async fn exec(
        &self,
        container: &ContainerState,
        cmd: &[&str],
    ) -> Result<ContainerState, RuntimeError> {
        log::debug!("Executing command {:?} in image {}", cmd, container.0);
        let container = self.get_state(container).await?;

        let exec = self
            .client
            .create_exec(
                &container.0,
                bollard::exec::CreateExecOptions {
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    cmd: Some(cmd.iter().map(ToString::to_string).collect()),
                    ..Default::default()
                },
            )
            .await?;

        let res = self.client.start_exec(&exec.id, None).await?;
        match res {
            bollard::exec::StartExecResults::Detached => {
                return Err(RuntimeError::internal(
                    "Exec detached, this should not happen",
                ));
            }
            bollard::exec::StartExecResults::Attached { output, .. } => {
                let output = tokio_util::io::StreamReader::new(
                    output
                        .map_err(|err| {
                            if let bollard::errors::Error::IOError { err } = err {
                                err
                            } else {
                                std::io::Error::other(err)
                            }
                        })
                        .try_filter_map(|msg| match msg {
                            bollard::container::LogOutput::StdErr { message }
                            | bollard::container::LogOutput::StdOut { message }
                            | bollard::container::LogOutput::Console { message } => {
                                futures_util::future::ok(Some(message))
                            }
                            bollard::container::LogOutput::StdIn { .. } => {
                                futures_util::future::ok(None)
                            }
                        }),
                );
                let mut output = tokio::io::BufReader::new(output).lines();

                while let Some(line) = output.next_line().await? {
                    log::trace!(
                        "{} ({}): {}",
                        cmd.first().unwrap_or(&"<unknown>"),
                        container.0.get(0..12).unwrap_or("<invalid>"),
                        line
                    );
                }
            }
        }

        let exec_info = self.client.inspect_exec(&exec.id).await?;
        if let Some(code) = exec_info.exit_code {
            if code != 0 {
                return Err(RuntimeError::CommandExecution {
                    code,
                    command: cmd.iter().map(ToString::to_string).collect(),
                });
            }
        } else {
            return Err(RuntimeError::internal(
                "Exec exit code is None, this should not happen",
            ));
        }

        self.commit_container(container).await
    }

    /// Copy the given directory from the host into the container
    pub async fn copy_dir_into_container(
        &self,
        container: &ContainerState,
        src: &str,
        dest: &str,
    ) -> Result<ContainerState, RuntimeError> {
        log::debug!(
            "Copying directory {} into container {} at {}",
            src,
            container.0,
            dest
        );
        let container = self.get_state(container).await?;

        let tar_data = {
            let mut tar_data = Vec::new();
            let paths = ignore::WalkBuilder::new(src)
                .hidden(false)
                .git_ignore(true)
                .git_exclude(true)
                .git_global(true)
                .build()
                .filter_map(std::result::Result::ok);

            {
                let mut tar_builder = tar::Builder::new(&mut tar_data);

                for entry in paths {
                    let path = entry.path();
                    let relative_path = path.strip_prefix(src).unwrap_or(path);

                    if relative_path.to_string_lossy().is_empty() {
                        continue;
                    }

                    if path.is_file() {
                        tar_builder.append_path_with_name(path, relative_path)?;
                    } else if path.is_dir() {
                        let mut header = tar::Header::new_gnu();
                        header.set_path(relative_path)?;
                        header.set_entry_type(tar::EntryType::Directory);
                        header.set_mode(0o755);
                        header.set_size(0);
                        header.set_cksum();
                        tar_builder.append(&header, std::io::empty())?;
                    }
                }

                tar_builder.finish()?;
            }
            tar_data
        };

        self.client
            .upload_to_container(
                &container.0,
                Some(
                    bollard::query_parameters::UploadToContainerOptionsBuilder::new()
                        .path(dest)
                        .build(),
                ),
                bollard::body_full(tar_data.into()),
            )
            .await?;

        self.commit_container(container).await
    }

    /// Set the working directory of the container
    pub async fn set_working_dir(
        &self,
        image: &ContainerState,
        dir: &str,
    ) -> Result<ContainerState, RuntimeError> {
        log::debug!(
            "Setting working directory of container {} to {}",
            image.0,
            dir
        );
        let container = self.get_state(image).await?;

        let new_image = self
            .client
            .commit_container(
                bollard::query_parameters::CommitContainerOptionsBuilder::new()
                    .container(&container.0)
                    .repo("serpentine-worker-commit")
                    .tag(uuid::Uuid::new_v4().to_string().as_str())
                    .build(),
                bollard::secret::ContainerConfig {
                    working_dir: Some(dir.to_owned()),
                    ..Default::default()
                },
            )
            .await?
            .id;

        // Put the container back in the cache, as we didnt modify it
        self.containers
            .borrow_mut()
            .insert(image.clone(), container);

        Ok(ContainerState(Rc::from(new_image)))
    }
}

impl Drop for DockerClient {
    fn drop(&mut self) {
        let containers = std::mem::take(&mut *self.cleanup_list.borrow_mut());
        if !containers.is_empty() {
            log::warn!(
                "DockerClient dropped without calling shutdown(), leaving {} containers running",
                containers.len()
            );
        }
    }
}

/// A container created by the Docker client
struct Container(Box<str>);

/// A reference to a specific state of a container.
#[derive(Clone, Hash, PartialEq, Eq, Debug)]
pub struct ContainerState(Rc<str>);

#[cfg(test)]
#[expect(clippy::expect_used, reason = "Tests")]
mod tests {
    use super::*;
    use rstest::{fixture, rstest};

    const TEST_IMAGE: &str = "alpine:latest";

    #[fixture]
    async fn docker_client() -> DockerClient {
        DockerClient::new()
            .await
            .expect("Failed to create Docker client")
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    #[cfg_attr(not(docker_available), ignore = "Docker host not available")]
    async fn ping_client(#[future] docker_client: DockerClient) {
        docker_client.await.client.ping().await.unwrap();
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    #[cfg_attr(not(docker_available), ignore = "Docker host not available")]
    async fn pull_image(#[future] docker_client: DockerClient) {
        let docker_client = docker_client.await;
        docker_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        docker_client.shutdown().await;
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    #[cfg_attr(not(docker_available), ignore = "Docker host not available")]
    async fn exec_in_container(#[future] docker_client: DockerClient) {
        let docker_client = docker_client.await;
        let image = docker_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        docker_client
            .exec(&image, &["echo", "hello world"])
            .await
            .expect("Failed to exec in container");
        docker_client.shutdown().await;
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    #[cfg_attr(not(docker_available), ignore = "Docker host not available")]
    async fn exec_in_container_fail(#[future] docker_client: DockerClient) {
        let docker_client = docker_client.await;
        let image = docker_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");

        let res = docker_client.exec(&image, &["exit", "1"]).await;
        assert!(res.is_err(), "Expected exec to fail");

        docker_client.shutdown().await;
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    #[cfg_attr(not(docker_available), ignore = "Docker host not available")]
    async fn chained_exec(#[future] docker_client: DockerClient) {
        let docker_client = docker_client.await;
        let image = docker_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");

        let image = docker_client
            .exec(&image, &["touch", "/tmp/hello"])
            .await
            .expect("Exec failed");

        docker_client
            .exec(&image, &["cat", "/tmp/hello"])
            .await
            .expect("Exec failed");

        docker_client.shutdown().await;
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    #[cfg_attr(not(docker_available), ignore = "Docker host not available")]
    async fn forked_image(#[future] docker_client: DockerClient) {
        let docker_client = docker_client.await;
        let image = docker_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");

        let image = docker_client
            .exec(&image, &["touch", "/tmp/hello"])
            .await
            .expect("Exec failed");

        docker_client
            .exec(&image, &["rm", "/tmp/hello"])
            .await
            .expect("Exec failed");

        docker_client
            .exec(&image, &["cat", "/tmp/hello"])
            .await
            .expect("Exec failed");

        docker_client.shutdown().await;
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    #[cfg_attr(not(docker_available), ignore = "Docker host not available")]
    async fn copy_dir_into_container(#[future] docker_client: DockerClient) {
        let docker_client = docker_client.await;
        let image = docker_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let image = docker_client
            .copy_dir_into_container(&image, "./test_cases", "/data")
            .await
            .expect("Failed to copy dir into container");

        docker_client
            .exec(&image, &["ls", "/data"])
            .await
            .expect("Exec failed");
        docker_client
            .exec(&image, &["ls", "/data/positive"])
            .await
            .expect("Exec failed");

        docker_client.shutdown().await;
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    #[cfg_attr(not(docker_available), ignore = "Docker host not available")]
    async fn set_working_dir(#[future] docker_client: DockerClient) {
        let docker_client = docker_client.await;
        let image = docker_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let image = docker_client
            .exec(&image, &["mkdir", "-p", "/foo/bar"])
            .await
            .expect("Exec failed");
        let image = docker_client
            .set_working_dir(&image, "/foo")
            .await
            .expect("Failed to set working dir");
        docker_client
            .exec(&image, &["ls", "bar"])
            .await
            .expect("Exec failed");

        docker_client.shutdown().await;
    }
}
