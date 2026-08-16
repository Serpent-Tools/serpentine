//! A content addressable cache.

use futures_util::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::engine::RuntimeError;
use crate::engine::data_model::{CacheableData, Data, NodeKindId};

mod filesystem_backend;

pub use filesystem_backend::LocalCacheBackend;

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
const CACHE_COMPATIBILITY_VERSION: u8 = 5;

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
    async fn content_hash(&self, hasher: &mut blake3::Hasher) -> Result<(), RuntimeError>;
}

impl<T: serde::Serialize> ContentHash for T {
    fn content_hash(
        &self,
        hasher: &mut blake3::Hasher,
    ) -> impl Future<Output = Result<(), RuntimeError>> {
        match postcard::to_stdvec(self) {
            Ok(bytes) => {
                hasher.update(&bytes);
                std::future::ready(Ok(()))
            }
            Err(err) => std::future::ready(Err(err.into())),
        }
    }
}

/// The kind of cache key this is, used to avoid collisions between different types of cache keys with the same data.
#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum CacheScope {
    /// `Data`
    Data,
}

impl CacheHash {
    /// Hash the given data, prefixing it with its type id, so that different types with the same data do not collide.
    pub async fn from_data<T: ContentHash>(
        scope: CacheScope,
        data: &T,
    ) -> Result<Self, RuntimeError> {
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
    ///
    /// Returns `None` if the key does not exist in the cache backend.
    fn read_key(&self, key: CacheHash) -> BoxFuture<'_, Option<Box<dyn AsyncRead + Unpin + Send>>>;

    /// Write the given key to the cache backend, returning a writer for the data.
    ///
    /// Returns `None` if the key already exists in the cache backend, and thus should not be written to.
    fn write_key(
        &self,
        key: CacheHash,
    ) -> BoxFuture<'_, Option<Box<dyn AsyncWrite + Unpin + Send>>>;

    /// Retrive a reader to the  `DataCache` from this backend, this does not use `read_key`
    /// as the data cache should always be loaded and is not lazily loaded based on keys.
    fn get_data_cache(&self) -> BoxFuture<'static, Option<Box<dyn AsyncRead + Unpin + Send>>>;

    /// Get a writer to the `DataCache` in this backend, this does not use `write_key`
    /// as the data cache should always be saved and is not lazily saved based on keys
    fn get_data_cache_writer(
        &self,
    ) -> BoxFuture<'_, Result<Box<dyn AsyncWrite + Unpin + Send>, RuntimeError>>;
}

static_assertions::assert_obj_safe!(CacheBackend);

/// A key into the cache
#[derive(Debug)]
pub struct CacheKey<'caller> {
    /// The kind of node
    pub node: NodeKindId,
    /// The inputs to the node
    pub inputs: &'caller [Data],
}

impl ContentHash for CacheKey<'_> {
    async fn content_hash(&self, hasher: &mut blake3::Hasher) -> Result<(), RuntimeError> {
        hasher.update(&self.node.index().to_le_bytes());
        hasher.update(&(self.inputs.len() as u64).to_le_bytes());

        for input in self.inputs {
            input.content_hash(hasher).await?;
        }

        Ok(())
    }
}

// /// A external cache is data stored in another service like a docker volume that our cache system
// /// needs to take into account.
// pub trait ExternalCache {
//     /// Identity of an external resource referenced by cached data.
//     type ResourceKey: std::hash::Hash + Eq;
//
//     /// Export data from the external cache to this file
//     async fn export(
//         &self,
//         values: impl IntoIterator<Item = &Data>,
//         file: &mut (impl AsyncWrite + Unpin + Send),
//     ) -> Result<(), RuntimeError>;
//
//     /// Import data from the given file to this external cache
//     async fn import(&self, file: &mut (impl AsyncRead + Unpin + Send)) -> Result<(), RuntimeError>;
//
//     /// The external resources the given data references.
//     fn resource_keys(&self, data: &Data) -> Vec<Self::ResourceKey>;
//
//     /// Delete the given resource from this external cache
//     async fn cleanup(&self, key: Self::ResourceKey);
// }

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
    async fn write(
        self,
        keep_old_cache: bool,
        mut writer: impl AsyncWrite + Unpin + Send,
    ) -> Result<(), RuntimeError> {
        writer.write_u8(CACHE_COMPATIBILITY_VERSION).await?;

        let cache = if keep_old_cache {
            let mut combined_cache = self.new_cache;
            combined_cache.extend(self.old_cache);
            combined_cache
        } else {
            self.new_cache
        };

        serpentine_internal::write_postcard_frame(&cache, &mut writer).await?;

        Ok(())
    }

    /// Load a cache from the given reader, checking the version number.
    async fn load(mut reader: impl AsyncRead + Unpin + Send) -> Result<Self, RuntimeError> {
        let version = reader.read_u8().await?;
        if version != CACHE_COMPATIBILITY_VERSION {
            return Err(RuntimeError::CacheOutOfDate {
                current: CACHE_COMPATIBILITY_VERSION,
                got: version,
            });
        }

        let cache = serpentine_internal::read_postcard_frame(&mut reader).await?;

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
    pub backend: Box<dyn CacheBackend + Send + Sync>,
}

impl Cache {
    /// Load the cache from the given backend, or create a new empty cache if the backend does not have a cache.
    pub async fn new(backend: Box<dyn CacheBackend + Send + Sync>) -> Result<Self, RuntimeError> {
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

    /// Save the cache to the backend.
    pub async fn save(self, keep_old_cache: bool) -> Result<(), RuntimeError> {
        let data_cache = self.data_cache.into_inner().map_err(|_| {
            RuntimeError::internal("Failed to lock data cache for saving, mutex poisoned")
        })?;

        let mut writer = self.backend.get_data_cache_writer().await?;
        data_cache.write(keep_old_cache, &mut writer).await?;

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
    use crate::engine::filesystem;

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
                        .map(Data::from_cachable)
                        .collect::<Vec<_>>();
                    let data2 = data2
                        .iter()
                        .cloned()
                        .map(Data::from_cachable)
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
