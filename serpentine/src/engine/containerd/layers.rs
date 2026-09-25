//! Code responsible for pulling and exporting layers.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use containerd_client::services::v1 as containerd_services;
use futures_util::{Stream, StreamExt, TryStreamExt};
use miette::{Context, IntoDiagnostic};
use tokio::io::AsyncWriteExt;
use tokio_util::bytes::Bytes;
use tokio_util::io::ReaderStream;

use super::{CONTAINERD_GC_ROOT_LABEL, SNAPSHOTTER, WithLease, container_config};
use crate::engine::cache::{CacheHash, CacheScope};
use crate::engine::{WrapInternal, acquire_file_lock};
use crate::events::{TaskHandle, TaskId, TaskKind};

/// Label serpentine sets on pulled image layers to allow pulling them again from cache without it
/// counting as a pull.
const SERPENTINE_LAYER_DIGEST_LABEL: &str = "serpentine/manifest";

/// Return whether the given Oci platform object is compatible with the current system.
pub(super) fn platform_resolver(
    manifests: &[oci_client::manifest::ImageIndexEntry],
) -> Option<String> {
    manifests
        .iter()
        .find(|manifest| match &manifest.platform {
            None => false,
            Some(platform) => {
                platform.os == oci_client::config::Os::Linux
                    && platform.architecture == oci_client::config::Architecture::default()
            }
        })
        .map(|manifest| manifest.digest.clone())
}

/// postcard does not support skipping certain fields, like `oci_client`s Serialize does.
/// <https://github.com/jamesmunns/postcard/issues/125>
///
/// So instead we go via json
mod as_json {
    use std::marker::PhantomData;

    /// Serialize the given value as as json string
    pub fn serialize<S: serde::Serializer, T: serde::Serialize>(
        value: T,
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        let json_string = serde_json::to_string(&value).map_err(serde::ser::Error::custom)?;
        ser.serialize_str(&json_string)
    }

    /// A visitor that attempts to extra a string from the deserializer and parse it as json into
    /// the given type.
    struct JsonVisitor<T>(PhantomData<T>);

    impl<T: for<'json_de> serde::Deserialize<'json_de>> serde::de::Visitor<'_> for JsonVisitor<T> {
        type Value = T;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(formatter, "a string thats valid json for the given type")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            serde_json::from_str(value).map_err(serde::de::Error::custom)
        }

        fn visit_borrowed_str<E>(self, value: &'_ str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            serde_json::from_str(value).map_err(serde::de::Error::custom)
        }
    }

    /// deerialize a json string as a value
    pub fn deserialize<'de, D: serde::Deserializer<'de>, T: for<'any> serde::Deserialize<'any>>(
        de: D,
    ) -> Result<T, D::Error> {
        de.deserialize_string(JsonVisitor::<T>(PhantomData))
    }
}

/// A header for a snapshot cache entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SnapshotCacheEntryHeader {
    /// The parent if there is any
    parent: Option<String>,
    /// The kind of snapshot entry
    #[serde(with = "as_json")]
    entry_kind: SnapshotCacheEntryKind,
}

/// The kind of snapshot cache entries
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[expect(
    clippy::large_enum_variant,
    reason = "Only exists for a short time within one function"
)]
enum SnapshotCacheEntryKind {
    /// A local entry, the layer data exists after this header in the reader
    Local,
    /// The layer is stored at a remote location specified by the given `OciDescriptor`
    Remote(FullLayerManifest),
}

/// All the info needed to pull a specific layer.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FullLayerManifest {
    /// The image to pull from
    image: oci_client::Reference,
    /// The layer to pull
    layer: oci_client::manifest::OciDescriptor,
}

/// Optional extra data that `import_stream_into_content_store` might optionally take for known manifests.
///
/// Which can help it detect data corruption as well as provide better progress reporting.
///
/// NOTE: While technically its more "correct" for these fields to be `Option` (instead of the
/// function taking a `Option` of this), but the only two current call sites either provide all or
/// none of these fields.
#[derive(Clone, Debug)]
struct ImportStreamIntoContentStoreExtra {
    /// Th expected digest
    digest: String,
    /// The total amount of bytes to pull
    total_size: usize,
    /// The task id to report progress to
    task_id: TaskId,
}

