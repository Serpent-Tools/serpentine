//! A content addressable cache.

use base64::Engine;
use futures_util::future::BoxFuture;
use miette::{Context, IntoDiagnostic};

use crate::engine::data_model::{Data, NodeKindId};
use crate::engine::{BoxedReader, BoxedWriter};

mod filesystem_backend;
mod github_backend;

pub use filesystem_backend::LocalCacheBackend;
pub use github_backend::GithubActionsBackend;

/// Version number for the cache.
///
/// This is not purely the version of the cache struct, but an indicator of the caches validity in
/// general. As such any changes to serpentine that can cause the cache to be invalid must
/// increment this version number, changes that do not don't have to.
///
/// In general the following changes require modifying the version number:
/// * Modifying the cache structure
/// * Modifying a builtin node in a way that causes changes to the output.
/// * Adding or removing builtin nodes as this can shift the node kind ids.
/// * Modifying insertion order of builtin nodes.
/// * Changes to how `FileSystem` works
///
/// The following changes do not require incrementing this number:
/// * Changes to the stdlib (even breaking), as the cache sits on a lower level than it.
/// * Changes to builtin node names.
/// * Changes to the cli
/// * Etc...
pub const CACHE_COMPATIBILITY_VERSION: u8 = 7;

/// Wrapper around the raw blake3 hash output as its trait implementations (`Hash` and `Eq`) use
/// constant time functions, which we do not require
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CacheHash([u8; blake3::OUT_LEN]);

impl std::fmt::Debug for CacheHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hash = base64::prelude::BASE64_STANDARD_NO_PAD.encode(self.0);
        f.write_str(&hash)?;
        Ok(())
    }
}

impl std::ops::Deref for CacheHash {
    type Target = [u8; blake3::OUT_LEN];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

// This must only call one `write` method (and only ones <= 64 bits).
// https://docs.rs/nohash/latest/nohash/trait.IsEnabled.html
//
// This function does this by just taking the first 8 bytes of the hash.
// This is okay because they are as evenly distributed in isolation as the whole hash.
// And secondly because the `HashMap` will compare the full hashes anyway in the unlikely event of
// a collision.
impl std::hash::Hash for CacheHash {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        const U64_BYTES: usize = 64 / 8;
        static_assertions::const_assert!(U64_BYTES <= blake3::OUT_LEN);

        let first_bytes = self.0.first_chunk::<U64_BYTES>().unwrap_or_else(|| {
            debug_assert!(
                false,
                "blake3 hash ({} bytes), not big enough to construct u64 ({U64_BYTES} bytes)",
                blake3::OUT_LEN
            );

            // NOTE: this is still safe as it will only degrade the hashmap lookup to O(n)
            &[0; U64_BYTES]
        });

        state.write_u64(u64::from_le_bytes(*first_bytes));
    }
}

impl nohash::IsEnabled for CacheHash {}

/// A trait similar to `Hash`, but required to be stable across runs, and unique across values.
pub trait ContentHash {
    /// Hash the content of this value into the given hasher.
    async fn content_hash(&self, hasher: &mut blake3::Hasher) -> miette::Result<()>;
}

impl<T: serde::Serialize + ?Sized> ContentHash for T {
    fn content_hash(
        &self,
        hasher: &mut blake3::Hasher,
    ) -> impl Future<Output = miette::Result<()>> {
        match postcard::to_stdvec(self) {
            Ok(bytes) => {
                hasher.update(&bytes);
                std::future::ready(Ok(()))
            }
            Err(err) => std::future::ready(
                Err(err)
                    .into_diagnostic()
                    .context("hashing a value for the cache"),
            ),
        }
    }
}

/// The kind of cache key this is, used to avoid collisions between different types of cache keys with the same data.
#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum CacheScope {
    /// `Data`
    Data,
    /// A containerd snapshot
    Snapshot,
    /// Hash of the inputs to containerd exec
    ExecInputs,
    /// Hash of the inputs to containerd With
    WithInputs,
}

impl CacheHash {
    /// Hash the given data, prefixing it with its type id, so that different types with the same data do not collide.
    pub async fn from_data<T: ContentHash + ?Sized>(
        scope: CacheScope,
        data: &T,
    ) -> miette::Result<Self> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&[scope as u8]);

        data.content_hash(&mut hasher).await?;

        let hash = hasher.finalize();
        Ok(Self(hash.into()))
    }
}

/// A trait for interfacing with a caching backend, like local filestorage or github actions cache.
pub trait CacheBackend {
    /// Read the given key from the cache backend, returning a reader for the data.
    /// This must return one of the values written to this key using `write_key`, or `None`.
    ///
    /// Returns `None` if the key does not exist in the cache backend.
    fn read_key(&self, key: CacheHash) -> BoxFuture<'_, Option<BoxedReader>>;

