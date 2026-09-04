//! A content addressable cache.

use std::collections::HashSet;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use miette::{Context, Diagnostic, IntoDiagnostic};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::engine::data_model::{CacheableData, Data, NodeKindId, ResourceKey};
use crate::engine::{BoxedReader, BoxedWriter, internal};

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
pub const CACHE_COMPATIBILITY_VERSION: u8 = 5;

/// The cache was out of date.
#[derive(Debug, Error, Diagnostic)]
#[error("Cache format version {got} doesn't match current version {current}")]
struct CacheOutOfDate {
    /// The version in the cache file
    got: u8,
    /// The version of this binary
    current: u8,
}

/// Wrapper around the raw blake3 hash output as its trait implementations (`Hash` and `Eq`) use
/// constant time functions, which we do not require
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CacheHash([u8; blake3::OUT_LEN]);

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
    /// (It is not a requirement to return `None` when the key already exsists, but its highly
    /// engouraged.)
    fn write_key(&self, key: CacheHash) -> BoxFuture<'_, Option<BoxedWriter>>;

    /// Retrieve a reader to the  `DataCache` from this backend, this does not use `read_key`
    /// as the data cache should always be loaded and is not lazily loaded based on keys.
    ///
    /// Should return the same data as written by `get_data_cache_writer`, preferably the latest
    /// bytes written, but any previously written bytes are acceptable. (or `None` if none written).
    fn get_data_cache(&self) -> BoxFuture<'_, Option<BoxedReader>>;

    /// Get a writer to the `DataCache` in this backend, this does not use `write_key`
    /// as the data cache should always be saved and is not lazily saved based on keys
    fn get_data_cache_writer(&self) -> BoxFuture<'_, miette::Result<BoxedWriter>>;

    /// Delete the given key if it exists in the cache backend.
    ///
    /// This function is allowed to be a noop if the backend already performs its own eviction, or
    /// if the backend is append-only and does not support deletion.
    fn delete_key(&self, _key: CacheHash) -> BoxFuture<'_, ()> {
        Box::pin(std::future::ready(()))
    }
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

    fn get_data_cache(&self) -> BoxFuture<'_, Option<BoxedReader>> {
        Box::pin(std::future::ready(None))
    }

    fn get_data_cache_writer(&self) -> BoxFuture<'_, miette::Result<BoxedWriter>> {
        Box::pin(std::future::ready(Ok(BoxedWriter::new(
            std::io::Cursor::new(Vec::new()),
        ))))
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

/// A hashmap storing the cache data
type CacheHashMap = nohash::IntMap<CacheHash, CacheableData>;

/// A content addressable cache using blake3
/// And allows serializing to disk
#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct DataCache {
    /// The cache that was loaded from disk, might not be serialized.
    old_cache: CacheHashMap,
    /// The cache generated from this run.
    new_cache: CacheHashMap,
}

impl DataCache {
    /// Create a new empty cache
    fn new() -> Self {
        Self::default()
    }

    /// Store a value in the cache
    pub fn insert(&mut self, key: CacheHash, value: CacheableData) {
        log::debug!("Saving {key:?}={value:?} in cache");
        self.new_cache.insert(key, value);
    }

    /// Get a value from the cache
    ///
    /// This also moves the value from `old_cache` to `new_cache`
    pub fn get(&mut self, key: CacheHash) -> Option<&CacheableData> {
        log::debug!("Reading {key:?}");
        if let Some(data) = self.old_cache.remove(&key) {
            log::debug!("Got {data:?}, moving to new_cache");
            let data = self.new_cache.entry(key).insert_entry(data).into_mut();
            Some(data)
        } else if let Some(data) = self.new_cache.get(&key) {
            log::debug!("Got {data:?}");
            Some(data)
        } else {
            log::debug!("Key {key:?} not in cache");
            None
        }
    }