impl super::Client {
    /// Check if a state and all its services exist.
    ///
    /// Will attempt to pull missing snapshots from the cache.
    pub async fn healthcheck_value(&self, config: &container_config::ContainerState) -> bool {
        if !self.ensure_snapshot(&config.snapshot).await {
            return false;
        }

        for service in config.config.services.values() {
            if !Box::pin(self.healthcheck_value(service)).await {
                return false;
            }
        }

        true
    }

    /// Checks if the snapshot exists in containerd and if not tries to import it from the cache.
    ///
    /// Returns whether the snapshot exists after this.
    async fn ensure_snapshot(&self, snapshot: &str) -> bool {
        log::debug!("Ensuring {snapshot} exists");

        let lock = acquire_file_lock(&format!("import/{snapshot}")).await;
        if let Err(err) = &lock {
            log::warn!("Proceeding without an import lock for {snapshot}: {err:?}");
        }

        if self
            .containerd
            .snapshot()
            .stat(containerd_services::snapshots::StatSnapshotRequest {
                snapshotter: SNAPSHOTTER.to_owned(),
                key: snapshot.into(),
            })
            .await
            .is_ok()
        {
            log::debug!("{snapshot} exists");
            drop(lock);
            true
        } else {
            let result = self.import_snapshot(snapshot).await;

            drop(lock);
            match result {
                Ok(imported) => imported,
                Err(err) => {
                    log::error!("Failed to import layer: {err:?}");
                    debug_assert!(false, "Failed to import layer: {err:?}");
                    false
                }
            }
        }
    }
    /// Download the given image and return a normal `ContainerState` representing it.
    pub async fn pull_image(
        &self,
        image_name: &str,
    ) -> miette::Result<container_config::ContainerState> {
        let (config, snapshot_name) = self.fetch_image(image_name).await?;

        let config = if let Some(config) = config.config {
            container_config::ContainerConfig::from(config)
        } else {
            container_config::ContainerConfig::default()
        };

        Ok(container_config::ContainerState {
            snapshot: snapshot_name.into(),
            config,
        })
    }

    /// Download the given image and return a `ServiceState` representing it.
    pub async fn pull_service(
        &self,
        image_name: &str,
    ) -> miette::Result<container_config::ServiceState> {
        let (config, snapshot_name) = self.fetch_image(image_name).await?;

        let (service_config, config) = if let Some(config) = config.config {
            (
                container_config::ServiceConfig::from(config.clone()),
                container_config::ContainerConfig::from(config),
            )
        } else {
            (
                container_config::ServiceConfig::default(),
                container_config::ContainerConfig::default(),
            )
        };

        Ok(container_config::ServiceState {
            container: container_config::ContainerState {
                snapshot: snapshot_name.into(),
                config,
            },
            service_config,
        })
    }

    /// Pull the given image from the registry and return both the snapshot name and config.
    async fn fetch_image(
        &self,
        image_name: &str,
    ) -> miette::Result<(oci_client::config::ConfigFile, String)> {
        let image = oci_client::Reference::try_from(image_name)
            .into_diagnostic()
            .with_context(|| format!("invalid image name {image_name:?}"))?;
        let auth = oci_client::secrets::RegistryAuth::Anonymous;

        log::debug!("Pulling image {image} manifest");
        let (manifest, manifest_digest, config) = self
            .oci
            .pull_manifest_and_config(&image, &auth)
            .await
            .into_diagnostic()
            .with_context(|| format!("pulling the manifest of {image}"))?;
        let pull_guard = acquire_file_lock(&manifest_digest).await?;
        let lease = self.new_lease().await?;

        log::debug!("Pulling image {image}");
        let snapshot_name = self
            .pull_all_snapshots_from_image(&image, image_name, manifest, &lease)
            .await?;
        self.drop_lease(lease).await?;

        pull_guard.unlock();

        let config: oci_client::config::ConfigFile = serde_json::from_str(&config)
            .into_diagnostic()
            .with_context(|| format!("parsing the image config of {image}"))?;

        Ok((config, snapshot_name))
    }

