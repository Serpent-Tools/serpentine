//! Wrapper around bollard Docker API client

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;

use bollard::API_DEFAULT_VERSION;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;

use crate::engine::RuntimeError;
use crate::engine::data_model::{FILE_SYSTEM_FILE_TAR_NAME, FileSystem};
use crate::tui::{Task, TaskProgress, TuiSender};

/// A container created by the Docker client
struct Container(Box<str>);

/// A reference to a specific state of a container.
#[derive(Clone, Hash, PartialEq, Eq, Debug, bincode::Encode, bincode::Decode)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub struct ContainerState(Rc<str>);

/// A docker client wrapper
pub struct Client {
    /// The underlying bollard Docker client
    docker: bollard::Docker,
    /// Cache of containers at the specified image state
    containers: Mutex<HashMap<ContainerState, Container>>,
    /// List of all containers created by this client, never removed from to ensure cleanup
    cleanup_list: Mutex<Vec<Container>>,
    /// Sender to the TUI
    tui: TuiSender,
    /// Limiter on the amount of exec jobs running at once
    exec_lock: tokio::sync::Semaphore,
}

impl Client {
    /// Create a new Docker client
    pub async fn new(tui: TuiSender, exec_permits: usize) -> Result<Self, RuntimeError> {
        let docker = Self::connect_docker()
            .await
            .map_err(|err| RuntimeError::DockerNotFound {
                inner: Box::new(err),
            })?;

        Ok(Self {
            docker,
            containers: Mutex::new(HashMap::new()),
            cleanup_list: Mutex::new(Vec::new()),
            tui,
            exec_lock: tokio::sync::Semaphore::new(exec_permits),
        })
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
                return Self::try_podman_connection();
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
                Self::try_podman_connection()
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
                RuntimeError::internal(
                    format!("Failed to parse podman socket path: {err}").as_str(),
                )
            })?
            .trim()
            .to_owned();

        let client =
            bollard::Docker::connect_with_socket(&podman_socket_path, 120, API_DEFAULT_VERSION)?;
        Ok(client)
    }

    /// Stop and remove all containers created by this client
    pub async fn shutdown(self) {
        todo!()
    }

    /// Check if a image exists
    pub async fn exists(&self, image: &ContainerState) -> bool {
        todo!()
    }

    /// Create a new container, download the image if necessary
    pub async fn pull_image(&self, image: &str) -> Result<ContainerState, RuntimeError> {
        todo!()
    }

    /// Commit the current state of a container and return a reference to it
    async fn commit_container(&self, container: Container) -> Result<ContainerState, RuntimeError> {
        todo!()
    }

    /// Get a container in the given state (image), creating it if necessary.
    /// This removes the container from the cache of containers at that given image,
    /// with the assumption that it's about to be modified.
    async fn get_state(&self, state: &ContainerState) -> Result<Container, RuntimeError> {
        todo!()
    }

    /// Stop the given container.
    ///
    /// This should be called when its known the container wont be used again.
    /// Or when the container required temporary exclusive access, but no semantic change.
    /// Its rare to fork on such operations, so as such we spin down the container.
    async fn stop_container(&self, container: Container) {
        todo!()
    }

    /// Execute a command in a container
    pub async fn exec(
        &self,
        container: &ContainerState,
        cmd: &[&str],
    ) -> Result<ContainerState, RuntimeError> {
        todo!()
    }

    /// Execute a command and its stdout and stderr.
    pub async fn exec_get_output(
        &self,
        container: &ContainerState,
        cmd: &[&str],
    ) -> Result<String, RuntimeError> {
        todo!()
    }

    /// Execute a command in a container directly without progress logging etc.
    /// For internal use
    async fn exec_internal(
        &self,
        container: &Container,
        cmd: &[&str],
    ) -> Result<String, RuntimeError> {
        todo!()
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

    async fn ping_client(#[future] containerd_client: Client) {
        containerd_client.await.docker.ping().await.unwrap();
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
        containerd_client.shutdown().await;
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
            .exec(&image, &["echo", "hello world"])
            .await
            .expect("Failed to exec in container");
        containerd_client.shutdown().await;
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

        let res = containerd_client.exec(&image, &["exit", "1"]).await;
        assert!(res.is_err(), "Expected exec to fail");

        containerd_client.shutdown().await;
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
            .exec(&image, &["touch", "/tmp/hello"])
            .await
            .expect("Exec failed");

        containerd_client
            .exec(&image, &["cat", "/tmp/hello"])
            .await
            .expect("Exec failed");

        containerd_client.shutdown().await;
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
            .exec(&image, &["touch", "/tmp/hello"])
            .await
            .expect("Exec failed");

        containerd_client
            .exec(&image, &["rm", "/tmp/hello"])
            .await
            .expect("Exec failed");

        containerd_client
            .exec(&image, &["cat", "/tmp/hello"])
            .await
            .expect("Exec failed");

        containerd_client.shutdown().await;
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
            .exec_get_output(&image, &["echo", "hello world"])
            .await
            .expect("Failed to exec in container");
        containerd_client.shutdown().await;

        assert_eq!(output, "hello world");
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
            .exec(&base, &["sh", "-c", "echo hello > /tmp/hello.txt"])
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
            .exec(&to, &["sh", "-c", "grep -q hello nice.txt || exit 1"])
            .await
            .expect("Exec failed");

        containerd_client.shutdown().await;
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
            .exec(&base, &["mkdir", "-p", "/tmp/foo/bar/baz"])
            .await
            .expect("Exec failed");

        let from = containerd_client
            .exec(
                &from,
                &["sh", "-c", "echo hello > /tmp/foo/bar/baz/nice.txt"],
            )
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
            .exec(&to, &["ls", "hello/bar/baz"])
            .await
            .expect("Exec failed");

        containerd_client
            .exec(
                &to,
                &["sh", "-c", "grep -q hello hello/bar/baz/nice.txt || exit 1"],
            )
            .await
            .expect("Exec failed");

        containerd_client.shutdown().await;
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
            .exec(&base, &["mkdir", "-p", "./foo/bar/baz"])
            .await
            .expect("Exec failed");

        let from = containerd_client
            .exec(&from, &["sh", "-c", "echo hello > ./foo/bar/baz/nice.txt"])
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
            .exec(&to, &["ls", "./hello/bar/baz"])
            .await
            .expect("Exec failed");

        containerd_client
            .exec(
                &to,
                &[
                    "sh",
                    "-c",
                    "grep -q hello ./hello/bar/baz/nice.txt || exit 1",
                ],
            )
            .await
            .expect("Exec failed");

        containerd_client.shutdown().await;
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

        containerd_client.shutdown().await;
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
            .exec(&image, &["mkdir", "-p", "/foo/bar"])
            .await
            .expect("Exec failed");
        let image = containerd_client
            .set_working_dir(&image, "/foo")
            .await
            .expect("Failed to set working dir");
        containerd_client
            .exec(&image, &["ls", "bar"])
            .await
            .expect("Exec failed");

        let working_dir_pwd = containerd_client
            .exec_get_output(&image, &["pwd"])
            .await
            .expect("Exec failed");
        assert_eq!(
            working_dir_pwd, "/foo",
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
            .exec(&image, &["sh", "-c", "echo $HELLO | grep -q WORLD"])
            .await
            .expect("Exec failed");

        containerd_client.shutdown().await;
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
    //         .exec(&image, &["touch", "hello.txt"])
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
    //         .exec(&image, &["cat", "hello.txt"])
    //         .await
    //         .expect("Failed to find file");
    //
    //     containerd_client.shutdown().await;
    // }

    // The following tests, against our general test policy, use the clients internal details.
    // This is because these tests are designed to test "external" interference.
    // which is simplest to do by just using the existing docker connection and internal data in
    // the client.
    //
    // Like yes public code shouldnt be able to grab the image name and mess with it, but external
    // programs can via the docker cli, so we find this justified.

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn external_interference_image_deleted(#[future] containerd_client: Client) {
        let containerd_client = containerd_client.await;
        let image = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let image = containerd_client
            .exec(&image, &["touch", "hello.txt"])
            .await
            .expect("Exec failed");

        log::debug!("Deleting {image:?}");
        containerd_client.delete_state(&image).await;

        containerd_client
            .exec(&image, &["cat", "hello.txt"])
            .await
            .expect("Exec failed");

        // Trigger forking (i.e we need to use the image)
        // The system should handle image deletion gracefully without panicking
        // whether it succeeds or fails is implementation detail
        // (At the time of writing this fails, and there isnt a clear recovery path to make it not)
        let _ = containerd_client.exec(&image, &["cat", "hello.txt"]).await;

        containerd_client.shutdown().await;
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn external_interference_container_deleted(#[future] containerd_client: Client) {
        let mut containerd_client = containerd_client.await;
        let image = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let image = containerd_client
            .exec(&image, &["touch", "hello.txt"])
            .await
            .expect("Exec failed");

        let container = containerd_client
            .containers
            .get_mut()
            .get(&image)
            .expect("container for image not found");
        log::debug!("Deleting {}", container.0);
        containerd_client
            .docker
            .remove_container(
                &container.0,
                Some(
                    bollard::query_parameters::RemoveContainerOptionsBuilder::new()
                        .force(true)
                        .build(),
                ),
            )
            .await
            .expect("Failed to remove container");

        containerd_client
            .exec(&image, &["cat", "hello.txt"])
            .await
            .expect("Exec failed");

        containerd_client.shutdown().await;
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn external_interference_container_stopped(#[future] containerd_client: Client) {
        let mut containerd_client = containerd_client.await;
        let image = containerd_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let image = containerd_client
            .exec(&image, &["touch", "hello.txt"])
            .await
            .expect("Exec failed");

        let container = containerd_client
            .containers
            .get_mut()
            .get(&image)
            .expect("container for image not found");
        log::debug!("Stopping {}", container.0);
        containerd_client
            .docker
            .stop_container(
                &container.0,
                Some(bollard::query_parameters::StopContainerOptionsBuilder::new().build()),
            )
            .await
            .expect("Failed to stop container");

        containerd_client
            .exec(&image, &["cat", "hello.txt"])
            .await
            .expect("Exec failed");

        containerd_client.shutdown().await;
    }
}