    /// Write this cache to the given writer, including the version number.
    ///
    /// Returns the resource keys to clean from the cache backend.
    async fn write(
        self,
        keep_old_cache: bool,
        mut writer: impl AsyncWrite + Unpin + Send,
    ) -> miette::Result<HashSet<ResourceKey>> {
        writer
            .write_u8(CACHE_COMPATIBILITY_VERSION)
            .await
            .into_diagnostic()
            .context("writing the cache version")?;

        if keep_old_cache {
            let mut combined_cache = self.new_cache;
            combined_cache.extend(self.old_cache);
            serpentine_internal::write_postcard_frame(&combined_cache, &mut writer)
                .await
                .into_diagnostic()
                .context("writing the data cache")?;
            Ok(HashSet::new())
        } else {
            serpentine_internal::write_postcard_frame(&self.new_cache, &mut writer)
                .await
                .into_diagnostic()
                .context("writing the data cache")?;
            Ok(self
                .old_cache
                .into_values()
                .flat_map(|data| data.resource_keys())
                .collect())
        }
    }

    /// Load a cache from the given reader, checking the version number.
    async fn load(mut reader: impl AsyncRead + Unpin + Send) -> miette::Result<Self> {
        let version = reader
            .read_u8()
            .await
            .into_diagnostic()
            .context("reading the cache version")?;
        if version != CACHE_COMPATIBILITY_VERSION {
            return Err(CacheOutOfDate {
                current: CACHE_COMPATIBILITY_VERSION,
                got: version,
            }
            .into());
        }

        let cache = serpentine_internal::read_postcard_frame(&mut reader)
            .await
            .into_diagnostic()
            .context("reading the data cache")?;

        Ok(DataCache {
            old_cache: cache,
            new_cache: CacheHashMap::default(),
        })
    }
}

/// A cache that can be used to store both `Data` and arbitrary keyed blobs.
pub struct Cache {
    /// The cache for `Data` values
    pub data_cache: std::sync::Mutex<DataCache>,
    /// The cache for arbitrary keyed blobs
    pub backend: Arc<dyn CacheBackend + Send + Sync>,
}

impl Cache {
    /// Load the cache from the given backend, or create a new empty cache if the backend does not have a cache.
    pub async fn new(backend: Arc<dyn CacheBackend + Send + Sync>) -> miette::Result<Self> {
        let data_cache = if let Some(mut reader) = backend.get_data_cache().await {
            match DataCache::load(&mut reader).await {
                Ok(data_cache) => data_cache,
                Err(err) => {
                    log::warn!(
                        "Failed to load cache from backend: {err}, creating new empty cache"
                    );
                    DataCache::new()
                }
            }
        } else {
            DataCache::new()
        };

        Ok(Self {
            data_cache: std::sync::Mutex::new(data_cache),
            backend,
        })
    }

    /// Delete the given resource key, split out to make error handling easier in `save`.
    async fn delete_resource_key(
        backend: &(impl CacheBackend + ?Sized),
        key: &ResourceKey,
    ) -> miette::Result<()> {
        let hash = key.cache_hash().await?;
        backend.delete_key(hash).await;

        Ok(())
    }

    /// Save the cache to the backend.
    ///
    /// Returns a hashset of the resource keys that the shutdown system should pass along to the
    /// various engines for cleanup.
    pub async fn save(self, keep_old_cache: bool) -> miette::Result<HashSet<ResourceKey>> {
        let Self {
            data_cache,
            backend,
        } = self;

        let data_cache = data_cache
            .into_inner()
            .map_err(|_| internal("data cache mutex poisoned"))?;

        let mut writer = backend.get_data_cache_writer().await?;
        let removed_resource_keys = data_cache.write(keep_old_cache, &mut writer).await?;
        writer
            .shutdown()
            .await
            .into_diagnostic()
            .context("flushing the data cache")?;

        for key in &removed_resource_keys {
            if let Err(err) = Self::delete_resource_key(&*backend, key).await {
                log::error!("Failed to delete resource key {key:?}: {err}");
            }
        }

        Ok(removed_resource_keys)
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
    /// * `.get_data_cache` when a data cache has not been written returns `None`, (see note on
    ///   `read_key`).
    /// * `.get_data_cache_writer`, followed by `.get_data_cache` returns the data written.
    /// * `.get_data_cache_writer`, followed by `.get_data_cache_writer` (writing other bytes),
    ///   followed by `.get_data_cache` returns the last written bytes.
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

                let  writer = backend
                    .write_key($crate::engine::cache::CacheHash([2; _]))
                    .await;
                assert!(writer.is_none(), "Expected trying to write key twice to return None, as caches are content addressed.");
            }