    /// Pull all snapshots from the given image
    ///
    /// Returns the root snapshot name.
    async fn pull_all_snapshots_from_image(
        &self,
        image: &oci_client::Reference,
        image_name: &str,
        manifest: oci_client::manifest::OciImageManifest,
        lease: &str,
    ) -> miette::Result<String> {
        let mut parent = String::new();
        let layer_count = manifest.layers.len();

        let task = self.reporter.start_task(TaskKind::Pull, image_name);
        self.reporter.task_layer_progress(task.id(), 0, layer_count);

        let mut layer_stack_hash = blake3::Hasher::new();

        for (index, layer) in manifest.layers.into_iter().enumerate() {
            layer_stack_hash.update(layer.digest.as_bytes());
            let snapshot_name = layer_stack_hash.finalize().to_hex().to_string();

            let pull_guard = acquire_file_lock(&snapshot_name).await?;

            let layer_exists = self
                .containerd
                .snapshot()
                .stat(containerd_services::snapshots::StatSnapshotRequest {
                    snapshotter: SNAPSHOTTER.to_owned(),
                    key: snapshot_name.clone(),
                })
                .await
                .is_ok();

            if layer_exists {
                log::debug!("Snapshot {snapshot_name} already exists.");
            } else {
                self.pull_layer(
                    FullLayerManifest {
                        image: image.clone(),
                        layer,
                    },
                    lease,
                    parent,
                    &task,
                    snapshot_name.clone(),
                )
                .await?;
            }

            pull_guard.unlock();

            self.reporter
                .task_layer_progress(task.id(), index.saturating_add(1), layer_count);
            parent = snapshot_name;
        }

        Ok(parent)
    }

    /// Export all the referenced snapshots from this config.
    pub async fn export_snapshots_from(&self, config: &container_config::ContainerState) {
        if let Err(err) = self.export_snapshot(&config.snapshot).await {
            log::error!("Failed to export snapshot: {err:?}");
        }

        for service in config.config.services.values() {
            Box::pin(self.export_snapshots_from(service)).await;
        }
    }

    /// Export the given snapshot to the caching backend
    async fn export_snapshot(&self, snapshot: &str) -> miette::Result<()> {
        let info = self
            .containerd
            .snapshot()
            .stat(containerd_services::snapshots::StatSnapshotRequest {
                snapshotter: SNAPSHOTTER.into(),
                key: snapshot.into(),
            })
            .await
            .into_diagnostic()
            .with_context(|| format!("stating snapshot {snapshot}"))?
            .into_inner()
            .info
            .wrap_internal("snapshot didnt have any info")?;

        let digest = info.labels.get(SERPENTINE_LAYER_DIGEST_LABEL);
        let parent = (!info.parent.is_empty()).then_some(info.parent);

        if let Some(parent) = parent.as_ref() {
            Box::pin(self.export_snapshot(parent))
                .await
                .with_context(|| format!("exporting parent of {snapshot}"))?;
        }

        log::debug!("Exporting snapshot {snapshot} to cache");

        let hash = CacheHash::from_data(CacheScope::Snapshot, snapshot).await?;
        let Some(mut writer) = self.cache.write_key(hash).await else {
            log::debug!("Snapshot {snapshot} already exists in cache");
            return Ok(());
        };

        if let Some(manifest) = digest {
            let manifest = serde_json::from_str(manifest)
                .into_diagnostic()
                .context("Reading oci descriptor from containerd label")?;
            let header = SnapshotCacheEntryHeader {
                parent,
                entry_kind: SnapshotCacheEntryKind::Remote(manifest),
            };
            log::debug!("Writing header: {header:?}");
            serpentine_internal::write_postcard_frame(&header, &mut writer)
                .await
                .into_diagnostic()
                .context("Writing header")?;
        } else {
            let task = self
                .reporter
                .start_task(TaskKind::Status, "exporting layer");

            let view_name = format!("{snapshot}/view/{}", uuid::Uuid::new_v4());
            let lease = self.new_lease().await?;

            let mounts = self
                .containerd
                .snapshot()
                .view(
                    containerd_services::snapshots::ViewSnapshotRequest {
                        snapshotter: SNAPSHOTTER.into(),
                        key: view_name,
                        parent: snapshot.into(),
                        labels: HashMap::new(),
                    }
                    .with_lease(&lease),
                )
                .await
                .into_diagnostic()
                .with_context(|| format!("viewing snapshot {snapshot}"))?
                .into_inner()
                .mounts;

            debug_assert!(
                mounts.len() == 1,
                "Expected overlayfs mounts to only have one mount returned"
            );
            let mount = mounts
                .into_iter()
                .next()
                .wrap_internal("No mounts returned for snapshoter")?;

            let header = SnapshotCacheEntryHeader {
                parent,
                entry_kind: SnapshotCacheEntryKind::Local,
            };
            log::debug!("Writing header: {header:?}");
            serpentine_internal::write_postcard_frame(&header, &mut writer)
                .await
                .into_diagnostic()
                .context("Writing header")?;

            let mut tar_stream = self.sidecar.export_layer(mount).await?;
            tokio::io::copy(&mut tar_stream, &mut writer)
                .await
                .into_diagnostic()
                .with_context(|| format!("writing snapshot {snapshot} to the cache"))?;

            self.drop_lease(lease).await?;
            drop(task);
        }

        writer
            .shutdown()
            .await
            .into_diagnostic()
            .context("flushing the snapshot to the cache")?;

        log::debug!("Finished exporting layer");

        Ok(())
    }

