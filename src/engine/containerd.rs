//! Wrapper around bollard Docker API client

use std::collections::HashMap;
use std::rc::Rc;

use containerd_client::services::v1 as containerd_services;
use futures_util::{StreamExt, TryStreamExt};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::engine::RuntimeError;
use crate::engine::data_model::FileSystem;
use crate::tui::TuiSender;

/// The snapshoter to use for containers.
const SNAPSHOTER: &str = "overlayfs";

/// A container created by the client
#[derive(Debug)]
struct Container(Box<str>);

/// A reference to a specific state of a container.
#[derive(Clone, Hash, PartialEq, Eq, Debug, bincode::Encode, bincode::Decode)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub struct ContainerState(Rc<str>);

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
}

/// A docker client wrapper
pub struct Client {
    /// containerd client
    containerd: ContainerdRootClient,
    /// Container registery client
    oci: oci_client::Client,
    /// Sender to the TUI
    tui: TuiSender,
    /// Limiter on the amount of exec jobs running at once
    exec_lock: tokio::sync::Semaphore,
}

impl Client {
    /// Create a new Docker client
    pub async fn new(tui: TuiSender, exec_permits: usize) -> Result<Self, RuntimeError> {
        let oci = oci_client::Client::new(oci_client::client::ClientConfig {
            user_agent: concat!("serpentine/", env!("CARGO_PKG_VERSION")),
            platform_resolver: Some(Box::new(platform_resolver)),
            ..Default::default()
        });

        Ok(Self {
            containerd: ContainerdRootClient(crate::engine::docker::connect().await?),
            oci,
            tui,
            exec_lock: tokio::sync::Semaphore::new(exec_permits),
        })
    }

    /// Check if a state exists
    pub async fn exists(&self, snapshot: &ContainerState) -> bool {
        self.containerd
            .snapshot()
            .stat(containerd_services::snapshots::StatSnapshotRequest {
                snapshotter: SNAPSHOTER.to_owned(),
                key: (*snapshot.0).to_owned(),
            })
            .await
            .is_ok()
    }

    /// download the given image if necessary
    pub async fn pull_image(&self, image: &str) -> Result<ContainerState, RuntimeError> {
        let image = oci_client::Reference::try_from(image)?;
        let auth = oci_client::secrets::RegistryAuth::Anonymous;

        let final_container_state = ContainerState(image.to_string().into());
        if self.exists(&final_container_state).await {
            log::debug!("image {image} already exists");
            return Ok(final_container_state);
        }

        log::debug!("Pulling image {image}");
        let (manifest, _) = self.oci.pull_image_manifest(&image, &auth).await?;

        futures_util::future::try_join_all(
            manifest
                .layers
                .iter()
                .map(|layer| self.pull_layer(&image, layer)),
        )
        .await?;

        log::debug!("Uploaded all layers to containerd");
        log::debug!("Creating snapshot from image");

        let key = uuid::Uuid::new_v4().to_string();
        let mounts = self
            .containerd
            .snapshot()
            .prepare(containerd_services::snapshots::PrepareSnapshotRequest {
                key: key.clone(),
                snapshotter: SNAPSHOTER.to_owned(),
                labels: HashMap::new(),
                parent: String::new(),
            })
            .await?
            .into_inner()
            .mounts;
        for layer in manifest.layers {
            log::debug!("Applying layer {}", layer.digest);

            self.containerd
                .diff()
                .apply(containerd_services::ApplyRequest {
                    diff: Some(containerd_client::types::Descriptor {
                        media_type: layer.media_type,
                        digest: layer.digest,
                        size: layer.size,
                        annotations: HashMap::new(),
                    }),
                    mounts: mounts.clone(),
                    payloads: HashMap::new(),
                    sync_fs: false,
                })
                .await?;
        }

        log::debug!("Commiting {key} to {image}");
        self.containerd
            .snapshot()
            .commit(containerd_services::snapshots::CommitSnapshotRequest {
                name: image.to_string(),
                key,
                labels: HashMap::new(),
                snapshotter: SNAPSHOTER.to_owned(),
            })
            .await?;

        log::debug!("Created snapshot {image}");
        Ok(final_container_state)
    }

    /// Pull the given layer into containerd.
    async fn pull_layer(
        &self,
        image: &oci_client::Reference,
        layer: &oci_client::manifest::OciDescriptor,
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
                    }),
            )
            .await?
            .into_inner()
            .try_for_each(async |_| Ok(()))
            .await?;

        log::debug!("Finished pulling {digest_clone}.");
        self.containerd
            .content()
            .write(futures_util::stream::iter(std::iter::once(
                containerd_services::WriteContentRequest {
                    action: containerd_services::WriteAction::Commit.into(),
                    r#ref: upload_ref_clone,
                    total: total_size,
                    expected: digest_clone,
                    offset: total_size,
                    data: Vec::new(),
                    labels: HashMap::new(),
                },
            )))
            .await?
            .into_inner()
            .try_for_each(async |_| Ok(()))
            .await?;

        Ok::<_, RuntimeError>(())
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
            .exec(&image, &["echo", "hello world"])
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

        let res = containerd_client.exec(&image, &["exit", "1"]).await;
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
            .exec(&image, &["touch", "/tmp/hello"])
            .await
            .expect("Exec failed");

        containerd_client
            .exec(&image, &["cat", "/tmp/hello"])
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
    // }
}