    /// Write the given key to the cache backend, returning a writer for the data.
    ///
    /// Returns `None` if the key already exists in the cache backend, and thus should not be written to.
    /// (It is not a requirement to return `None` when the key already exists, but its highly
    /// engouraged.)
    fn write_key(&self, key: CacheHash) -> BoxFuture<'_, Option<BoxedWriter>>;
}

static_assertions::assert_obj_safe!(CacheBackend);

/// A cache backend that does not store anything.
pub struct NoneCacheBackend;

impl CacheBackend for NoneCacheBackend {
    fn read_key(&self, _key: CacheHash) -> BoxFuture<'_, Option<BoxedReader>> {
        Box::pin(std::future::ready(None))
    }

    fn write_key(&self, _key: CacheHash) -> BoxFuture<'_, Option<BoxedWriter>> {
        Box::pin(std::future::ready(None))
    }
}

/// A key into the cache
#[derive(Debug)]
pub struct CacheKey<'caller> {
    /// The kind of node
    pub node: NodeKindId,
    /// The inputs to the node
    pub inputs: &'caller [Data],
}

impl ContentHash for CacheKey<'_> {
    async fn content_hash(&self, hasher: &mut blake3::Hasher) -> miette::Result<()> {
        hasher.update(&self.node.index().to_le_bytes());
        hasher.update(&(self.inputs.len() as u64).to_le_bytes());

        for input in self.inputs {
            input.content_hash(hasher).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests")]
mod tests {
    use std::time::Duration;

    use rstest::rstest;
    use typed_path::UnixPath;

    use super::*;
    use crate::engine::containerd::{ContainerConfig, ContainerState, ServiceState};
    use crate::engine::data_model::CacheableData;
    use crate::engine::filesystem;

    /// Generate tests for the given cache backend.
    ///
    /// These tests assume a "well-behaved" cache, which is a subset of valid implementations of
    /// `CacheBackend`.
    /// For example while the `NoneCacheBackend` is a valid implementation, it would
    /// fail these tests.
    /// A well-behaved cache is defined as:
    /// * `read_key` from a unwritten key returns `None` (This should be true for all valid
    ///   implementations of a `CacheBackend` unless they can magically translate a opaque hash to the
    ///   expected bytes)
    /// * `.write_key` followed by `.read_key` returns the same data.
    /// * `.write_key`, followed by `.write_key` returns None.
    ///
    /// It is assumed that the backend is fully empty when these tests start executing, but it is
    /// tolerate that multiple tests share the same cache storage (they all use different hashes).
    #[macro_export]
    macro_rules! test_well_behaved_cache {
        ($init:expr) => {
            use ::tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

            #[tokio::test]
            #[test_log::test]
            async fn test_cant_read_undefined() {
                let backend = $init;

                let result = backend
                    .read_key($crate::engine::cache::CacheHash([0; _]))
                    .await;
                assert!(result.is_none(), "Expected unwritten key to be None");
            }

            #[tokio::test]
            #[test_log::test]
            async fn test_read_write_key() {
                const TEST_STRING: &str = "integration testing for life!";

                let backend = $init;

                let mut writer = backend
                    .write_key($crate::engine::cache::CacheHash([1; _]))
                    .await
                    .expect("Expected to be able to write to key 1");
                writer
                    .write_all(TEST_STRING.as_bytes())
                    .await
                    .expect("Failed to write");
                writer.shutdown().await.expect("Failed to close writer");

                // ensure backends have time to sync any needed changes on their end
                let _ = tokio::time::sleep(std::time::Duration::from_secs(30)).await;

                let mut reader = backend
                    .read_key($crate::engine::cache::CacheHash([1; _]))
                    .await
                    .expect("Failed to key 1");
                let mut read_content = String::new();
                reader
                    .read_to_string(&mut read_content)
                    .await
                    .expect("Failed to read content");

                assert_eq!(
                    read_content, TEST_STRING,
                    "Read content didnt match written"
                );
            }

            #[tokio::test]
            #[test_log::test]
            async fn test_write_twice() {
                const TEST_STRING: &str = "integration testing for life!";

                let backend = $init;

                let mut writer = backend
                    .write_key($crate::engine::cache::CacheHash([2; _]))
                    .await
                    .expect("Expected to be able to write to key 2");
                writer
                    .write_all(TEST_STRING.as_bytes())
                    .await
                    .expect("Failed to write");
                writer.shutdown().await.expect("Failed to close writer");
                let _ = tokio::time::sleep(std::time::Duration::from_secs(30)).await;

                let  writer = backend
                    .write_key($crate::engine::cache::CacheHash([2; _]))
                    .await;
                assert!(writer.is_none(), "Expected trying to write key twice to return None, as caches are content addressed.");
            }
        };
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("failed to build runtime")
    }

    fn simple_container() -> ContainerState {
        let mut config = ContainerConfig::default();
        config.set_working_dir(UnixPath::new("/app"));
        config.set_env_var("PATH".into(), "/usr/local/bin".into());
        config.set_env_var("HOME".into(), "/home/root".into());
        config.set_user("root:root".into());

        ContainerState::from_parts("snapshot-fixture".into(), config)
    }

    fn simple_service() -> ServiceState {
        simple_container()
            .into_service("exec /entry".into())
            .update_service_config(|config| {
                config.set_healthcheck("exit 0".into(), Duration::from_secs(1));
            })
    }

    fn complex_container() -> ContainerState {
        let mut config = ContainerConfig::default();
        config.with_service(simple_service(), "backend".into());
        config.with_service(simple_service(), "frontend".into());

        ContainerState::from_parts("snapshot-fixture-complex".into(), config)
    }

    fn file() -> filesystem::FileSystem {
        let tree = filesystem::fuzz::Tree::File("hello world".as_bytes().to_vec());
        let bytes = tree.encode();

        filesystem::fuzz::InMemoryFile(bytes.into()).into()
    }

    fn folder() -> filesystem::FileSystem {
        let tree = filesystem::fuzz::Tree::Folder(vec![
            (
                "file1.txt".into(),
                filesystem::fuzz::Tree::File("file1".as_bytes().to_vec()),
            ),
            (
                "file2.txt".into(),
                filesystem::fuzz::Tree::File("file2".as_bytes().to_vec()),
            ),
        ]);
        let bytes = tree.encode();

        filesystem::fuzz::InMemoryFile(bytes.into()).into()
    }

    #[rstest]
    #[case::zero("zero", Data::Int(0))]
    #[case::one("one", Data::Int(1))]
    #[case::negative("negative", Data::Int(-20))]
    #[case::hello_world("hello_world", Data::String("Hello World".into()))]
    #[case::complex_string("complex_string", Data::String("\n\r".into()))]
    #[case::container("container", Data::Container(simple_container()))]
    #[case::service("service", Data::Service(simple_service()))]
    #[case::complex_container("complex_container", Data::Container(complex_container()))]
    #[case::file("file", Data::FileSystem(file()))]
    #[case::folder("folder", Data::FileSystem(folder()))]
    #[test_log::test]
    fn snapshot_hashes(#[case] name: &str, #[case] value: Data) {
        let rt = runtime();
        rt.block_on(async {
            let hash = CacheHash::from_data(CacheScope::Data, &value)
                .await
                .expect("Failed to hash value");

            insta::assert_debug_snapshot!(format!("hash_{name}"), hash, &format!("{value:?}"));
        });
    }

    #[test]
    #[test_log::test]
    fn different_entries_hash_differently() {
        let rt = runtime();
        bolero::check!().with_type().for_each(
            |(node, data1, data2): &(NodeKindId, Vec<CacheableData>, Vec<CacheableData>)| {
                rt.block_on(async {
                    if data1 == data2 {
                        return;
                    }

                    let data1 = data1
                        .iter()
                        .cloned()
                        .map(Data::from_cacheable)
                        .collect::<Vec<_>>();
                    let data2 = data2
                        .iter()
                        .cloned()
                        .map(Data::from_cacheable)
                        .collect::<Vec<_>>();

                    let key1 = CacheKey {
                        node: *node,
                        inputs: &data1,
                    };

                    let key2 = CacheKey {
                        node: *node,
                        inputs: &data2,
                    };

                    let hash_1 = CacheHash::from_data(CacheScope::Data, &key1).await.unwrap();
                    let hash_2 = CacheHash::from_data(CacheScope::Data, &key2).await.unwrap();

                    assert_ne!(hash_1, hash_2, "Keys different expected different hash.");
                });
            },
        );
    }

    #[test]
    #[test_log::test]
    fn same_entry_hashes_equal() {
        let rt = runtime();
        bolero::check!()
            .with_type()
            .for_each(|(node, data): &(NodeKindId, Vec<Data>)| {
                rt.block_on(async {
                    let key = CacheKey {
                        node: *node,
                        inputs: data,
                    };

                    assert_eq!(
                        CacheHash::from_data(CacheScope::Data, &key).await.unwrap(),
                        CacheHash::from_data(CacheScope::Data, &key).await.unwrap(),
                        "Same key expected same hash."
                    );
                });
            });
    }
}