    /// Attempt to load the given snapshot from the cache backend.
    ///
    /// returns whether the snapshot was imported
    #[expect(clippy::too_many_lines, reason = "Tightly coupled linear task")]
    async fn import_snapshot(&self, snapshot: &str) -> miette::Result<bool> {
        log::debug!("Attempting to import {snapshot}");

        let hash = CacheHash::from_data(CacheScope::Snapshot, snapshot).await?;
        let Some(mut reader) = self.cache.read_key(hash).await else {
            log::debug!("Snapshot {snapshot} not in cache backend");
            return Ok(false);
        };

        let header: SnapshotCacheEntryHeader =
            serpentine_internal::read_postcard_frame(&mut reader)
                .await
                .into_diagnostic()
                .context("reading the header from the cache")?;
        log::debug!("Read header {header:?}");

        let download_parent = async {
            if let Some(parent) = header.parent.as_ref() {
                Box::pin(self.ensure_snapshot(parent)).await
            } else {
                true
            }
        };

        let lease = self.new_lease().await?;

        let task = self
            .reporter
            .start_task(TaskKind::Status, "importing layer");

        match header.entry_kind {
            SnapshotCacheEntryKind::Remote(manifest) => {
                let parent_found = download_parent.await;
                if !parent_found {
                    return Ok(false);
                }

                self.pull_layer(
                    manifest,
                    &lease,
                    header.parent.unwrap_or_default(),
                    &task,
                    snapshot.to_owned(),
                )
                .await?;
            }
            SnapshotCacheEntryKind::Local => {
                log::debug!("Importing {snapshot} into content store");
                let import_to_content_store = self.import_stream_into_content_store(
                    ReaderStream::new(reader).map(IntoDiagnostic::into_diagnostic),
                    &lease,
                    None,
                );

                let (parent_found, import_result) =
                    futures_util::join!(download_parent, import_to_content_store);
                let (total_size, digest) = import_result?;
                if !parent_found {
                    return Ok(false);
                }

                let temp_snapshot = uuid::Uuid::new_v4().to_string();
                log::debug!(
                    "Creating temporary snapshot {temp_snapshot} from {:?}",
                    header.parent
                );
                let mounts = self
                    .containerd
                    .snapshot()
                    .prepare(
                        containerd_services::snapshots::PrepareSnapshotRequest {
                            snapshotter: SNAPSHOTTER.into(),
                            key: temp_snapshot.clone(),
                            parent: header.parent.unwrap_or_default(),
                            labels: HashMap::new(),
                        }
                        .with_lease(&lease),
                    )
                    .await
                    .into_diagnostic()
                    .with_context(|| format!("preparing snapshot {temp_snapshot}"))?
                    .into_inner()
                    .mounts;

                log::debug!("Applying layer diff {digest} to {temp_snapshot}");
                let descriptor = containerd_client::types::Descriptor {
                    media_type: "application/vnd.oci.image.layer.v1.tar+zstd".into(),
                    digest,
                    size: total_size.try_into().unwrap_or(0),
                    annotations: HashMap::new(),
                };
                self.containerd
                    .diff()
                    .apply(containerd_services::ApplyRequest {
                        mounts,
                        diff: Some(descriptor),
                        payloads: HashMap::new(),
                        sync_fs: true,
                    })
                    .await
                    .into_diagnostic()
                    .with_context(|| format!("applying the layer diff to {temp_snapshot}"))?;
                log::debug!("Diff applied, committing snapshot to {snapshot}");
                self.containerd
                    .snapshot()
                    .commit(containerd_services::snapshots::CommitSnapshotRequest {
                        snapshotter: SNAPSHOTTER.into(),
                        key: temp_snapshot,
                        name: snapshot.to_owned(),
                        labels: HashMap::from([(
                            CONTAINERD_GC_ROOT_LABEL.to_owned(),
                            "1".to_owned(),
                        )]),
                    })
                    .await
                    .into_diagnostic()
                    .with_context(|| format!("committing snapshot {snapshot}"))?;
            }
        }

        self.drop_lease(lease).await?;
        drop(task);

        Ok(true)
    }

