//! Handles moving files in and out of containers.
//!
//! (Layer exports are handled by `layers.rs`)

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine;
use containerd_client::services::v1 as containerd_services;
use futures_util::future::BoxFuture;
use miette::{Context, IntoDiagnostic};
use serpentine_internal::FileSystemEntryHeader;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use typed_path::{UnixPath, UnixPathBuf};

use super::{CONTAINERD_GC_ROOT_LABEL, SNAPSHOTTER, WithLease, container_config};
use crate::engine::cache::{CacheHash, CacheScope};
use crate::engine::filesystem::{FileSystem, FileSystemProvider};
use crate::engine::{BoxedReader, internal, sidecar_client};

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
    fn get_reader(&self) -> BoxFuture<'_, miette::Result<BoxedReader>> {
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

impl super::Client {
    /// Read the given file from the given mounts
    ///
    /// This intended for known good paths and should not be used for user specific files.
    ///
    /// This requires the file to be UTF-8.
    pub(super) async fn read_file(
        &self,
        mounts: impl IntoIterator<Item = containerd_client::types::Mount>,
        file: UnixPathBuf,
    ) -> miette::Result<String> {
        let mut file_system_stream = self.sidecar.export_files(mounts, file).await?;

        let FileSystemEntryHeader::File { .. } =
            serpentine_internal::read_postcard_frame(&mut file_system_stream)
                .await
                .into_diagnostic()
                .context("reading the file header")?
        else {
            return Err(internal("Expected file entry header"));
        };

        let mut passwd = String::new();
        file_system_stream
            .read_to_string(&mut passwd)
            .await
            .into_diagnostic()
            .context("reading the file contents")?;
        Ok(passwd)
    }

    /// Write the given file into the given mounts
    pub(super) async fn write_file(
        &self,
        mounts: impl IntoIterator<Item = containerd_client::types::Mount>,
        file: UnixPathBuf,
        content: &[u8],
    ) -> miette::Result<()> {
        let mut stream = self.sidecar.import_files(mounts, file).await?;

        let header = serpentine_internal::FileSystemEntryHeader::File {
            name: Box::default(),
            length: content.len() as u64,
        };
        serpentine_internal::write_postcard_frame(&header, &mut stream)
            .await
            .into_diagnostic()
            .context("writing the file header")?;
        stream
            .write_all(content)
            .await
            .into_diagnostic()
            .context("writing the file contents")?;

        Ok(())
    }

    /// Copy the given file/directory into the container
    pub async fn copy_fs_into_container(
        &self,
        state: &container_config::ContainerState,
        src: FileSystem,
        dest: &UnixPath,
    ) -> miette::Result<container_config::ContainerState> {
        let hash = CacheHash::from_data(CacheScope::WithInputs, &src).await?;
        let hash = CacheHash::from_data(CacheScope::WithInputs, &(&state, hash)).await?;
        let final_snapshot = base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(*hash);

        let lease = self.new_lease().await?;

        let dest = if dest.as_bytes() == b"." {
            UnixPath::new("")
        } else {
            dest
        };

        let snapshot = uuid::Uuid::new_v4().to_string();
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
            .await
            .into_diagnostic()
            .with_context(|| format!("preparing snapshot {snapshot}"))?
            .into_inner()
            .mounts;

        log::debug!("Copying filesystem into container at {dest}");
        let dest = state.config.working_dir.join(dest);

        let mut src = src.get_reader().await?;
        let mut dest = self.sidecar.import_files(mounts, dest).await?;
        tokio::io::copy(&mut src, &mut dest)
            .await
            .into_diagnostic()
            .context("copying the filesystem into the container")?;

        let commit = self
            .containerd
            .snapshot()
            .commit(containerd_services::snapshots::CommitSnapshotRequest {
                snapshotter: SNAPSHOTTER.to_owned(),
                name: final_snapshot.clone(),
                key: snapshot.clone(),
                labels: HashMap::from([(CONTAINERD_GC_ROOT_LABEL.to_owned(), "1".to_owned())]),
            })
            .await;
        if let Err(status) = commit
            && !Self::is_already_exists(&status)
        {
            return Err(status)
                .into_diagnostic()
                .with_context(|| format!("committing snapshot {snapshot}"));
        }
        self.drop_lease(lease).await?;

        Ok(container_config::ContainerState {
            snapshot: final_snapshot.into(),
            config: state.config.clone(),
        })
    }