            #[tokio::test]
            #[test_log::test]
            async fn test_data_cache() {
                const TEST_STRING1: &str = "integration testing for life!";
                const TEST_STRING2: &str = "macros go brrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr.";

                let backend = $init;

                let reader = backend.get_data_cache().await;
                assert!(reader.is_none(), "Expected data cache to be None before writing.");

                let mut writer1 = backend
                    .get_data_cache_writer()
                    .await
                    .expect("Expected to be able to write to cache data");
                writer1
                    .write_all(TEST_STRING1.as_bytes())
                    .await
                    .expect("Failed to write");
                writer1.shutdown().await.expect("Failed to close writer");

                let mut reader1 = backend
                    .get_data_cache()
                    .await
                    .expect("Failed to get cache data");
                let mut read_content1 = String::new();
                reader1
                    .read_to_string(&mut read_content1)
                    .await
                    .expect("Failed to read content");
                assert_eq!(read_content1, TEST_STRING1, "Expected bytes read from data cache to match first write");

                let mut writer2 = backend
                    .get_data_cache_writer()
                    .await
                    .expect("Expected to be able to write to cache data twice");
                writer2
                    .write_all(TEST_STRING2.as_bytes())
                    .await
                    .expect("Failed to write");
                writer2.shutdown().await.expect("Failed to close writer");

                let mut reader2 = backend
                    .get_data_cache()
                    .await
                    .expect("Failed to get cache data");
                let mut read_content2 = String::new();
                reader2
                    .read_to_string(&mut read_content2)
                    .await
                    .expect("Failed to read content");
                assert_eq!(read_content2, TEST_STRING2, "Expected bytes read from data cache to match second write");
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

    #[test]
    #[test_log::test]
    fn save_and_load_one_entry() {
        let rt = runtime();
        bolero::check!().with_type().for_each(
            |(node, data, value): &(NodeKindId, Vec<Data>, CacheableData)| {
                rt.block_on(async {
                    let key = CacheKey {
                        node: *node,
                        inputs: data,
                    };

                    let mut cache = DataCache::new();
                    let hash = CacheHash::from_data(CacheScope::Data, &key).await.unwrap();
                    cache.insert(hash, value.clone());

                    let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
                    cache.write(true, &mut cache_file).await.unwrap();

                    cache_file.set_position(0);
                    let mut loaded_cache = DataCache::load(&mut cache_file).await.unwrap();
                    let loaded_value = loaded_cache.get(hash).expect("Value not found");

                    assert_eq!(loaded_value, value);
                });
            },
        );
    }

    #[test]
    #[test_log::test]
    fn save_and_load_duplicate() {
        let rt = runtime();
        bolero::check!()
            .with_type()
            .for_each(|value: &CacheableData| {
                rt.block_on(async {
                    let mut cache = DataCache::new();

                    let key1 = CacheHash(blake3::hash(&[0]).into());
                    let key2 = CacheHash(blake3::hash(&[1]).into());
                    let key3 = CacheHash(blake3::hash(&[2]).into());

                    cache.insert(key1, value.clone());
                    cache.insert(key2, value.clone());
                    cache.insert(key3, value.clone());

                    let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
                    cache.write(true, &mut cache_file).await.unwrap();

                    cache_file.set_position(0);
                    let mut loaded_cache = DataCache::load(&mut cache_file).await.unwrap();

                    for key in [key1, key2, key3] {
                        let loaded_value = loaded_cache.get(key).expect("Value not found");
                        assert_eq!(loaded_value, value);
                    }
                });
            });
    }

    #[test]
    #[test_log::test]
    fn save_and_load_multiple_entries() {
        let rt = runtime();
        bolero::check!()
            .with_generator(
                bolero::produce::<Vec<(NodeKindId, Vec<Data>, CacheableData)>>()
                    .with()
                    .len(0..4_usize),
            )
            .for_each(|values: &Vec<(NodeKindId, Vec<Data>, CacheableData)>| {
                rt.block_on(async {
                    let mut cache = DataCache::new();
                    for (node, data, value) in values {
                        let key = CacheKey {
                            node: *node,
                            inputs: data,
                        };

                        cache.insert(
                            CacheHash::from_data(CacheScope::Data, &key).await.unwrap(),
                            value.clone(),
                        );
                    }

                    let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
                    cache.write(true, &mut cache_file).await.unwrap();

                    cache_file.set_position(0);
                    let mut loaded_cache = DataCache::load(&mut cache_file).await.unwrap();

                    for (node, data, _value) in values {
                        let key = CacheKey {
                            node: *node,
                            inputs: data,
                        };

                        let _ = loaded_cache
                            .get(CacheHash::from_data(CacheScope::Data, &key).await.unwrap())
                            .expect("Value not found");
                    }
                });
            });
    }

    /// If a entry in the old cache is used then it should be kept even if `keep_old_cache` is false.
    /// As `keep_old_cache=false` is for cleaning up cache not used/generated this session.
    #[test]
    #[test_log::test]
    fn if_cache_used_should_always_be_kept() {
        let rt = runtime();
        bolero::check!().with_type().for_each(
            |(node, data, value): &(NodeKindId, Vec<Data>, CacheableData)| {
                rt.block_on(async {
                    let key = CacheKey {
                        node: *node,
                        inputs: data,
                    };

                    let hash = CacheHash::from_data(CacheScope::Data, &key).await.unwrap();

                    let mut cache = DataCache::new();
                    cache.insert(hash, value.clone());

                    let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
                    cache.write(false, &mut cache_file).await.unwrap();

                    cache_file.set_position(0);
                    let mut loaded_cache = DataCache::load(&mut cache_file).await.unwrap();

                    loaded_cache.get(hash).expect("Value not found");

                    // Even tho `keep_old_cache` is false it should still keep the entry in there
                    // since we used it.
                    cache_file.set_position(0);
                    cache_file.get_mut().clear();

                    loaded_cache.write(false, &mut cache_file).await.unwrap();

                    cache_file.set_position(0);
                    let mut second_loaded_cache = DataCache::load(&mut cache_file).await.unwrap();
                    second_loaded_cache.get(hash).expect("Value not found");
                });
            },
        );
    }

    #[test]
    #[test_log::test]
    fn old_entry_cleared_if_not_used() {
        let rt = runtime();
        bolero::check!().with_type().for_each(
            |(node, data, value): &(NodeKindId, Vec<Data>, CacheableData)| {
                rt.block_on(async {
                    let key = CacheKey {
                        node: *node,
                        inputs: data,
                    };

                    let hash = CacheHash::from_data(CacheScope::Data, &key).await.unwrap();
                    let mut cache = DataCache::new();
                    cache.insert(hash, value.clone());

                    let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
                    cache.write(false, &mut cache_file).await.unwrap();

                    cache_file.set_position(0);
                    let loaded_cache = DataCache::load(&mut cache_file).await.unwrap();

                    cache_file.set_position(0);
                    cache_file.get_mut().clear();
                    loaded_cache.write(false, &mut cache_file).await.unwrap();

                    cache_file.set_position(0);
                    let mut second_loaded_cache = DataCache::load(&mut cache_file).await.unwrap();
                    let result = second_loaded_cache.get(hash);
                    assert!(result.is_none(), "unused old_cache value was saved.");
                });
            },
        );
    }

    #[test]
    #[test_log::test]
    fn old_entry_kept_if_keep_old_true_even_if_not_used() {
        let rt = runtime();
        bolero::check!().with_type().for_each(
            |(node, data, value): &(NodeKindId, Vec<Data>, CacheableData)| {
                rt.block_on(async {
                    let key = CacheKey {
                        node: *node,
                        inputs: data,
                    };

                    let hash = CacheHash::from_data(CacheScope::Data, &key).await.unwrap();
                    let mut cache = DataCache::new();
                    cache.insert(hash, value.clone());

                    let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
                    cache.write(true, &mut cache_file).await.unwrap();

                    cache_file.set_position(0);
                    let loaded_cache = DataCache::load(&mut cache_file).await.unwrap();

                    cache_file.set_position(0);
                    cache_file.get_mut().clear();
                    loaded_cache.write(true, &mut cache_file).await.unwrap();

                    cache_file.set_position(0);
                    let mut second_loaded_cache = DataCache::load(&mut cache_file).await.unwrap();
                    second_loaded_cache.get(hash).expect("Value not found");
                });
            },
        );
    }
}