    /// Pull the given layer and convert into a snapshot under the given name
    async fn pull_layer(
        &self,
        manifest: FullLayerManifest,
        lease: &str,
        parent: String,
        task: &TaskHandle,
        snapshot_name: String,
    ) -> Result<(), miette::Error> {
        self.fetch_layer(&manifest, lease, task.id()).await?;
        let key = uuid::Uuid::new_v4().to_string();
        log::debug!("Applying layer {} to {key}", manifest.layer.digest);
        let mounts = self
            .containerd
            .snapshot()
            .prepare(
                containerd_services::snapshots::PrepareSnapshotRequest {
                    key: key.clone(),
                    snapshotter: SNAPSHOTTER.to_owned(),
                    labels: HashMap::new(),
                    parent,
                }
                .with_lease(lease),
            )
            .await
            .into_diagnostic()
            .with_context(|| format!("preparing snapshot {key}"))?
            .into_inner()
            .mounts;
        self.containerd
            .diff()
            .apply(containerd_services::ApplyRequest {
                diff: Some(containerd_client::types::Descriptor {
                    media_type: manifest.layer.media_type.clone(),
                    digest: manifest.layer.digest.clone(),
                    size: manifest.layer.size,
                    annotations: HashMap::new(),
                }),
                mounts: mounts.clone(),
                payloads: HashMap::new(),
                sync_fs: false,
            })
            .await
            .into_diagnostic()
            .with_context(|| format!("applying layer {} to {key}", manifest.layer.digest))?;
        log::debug!("Committing {key} to {snapshot_name}");
        let mut labels = HashMap::new();
        labels.insert(
            SERPENTINE_LAYER_DIGEST_LABEL.to_owned(),
            serde_json::to_string(&manifest)
                .wrap_internal("layer manifest could not be serialized to json")?,
        );
        labels.insert(CONTAINERD_GC_ROOT_LABEL.to_owned(), "1".to_owned());
        let commit = self
            .containerd
            .snapshot()
            .commit(
                containerd_services::snapshots::CommitSnapshotRequest {
                    snapshotter: SNAPSHOTTER.to_owned(),
                    name: snapshot_name,
                    key,
                    labels,
                }
                .with_lease(lease),
            )
            .await;

        if let Err(status) = commit
            && !Self::is_already_exists(&status)
        {
            return Err(status).into_diagnostic().context("committing snapshot");
        }

        Ok(())
    }

