//! Wrapper around bollard Docker API client

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;

use bollard::API_DEFAULT_VERSION;
use futures_util::{StreamExt, TryStreamExt};
use tokio::io::AsyncBufReadExt;
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
pub struct DockerClient {
    /// The underlying bollard Docker client
    client: bollard::Docker,
    /// Cache of containers at the specified image state
    containers: Mutex<HashMap<ContainerState, Container>>,
    /// List of all containers created by this client, never removed from to ensure cleanup
    cleanup_list: Mutex<Vec<Container>>,
    /// Sender to the TUI
    tui: TuiSender,
    /// Limiter on the amount of exec jobs running at once
    exec_lock: tokio::sync::Semaphore,
}

impl DockerClient {
    /// Create a new Docker client
    pub async fn new(tui: TuiSender, exec_permits: usize) -> Result<Self, RuntimeError> {
        let client = Self::connect_docker()
            .await
            .map_err(|err| RuntimeError::DockerNotFound {
                inner: Box::new(err),
            })?;

        Ok(Self {
            client,
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
        let Self {
            client,
            cleanup_list,
            ..
        } = self;
        let client = &client;

        let mut futures = Vec::new();

        for container in cleanup_list.into_inner() {
            log::debug!("Stopping and removing container {}", container.0);

            let future = async move {
                client
                    .remove_container(
                        &container.0,
                        Some(
                            bollard::query_parameters::RemoveContainerOptionsBuilder::new()
                                .force(true)
                                .build(),
                        ),
                    )
                    .await
            };
            futures.push(future);
        }

        futures_util::stream::iter(futures.into_iter())
            .for_each_concurrent(None, |res| async {
                if let Err(err) = res.await {
                    log::error!("Error removing container: {err}");
                }
            })
            .await;
    }

    /// Check if a image exists
    pub async fn exists(&self, image: &ContainerState) -> bool {
        self.client.inspect_image(&image.0).await.is_ok()
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

            let task_id: Arc<str> = Arc::from(format!("pull-{image}"));
            let task_title: Arc<str> = Arc::from(format!("docker pull {image}"));

            let mut create_image_options_builder =
                bollard::query_parameters::CreateImageOptionsBuilder::new().from_image(image);
            if !image.contains(':') {
                // If not set causes docker to pull all tags of image
                create_image_options_builder = create_image_options_builder.tag("latest");
            }

            let mut stream =
                self.client
                    .create_image(Some(create_image_options_builder.build()), None, None);

            while let Some(status) = stream.try_next().await? {
                if let Some(status) = status.status {
                    log::trace!("{image}: {status}");
                    self.tui.send(crate::tui::TuiMessage::UpdateTask(Task {
                        identifier: Arc::clone(&task_id),
                        title: Arc::clone(&task_title),
                        progress: TaskProgress::Log(status.into_boxed_str()),
                    }));
                }
            }

            self.tui.send(crate::tui::TuiMessage::FinishTask(task_id));

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
                    .pause(false)
                    .build(),
                bollard::secret::ContainerConfig::default(),
            )
            .await?
            .id;

        log::trace!("Committed container {} to image {}", container.0, id);

        let state = ContainerState(Rc::from(id));
        self.containers
            .lock()
            .await
            .insert(state.clone(), container);
        Ok(state)
    }

    /// Get a container in the given state (image), creating it if necessary.
    /// This removes the container from the cache of containers at that given image,
    /// with the assumption that it's about to be modified.
    async fn get_state(&self, state: &ContainerState) -> Result<Container, RuntimeError> {
        if let Some(container) = self.containers.lock().await.remove(state) {
            log::trace!("Reusing existing container {}", container.0);

            if let Ok(status) = self
                .client
                .inspect_container(
                    &container.0,
                    Some(bollard::query_parameters::InspectContainerOptionsBuilder::new().build()),
                )
                .await
            {
                if let Some(bollard::secret::ContainerState {
                    status: Some(bollard::secret::ContainerStateStatusEnum::RUNNING),
                    ..
                }) = status.state
                {
                    return Ok(container);
                }

                log::error!("Container {} stopped externally, re-starting", container.0);
                let res = self
                    .client
                    .start_container(
                        &container.0,
                        Some(
                            bollard::query_parameters::StartContainerOptionsBuilder::new().build(),
                        ),
                    )
                    .await;

                if res.is_ok() {
                    return Ok(container);
                }

                log::error!("Failed to start container, re-creating");
            } else {
                log::error!("Container {} deleted externally, re-creating", container.0);
            }
        }

        log::trace!("Creating container from image {}", state.0);
        let name = format!("serpentine-worker-{}", uuid::Uuid::new_v4());
        let id = self
            .client
            .create_container(
                Some(
                    bollard::query_parameters::CreateContainerOptionsBuilder::new()
                        .name(&name)
                        .build(),
                ),
                bollard::secret::ContainerCreateBody {
                    image: Some(state.0.to_string()),
                    cmd: Some(vec!["/bin/sleep".into(), "infinity".into()]),
                    tty: Some(false),
                    open_stdin: Some(false),
                    host_config: Some(bollard::secret::HostConfig {
                        init: Some(true),
                        privileged: Some(true),
                        auto_remove: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await?
            .id;

        self.cleanup_list
            .lock()
            .await
            .push(Container(id.clone().into_boxed_str()));

        log::trace!("Starting container {id}");
        self.tui.send(crate::tui::TuiMessage::Container(
            id.clone().into_boxed_str(),
        ));
        self.client
            .start_container(
                &id,
                Some(bollard::query_parameters::StartContainerOptionsBuilder::new().build()),
            )
            .await?;

        let container = Container(id.into_boxed_str());
        Ok(container)
    }

    /// Stop the given container.
    ///
    /// This should be called when its known the container wont be used again.
    /// Or when the container required temporary exclusive access, but no semantic change.
    /// Its rare to fork on such operations, so as such we spin down the container.
    async fn stop_container(&self, container: Container) {
        log::debug!("Stopping {}", container.0);
        let _ = self
            .client
            .stop_container(
                &container.0,
                Some(
                    bollard::query_parameters::StopContainerOptionsBuilder::new()
                        .t(1)
                        .build(),
                ),
            )
            .await;

        self.tui
            .send(crate::tui::TuiMessage::StopContainer(container.0));
    }

    /// Execute a command in a container
    pub async fn exec(
        &self,
        container: &ContainerState,
        cmd: &[&str],
    ) -> Result<ContainerState, RuntimeError> {
        let container = self.get_state(container).await?;

        let lock = self
            .exec_lock
            .acquire()
            .await
            .map_err(|_| RuntimeError::internal("Failed to acquire lock"));
        self.exec_internal(&container, cmd).await?;
        drop(lock);

        let image = self.commit_container(container).await?;
        Ok(image)
    }

    /// Execute a command and its stdout and stderr.
    pub async fn exec_get_output(
        &self,
        container: &ContainerState,
        cmd: &[&str],
    ) -> Result<String, RuntimeError> {
        let container = self.get_state(container).await?;

        let lock = self
            .exec_lock
            .acquire()
            .await
            .map_err(|_| RuntimeError::internal("Failed to acquire lock"));
        let output = self.exec_internal(&container, cmd).await?;
        drop(lock);

        self.stop_container(container).await;

        Ok(output)
    }

    /// Execute a command in a container directly without progress logging etc.
    /// For internal use
    async fn exec_internal(
        &self,
        container: &Container,
        cmd: &[&str],
    ) -> Result<String, RuntimeError> {
        log::debug!("Executing command {:?} in container {}", cmd, container.0);
        let task_title: Arc<str> = Arc::from(cmd.join(" "));
        let task_id: Arc<str> = Arc::from(format!("docker-exec-{}-{}", container.0, task_title));

        self.tui.send(crate::tui::TuiMessage::UpdateTask(Task {
            identifier: Arc::clone(&task_id),
            title: Arc::clone(&task_title),
            progress: TaskProgress::Log(Box::from("")),
        }));

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
        let mut all_output = Vec::new();
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
                    let line = strip_ansi_escapes::strip_str(line);

                    log::trace!(
                        "{} ({}): {}",
                        cmd.first().unwrap_or(&"<unknown>"),
                        container.0.get(0..12).unwrap_or("<invalid>"),
                        line
                    );

                    self.tui.send(crate::tui::TuiMessage::UpdateTask(Task {
                        identifier: Arc::clone(&task_id),
                        title: Arc::clone(&task_title),
                        progress: TaskProgress::Log(line.clone().into_boxed_str()),
                    }));

                    all_output.push(line);
                }
            }
        }

        let exec_info = self.client.inspect_exec(&exec.id).await?;

        if let Some(code) = exec_info.exit_code {
            if code != 0 {
                return Err(RuntimeError::CommandExecution {
                    code,
                    command: cmd.iter().map(ToString::to_string).collect(),
                    output: all_output.join("\n"),
                });
            }
        } else {
            return Err(RuntimeError::internal(
                "Exec exit code is None, this should not happen",
            ));
        }

        self.tui.send(crate::tui::TuiMessage::FinishTask(task_id));

        Ok(all_output.join("\n"))
    }

    /// Copy the given file/directory into the container
    pub async fn copy_fs_into_container(
        &self,
        container: &ContainerState,
        src: FileSystem,
        dest: &str,
    ) -> Result<ContainerState, RuntimeError> {
        const FILE_TEMP_DIR: &str = "/tmp/serpentine/";

        log::debug!("Copying into container {} at {}", container.0, dest);
        let task_id: Arc<str> = Arc::from(format!("docker-copy-{}-to-{dest}", container.0));
        let task_title: Arc<str> = Arc::from(format!("cp -r <input> {dest}"));
        let container = self.get_state(container).await?;

        self.tui.send(crate::tui::TuiMessage::UpdateTask(Task {
            identifier: Arc::clone(&task_id),
            title: Arc::clone(&task_title),
            progress: TaskProgress::Log(Box::from("Uploading to container")),
        }));

        match src {
            FileSystem::File(tar_data) => {
                self.exec_internal(&container, &["/bin/mkdir", "-p", FILE_TEMP_DIR])
                    .await?;

                self.client
                    .upload_to_container(
                        &container.0,
                        Some(
                            bollard::query_parameters::UploadToContainerOptionsBuilder::new()
                                .path(FILE_TEMP_DIR)
                                .build(),
                        ),
                        bollard::body_full(tar_data.as_ref().to_vec().into()),
                    )
                    .await?;

                self.tui.send(crate::tui::TuiMessage::UpdateTask(Task {
                    identifier: Arc::clone(&task_id),
                    title: Arc::clone(&task_title),
                    progress: TaskProgress::Log(Box::from("Copying to destination")),
                }));
                self.exec_internal(
                    &container,
                    &[
                        "/bin/mv",
                        &format!("{FILE_TEMP_DIR}/{FILE_SYSTEM_FILE_TAR_NAME}"),
                        dest,
                    ],
                )
                .await?;
            }
            FileSystem::Folder(tar_data) => {
                log::debug!("Is folder, uploading directly");
                self.exec_internal(&container, &["/bin/mkdir", "-p", dest])
                    .await?;
                self.client
                    .upload_to_container(
                        &container.0,
                        Some(
                            bollard::query_parameters::UploadToContainerOptionsBuilder::new()
                                .path(dest)
                                .build(),
                        ),
                        bollard::body_full(tar_data.as_ref().to_vec().into()),
                    )
                    .await?;
            }
        }

        self.tui.send(crate::tui::TuiMessage::FinishTask(task_id));

        self.commit_container(container).await
    }

    /// Export the given path from the container into a `FileSystem`
    pub async fn export_path(
        &self,
        image: &ContainerState,
        docker_path: &str,
    ) -> Result<FileSystem, RuntimeError> {
        log::debug!("Exporting {docker_path} from {}", image.0);

        let container = self.get_state(image).await?;

        let docker_path = if docker_path.starts_with('/') {
            PathBuf::from(docker_path)
        } else {
            // Docker doesnt handle relative paths in `download_from_container` (podman does).
            let working_dir = self
                .client
                .inspect_container(
                    &container.0,
                    Some(bollard::query_parameters::InspectContainerOptionsBuilder::new().build()),
                )
                .await?
                .config
                .and_then(|config| config.working_dir)
                .unwrap_or_else(|| "/".into());

            PathBuf::from(working_dir).join(docker_path)
        };

        let docker_tar = self
            .client
            .download_from_container(
                &container.0,
                Some(
                    bollard::query_parameters::DownloadFromContainerOptionsBuilder::new()
                        .path(&docker_path.to_string_lossy())
                        .build(),
                ),
            )
            .map(|item| {
                Ok::<_, bollard::errors::Error>(futures_util::stream::iter(
                    item?
                        .into_iter()
                        .map(Result::<_, bollard::errors::Error>::Ok),
                ))
            })
            .try_flatten()
            .try_collect::<Vec<u8>>()
            .await?;
        self.stop_container(container).await;

        // We need to construct a new archive to match what `FileSystem` expects.
        // Docker returns a tar containing the path as an entry.
        // I.e `/the_folder` or `/the_file.txt`
        // But `FileSystem` expects single files to be named after `FILE_SYSTEM_FILE_TAR_NAME`,
        // And folders to be directly at the root.
        //
        // In other words:
        // | Path           | Docker                                 | `FileSystem`                   |
        // | -------------- | -------------------------------------- | ------------------------------ |
        // | `/my_folder`   | `/my_folder/a.txt`, `/my_folder/b.txt` | `/a.txt`, `/b.txt`             |
        // | `/my_file.txt` | `/my_file.txt`                         | `/{FILE_SYSTEM_FILE_TAR_NAME}` |
        let mut archive = tar::Archive::new(docker_tar.as_slice());
        let mut entries = archive.entries()?;

        let first_entry = entries
            .next()
            .ok_or_else(|| RuntimeError::internal("Docker returned an empty archive."))??;
        let is_file = first_entry.header().entry_type() == tar::EntryType::Regular;

        let tar_data = Vec::with_capacity(docker_tar.len());
        let mut builder = tar::Builder::new(tar_data);

        if is_file {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(first_entry.header().mode().unwrap_or(0o755));
            header.set_cksum();
            header.set_size(first_entry.header().size().unwrap_or(0));
            builder.append_data(&mut header, FILE_SYSTEM_FILE_TAR_NAME, first_entry)?;

            Ok(FileSystem::File(builder.into_inner()?.into()))
        } else {
            let dot_path = std::ffi::OsString::from(".");
            let suffix: &std::ffi::OsStr = docker_path
                .components()
                .next_back()
                .map_or(&dot_path, |comp| comp.as_os_str());

            // We skip the first entry because its the `docker_path` folder itself
            // Which we want to remove.
            for entry in entries {
                let entry = entry?;
                let entry_path = entry.path()?;

                #[expect(
                    clippy::map_unwrap_or,
                    reason = "For borrowing reasons we have to do this in two calls"
                )]
                let entry_path = entry_path
                    .strip_prefix(suffix)
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|_| entry_path.into_owned());

                let mut header = entry.header().clone();
                header.set_mtime(0);
                builder.append_data(&mut header, entry_path, entry)?;
            }
            Ok(FileSystem::Folder(builder.into_inner()?.into()))
        }
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
                    .pause(false)
                    .build(),
                bollard::secret::ContainerConfig {
                    working_dir: Some(dir.to_owned()),
                    ..Default::default()
                },
            )
            .await?
            .id;

        self.stop_container(container).await;

        Ok(ContainerState(Rc::from(new_image)))
    }

    /// Set a environment variable in the container
    pub async fn set_env_var(
        &self,
        image: &ContainerState,
        env: &str,
        value: &str,
    ) -> Result<ContainerState, RuntimeError> {
        log::debug!(
            "Setting env variable of container {} to {}={}",
            image.0,
            env,
            value
        );
        let container = self.get_state(image).await?;

        let new_image = self
            .client
            .commit_container(
                bollard::query_parameters::CommitContainerOptionsBuilder::new()
                    .container(&container.0)
                    .repo("serpentine-worker-commit")
                    .tag(uuid::Uuid::new_v4().to_string().as_str())
                    .pause(false)
                    .build(),
                bollard::secret::ContainerConfig {
                    env: Some(vec![format!("{env}={value}")]),
                    ..Default::default()
                },
            )
            .await?
            .id;

        self.stop_container(container).await;

        Ok(ContainerState(Rc::from(new_image)))
    }

    /// Delete a image.
    /// This should only be used during cleanup.
    ///
    /// This ignores error (just logs them), as its intended for cleanup.
    pub async fn delete_image(&self, image: &ContainerState) {
        log::debug!("Deleting image {image:?}");

        let result = self
            .client
            .remove_image(
                &image.0,
                Some(
                    bollard::query_parameters::RemoveImageOptionsBuilder::new()
                        .force(true)
                        .build(),
                ),
                None,
            )
            .await;
        if let Err(err) = result {
            log::error!("Failed to delete image {image:?}");
            log::error!("{err}");
        }
    }

    /// Export the given images to the target file
    pub async fn export(
        &self,
        images: impl Iterator<Item = &ContainerState>,
        mut target: impl std::io::Write,
    ) -> Result<(), RuntimeError> {
        self.client
            .export_images(&images.map(|image| image.0.as_ref()).collect::<Vec<_>>())
            .try_for_each(move |item| {
                let result = target.write_all(&item);
                std::future::ready(result.map_err(Into::into))
            })
            .await?;

        Ok(())
    }

    /// Import the given docker export to this docker client.
    pub async fn import(
        &self,
        mut file: impl std::io::Read + Send + 'static,
    ) -> Result<(), RuntimeError> {
        self.client
            .import_image_stream(
                bollard::query_parameters::ImportImageOptionsBuilder::new()
                    .quiet(true)
                    .build(),
                futures_util::stream::poll_fn(move |_| {
                    let mut buffer = [0; 2048];
                    match file.read(&mut buffer) {
                        Ok(bytes_read) => {
                            if bytes_read == 0 {
                                std::task::Poll::Ready(None)
                            } else {
                                std::task::Poll::Ready(Some(
                                    Vec::from(buffer.get(..bytes_read).unwrap_or(&[])).into(),
                                ))
                            }
                        }
                        Err(err) => {
                            log::error!("{err}");
                            std::task::Poll::Ready(None)
                        }
                    }
                }),
                None,
            )
            .try_collect::<Vec<_>>()
            .await?;

        Ok(())
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
    async fn docker_client() -> DockerClient {
        DockerClient::new(TuiSender(None), 1)
            .await
            .expect("Failed to create Docker client")
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn ping_client(#[future] docker_client: DockerClient) {
        docker_client.await.client.ping().await.unwrap();
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
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
    async fn exec_output(#[future] docker_client: DockerClient) {
        let docker_client = docker_client.await;
        let image = docker_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let output = docker_client
            .exec_get_output(&image, &["echo", "hello world"])
            .await
            .expect("Failed to exec in container");
        docker_client.shutdown().await;

        assert_eq!(output, "hello world");
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn copy_file_between_containers(#[future] docker_client: DockerClient) {
        let docker_client = docker_client.await;
        let base = docker_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");

        let from = docker_client
            .exec(&base, &["sh", "-c", "echo hello > /tmp/hello.txt"])
            .await
            .expect("Exec failed");

        let file = docker_client
            .export_path(&from, "/tmp/hello.txt")
            .await
            .expect("Export failed");

        let to = docker_client
            .copy_fs_into_container(&base, file, "nice.txt")
            .await
            .expect("Failed to copy into container");

        docker_client
            .exec(&to, &["sh", "-c", "grep -q hello nice.txt || exit 1"])
            .await
            .expect("Exec failed");

        docker_client.shutdown().await;
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn copy_folder_between_containers(#[future] docker_client: DockerClient) {
        let docker_client = docker_client.await;
        let base = docker_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");

        let from = docker_client
            .exec(&base, &["mkdir", "-p", "/tmp/foo/bar/baz"])
            .await
            .expect("Exec failed");

        let from = docker_client
            .exec(
                &from,
                &["sh", "-c", "echo hello > /tmp/foo/bar/baz/nice.txt"],
            )
            .await
            .expect("Exec failed");

        let file = docker_client
            .export_path(&from, "/tmp/foo")
            .await
            .expect("Export failed");

        let to = docker_client
            .copy_fs_into_container(&base, file, "hello")
            .await
            .expect("Failed to copy into container");

        docker_client
            .exec(&to, &["ls", "hello/bar/baz"])
            .await
            .expect("Exec failed");

        docker_client
            .exec(
                &to,
                &["sh", "-c", "grep -q hello hello/bar/baz/nice.txt || exit 1"],
            )
            .await
            .expect("Exec failed");

        docker_client.shutdown().await;
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn export_path_not_found(#[future] docker_client: DockerClient) {
        let docker_client = docker_client.await;
        let base = docker_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");

        let result = docker_client.export_path(&base, "i_am_not_real.txt").await;
        assert!(
            result.is_err(),
            "Expected reading non-existent path to fail"
        );

        docker_client.shutdown().await;
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
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

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn set_env_var(#[future] docker_client: DockerClient) {
        let docker_client = docker_client.await;
        let image = docker_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let image = docker_client
            .set_env_var(&image, "HELLO", "WORLD")
            .await
            .expect("Failed to set env");
        docker_client
            .exec(&image, &["sh", "-c", "echo $HELLO | grep -q WORLD"])
            .await
            .expect("Exec failed");

        docker_client.shutdown().await;
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn export_import(#[future] docker_client: DockerClient) {
        let docker_client = docker_client.await;
        let image = docker_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let image = docker_client
            .exec(&image, &["touch", "hello.txt"])
            .await
            .expect("Failed to create file");

        let mut data = Vec::new();
        docker_client
            .export(std::iter::once(&image), &mut data)
            .await
            .expect("Failed to export");
        docker_client.delete_image(&image).await;
        docker_client
            .import(std::io::Cursor::new(data))
            .await
            .expect("Failed to import");

        docker_client
            .exec(&image, &["cat", "hello.txt"])
            .await
            .expect("Failed to find file");

        docker_client.shutdown().await;
    }

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
    async fn external_interference_image_deleted(#[future] docker_client: DockerClient) {
        let docker_client = docker_client.await;
        let image = docker_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let image = docker_client
            .exec(&image, &["touch", "hello.txt"])
            .await
            .expect("Exec failed");

        log::debug!("Deleting {image:?}");
        docker_client.delete_image(&image).await;

        docker_client
            .exec(&image, &["cat", "hello.txt"])
            .await
            .expect("Exec failed");

        // Trigger forking (i.e we need to use the image)
        // The system should handle image deletion gracefully without panicking
        // whether it succeeds or fails is implementation detail
        // (At the time of writing this fails, and there isnt a clear recovery path to make it not)
        let _ = docker_client.exec(&image, &["cat", "hello.txt"]).await;

        docker_client.shutdown().await;
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn external_interference_container_deleted(#[future] docker_client: DockerClient) {
        let mut docker_client = docker_client.await;
        let image = docker_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let image = docker_client
            .exec(&image, &["touch", "hello.txt"])
            .await
            .expect("Exec failed");

        let container = docker_client
            .containers
            .get_mut()
            .get(&image)
            .expect("container for image not found");
        log::debug!("Deleting {}", container.0);
        docker_client
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
            .expect("Failed to remove container");

        docker_client
            .exec(&image, &["cat", "hello.txt"])
            .await
            .expect("Exec failed");

        docker_client.shutdown().await;
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn external_interference_container_stopped(#[future] docker_client: DockerClient) {
        let mut docker_client = docker_client.await;
        let image = docker_client
            .pull_image(TEST_IMAGE)
            .await
            .expect("Failed to create image");
        let image = docker_client
            .exec(&image, &["touch", "hello.txt"])
            .await
            .expect("Exec failed");

        let container = docker_client
            .containers
            .get_mut()
            .get(&image)
            .expect("container for image not found");
        log::debug!("Stopping {}", container.0);
        docker_client
            .client
            .stop_container(
                &container.0,
                Some(bollard::query_parameters::StopContainerOptionsBuilder::new().build()),
            )
            .await
            .expect("Failed to stop container");

        docker_client
            .exec(&image, &["cat", "hello.txt"])
            .await
            .expect("Exec failed");

        docker_client.shutdown().await;
    }
}