    /// Export the given path from the container into a `FileSystem`
    pub async fn export_path(
        &self,
        state: &container_config::ContainerState,
        container_path: &UnixPath,
    ) -> miette::Result<FileSystem> {
        log::debug!("Creating file system provider for {state:?} at {container_path}");
        let snapshot = format!("{}/view/{}", state.snapshot, uuid::Uuid::new_v4());
        let container_path = if container_path.as_bytes() == b"." {
            UnixPath::new("")
        } else {
            container_path
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
            .await
            .into_diagnostic()
            .with_context(|| format!("viewing snapshot {}", state.snapshot))?
            .into_inner()
            .mounts;

        let container_path = state.config.working_dir.join(container_path);

        Ok(ContainerFileExport {
            sidecar: self.sidecar,
            mounts: mounts.into(),
            path: container_path,
        }
        .into())
    }
}

#[cfg(test)]
#[cfg(feature = "_test_docker")]
mod integration_tests {
    use rstest::rstest;
    use typed_path::UnixPath;

    use crate::engine::containerd::Client;
    use crate::engine::containerd::test_support::{TEST_IMAGE, containerd_client};

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn copy_file_between_containers(
        #[future] containerd_client: miette::Result<Client>,
    ) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let base = containerd_client.pull_image(TEST_IMAGE).await?;

        let from = containerd_client
            .exec(&base, "echo hello > /tmp/hello.txt".to_owned())
            .await?;

        let file = containerd_client
            .export_path(&from, UnixPath::new("/tmp/hello.txt"))
            .await?;

        let to = containerd_client
            .copy_fs_into_container(&base, file, UnixPath::new("nice.txt"))
            .await?;

        containerd_client.exec(&to, "ls".to_owned()).await?;

        containerd_client
            .exec(&to, "grep -q hello nice.txt || exit 1".to_owned())
            .await?;

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn copy_folder_between_containers(
        #[future] containerd_client: miette::Result<Client>,
    ) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let base = containerd_client.pull_image(TEST_IMAGE).await?;

        let from = containerd_client
            .exec(&base, "mkdir -p /tmp/foo/bar/baz".to_owned())
            .await?;

        let from = containerd_client
            .exec(&from, "echo hello > /tmp/foo/bar/baz/nice.txt".to_owned())
            .await?;

        let file = containerd_client
            .export_path(&from, UnixPath::new("/tmp/foo"))
            .await?;

        let to = containerd_client
            .copy_fs_into_container(&base, file, UnixPath::new("hello"))
            .await?;

        containerd_client
            .exec(&to, "ls hello/bar/baz".to_owned())
            .await?;

        containerd_client
            .exec(
                &to,
                "grep -q hello hello/bar/baz/nice.txt || exit 1".to_owned(),
            )
            .await?;

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn copy_folder_between_containers_relative_paths(
        #[future] containerd_client: miette::Result<Client>,
    ) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let base = containerd_client.pull_image(TEST_IMAGE).await?;
        let base = base.update_config(|config| config.set_working_dir(UnixPath::new("/testing")));

        let from = containerd_client
            .exec(&base, "mkdir -p ./foo/bar/baz".to_owned())
            .await?;

        let from = containerd_client
            .exec(&from, "echo hello > ./foo/bar/baz/nice.txt".to_owned())
            .await?;

        let file = containerd_client
            .export_path(&from, UnixPath::new("./foo"))
            .await?;

        let to = containerd_client
            .copy_fs_into_container(&base, file, UnixPath::new("./hello"))
            .await?;

        containerd_client
            .exec(&to, "ls ./hello/bar/baz".to_owned())
            .await?;

        containerd_client
            .exec(
                &to,
                "grep -q hello ./hello/bar/baz/nice.txt || exit 1".to_owned(),
            )
            .await?;

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn copy_folder_between_containers_relative_paths_dot(
        #[future] containerd_client: miette::Result<Client>,
    ) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let base = containerd_client.pull_image(TEST_IMAGE).await?;
        let base = base.update_config(|config| config.set_working_dir(UnixPath::new("/testing")));

        let from = containerd_client
            .exec(&base, "mkdir -p ./foo/bar/baz".to_owned())
            .await?;

        let from = containerd_client
            .exec(&from, "echo hello > ./foo/bar/baz/nice.txt".to_owned())
            .await?;

        let file = containerd_client
            .export_path(&from, UnixPath::new("."))
            .await?;

        let to = containerd_client
            .copy_fs_into_container(&base, file, UnixPath::new("."))
            .await?;

        containerd_client
            .exec(&to, "ls ./foo/bar/baz".to_owned())
            .await?;

        containerd_client
            .exec(
                &to,
                "grep -q hello ./foo/bar/baz/nice.txt || exit 1".to_owned(),
            )
            .await?;

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn export_path_not_found(
        #[future] containerd_client: miette::Result<Client>,
    ) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        let base = containerd_client.pull_image(TEST_IMAGE).await?;

        let fs = containerd_client
            .export_path(&base, UnixPath::new("i_am_not_real.txt"))
            .await?;

        let result = containerd_client
            .copy_fs_into_container(&base, fs, UnixPath::new("huh.txt"))
            .await;

        assert!(
            result.is_err(),
            "Expected reading non-existent path to fail"
        );

        Ok(())
    }
}