    /// Pull the given layer into containers content store.
    async fn fetch_layer(
        &self,
        manifest: &FullLayerManifest,
        lease: &str,
        task_id: TaskId,
    ) -> miette::Result<()> {
        if self
            .containerd
            .content()
            .read(containerd_services::ReadContentRequest {
                digest: manifest.layer.digest.clone(),
                offset: 0,
                size: 1,
            })
            .await
            .is_ok()
        {
            log::debug!("layer {} already exists", manifest.layer.digest);
            return Ok(());
        }

        log::debug!("Pulling layer {}", manifest.layer);
        self.oci
            .auth(
                &manifest.image,
                &oci_client::secrets::RegistryAuth::Anonymous,
                oci_client::RegistryOperation::Pull,
            )
            .await
            .into_diagnostic()
            .with_context(|| format!("authenticating to pull from {}", manifest.image))?;

        let layer_stream = self
            .oci
            .pull_blob_stream(&manifest.image, &manifest.layer)
            .await
            .into_diagnostic()
            .with_context(|| {
                format!(
                    "pulling layer {} of {}",
                    manifest.layer.digest, manifest.image
                )
            })?;

        let total_size: usize = layer_stream
            .content_length
            .and_then(|len| len.try_into().ok())
            .unwrap_or(0);
        let digest = manifest.layer.digest.clone();

        self.import_stream_into_content_store(
            layer_stream.map(IntoDiagnostic::into_diagnostic),
            lease,
            Some(ImportStreamIntoContentStoreExtra {
                digest,
                total_size,
                task_id,
            }),
        )
        .await?;

        Ok(())
    }

    /// Import data from the given reader into the content store (under the given lease.)
    ///
    /// returns the total size in bytes, as well as the digest.
    #[expect(clippy::too_many_lines, reason = "Tightly coupled linear task")]
    async fn import_stream_into_content_store(
        &self,
        stream: impl Stream<Item = miette::Result<Bytes>> + Send + 'static,
        lease: &str,
        extra: Option<ImportStreamIntoContentStoreExtra>,
    ) -> miette::Result<(usize, String)> {
        let upload_ref = uuid::Uuid::new_v4().to_string();
        let upload_ref_clone = upload_ref.clone();
        let current_offset = Arc::new(AtomicUsize::new(0));
        let current_offset_clone = Arc::clone(&current_offset);

        let reporter = self.reporter.clone();
        let extra_clone = extra.clone();

        let digest = self
            .containerd
            .content()
            .write(
                stream
                    .filter_map(async |layer_data| layer_data.ok())
                    .map(move |layer_data| {
                        let previous_offset =
                            current_offset_clone.fetch_add(layer_data.len(), Ordering::Relaxed);

                        if let Some(extra_clone) = extra_clone.as_ref() {
                            reporter.task_bytes(
                                extra_clone.task_id,
                                previous_offset as u64,
                                extra_clone.total_size as u64,
                            );
                        }

                        containerd_services::WriteContentRequest {
                            action: containerd_services::WriteAction::Write.into(),
                            r#ref: upload_ref_clone.clone(),
                            total: 0,
                            expected: String::new(),
                            offset: previous_offset.try_into().unwrap_or(0),
                            data: layer_data.to_vec(),
                            labels: HashMap::new(),
                        }
                    })
                    .with_lease(lease),
            )
            .await
            .into_diagnostic()
            .context("writing content to the containerd store")?
            .into_inner()
            .try_fold(String::new(), async |_acc, response| Ok(response.digest))
            .await
            .into_diagnostic()?;

        let total_size = current_offset.load(Ordering::Relaxed);

        log::debug!("Committing {total_size} bytes to the store.");
        let committed = self
            .containerd
            .content()
            .write(
                futures_util::stream::once({
                    async move {
                        containerd_services::WriteContentRequest {
                            action: containerd_services::WriteAction::Commit.into(),
                            r#ref: upload_ref,
                            total: total_size.try_into().unwrap_or(0),
                            expected: extra.map_or_default(|extra| extra.digest),
                            offset: (total_size).try_into().unwrap_or(0),
                            data: Vec::new(),
                            labels: HashMap::new(),
                        }
                    }
                })
                .with_lease(lease),
            )
            .await;

        match committed {
            Ok(response) => {
                let commit_digest = response
                    .into_inner()
                    .try_next()
                    .await
                    .into_diagnostic()
                    .context("committing content")?
                    .wrap_internal("No response for commit")?
                    .digest;

                debug_assert_eq!(
                    commit_digest, digest,
                    "Last write digest doesnt match commit digest"
                );

                Ok((total_size, digest))
            }
            // The store is content addressed, so this says the bytes are already there. They are
            // held by whichever lease stored them first, so ours is given a reference of its own.
            Err(status) if Self::is_already_exists(&status) => {
                log::warn!("Content {digest} ({total_size} bytes) was already in the store");
                self.containerd
                    .leases()
                    .add_resource(containerd_services::AddResourceRequest {
                        id: lease.to_owned(),
                        resource: Some(containerd_services::Resource {
                            id: digest.clone(),
                            r#type: "content".to_owned(),
                        }),
                    })
                    .await
                    .into_diagnostic()
                    .context("leasing already stored content")?;

                Ok((total_size, digest))
            }
            Err(status) => Err(status)
                .into_diagnostic()
                .with_context(|| format!("committing content {digest}")),
        }
    }
}

#[cfg(test)]
#[cfg(feature = "_test_docker")]
mod integration_tests {
    use std::sync::Arc;

    use miette::IntoDiagnostic;
    use rstest::rstest;
    use typed_path::PlatformPathBuf;

    use crate::engine::cache::{CacheBackend, LocalCacheBackend};
    use crate::engine::containerd::Client;
    use crate::engine::containerd::test_support::{TEST_IMAGE, containerd_client};
    use crate::events::Reporter;

    #[rstest]
    #[tokio::test]
    #[test_log::test]
    async fn pull_image(#[future] containerd_client: miette::Result<Client>) -> miette::Result<()> {
        let containerd_client = containerd_client.await?;
        containerd_client.pull_image(TEST_IMAGE).await?;

        Ok(())
    }

    #[tokio::test]
    #[test_log::test]
    async fn export_import_cache() -> miette::Result<()> {
        let caching_dir = tempfile::TempDir::new().into_diagnostic()?;
        let caching_dir = PlatformPathBuf::from(caching_dir.path().as_os_str().as_encoded_bytes());
        let cache = LocalCacheBackend::new(caching_dir)
            .await
            .into_diagnostic()?;
        let cache = Arc::new(cache) as Arc<dyn CacheBackend + Send + Sync>;

        let first_client = Client::new(
            Reporter::none(),
            Arc::clone(&cache),
            1,
            uuid::Uuid::new_v4().to_string(),
        )
        .await?;

        let image = first_client.pull_image(TEST_IMAGE).await?;
        let first_layer = first_client
            .exec(
                &image,
                String::from(
                    "
mkdir -p foo/bar &&
touch foo/bar/test1.txt &&
touch foo/bar/test2.txt &&
ln -s foo bar_sym &&
ln foo/bar/test1.txt test1.txt &&

mkdir -p mov_source &&
touch mov_source/test1.txt &&

mkdir -p copy_source &&
touch copy_source/test1.txt &&

mkdir -p whiteout &&
touch whiteout/test1.txt &&

mkdir -p opaque &&
touch opaque/test1.txt
",
                ),
            )
            .await?;

        let second_layer = first_client
            .exec(
                &first_layer,
                String::from(
                    "
ln -s foo parent_sym &&
ln foo/bar/test1.txt parent_hard &&
rm foo/bar/test2.txt &&

mv mov_source mov_parent &&
cp -r copy_source copy_parent &&
rm -r whiteout &&

rm -r opaque &&
mkdir -p opaque &&
touch opaque/test2.txt
",
                ),
            )
            .await?;

        let test_command = String::from(
            "
! cat foo/bar/test2.txt &&
cat foo/bar/test1.txt &&
cat bar_sym/bar/test1.txt &&
cat test1.txt &&
cat parent_sym/bar/test1.txt &&
cat parent_hard &&

! ls mov_source &&
cat mov_parent/test1.txt &&

cat copy_source/test1.txt &&
cat copy_parent/test1.txt &&

! ls whiteout &&

! cat opaque/test1.txt &&
cat opaque/test2.txt
",
        );

        first_client
            .exec(&second_layer, test_command.clone())
            .await?;

        first_client.export_snapshots_from(&second_layer).await;

        let second_client = Client::new(
            Reporter::none(),
            Arc::clone(&cache),
            1,
            uuid::Uuid::new_v4().to_string(),
        )
        .await?;

        second_client.healthcheck_value(&second_layer).await;
        second_client
            .exec(&second_layer, test_command.clone())
            .await?;

        Ok(())
    }
}
