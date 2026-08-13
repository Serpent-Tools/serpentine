//! A content addressable cache.

use std::pin::Pin;

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
    fn read_key(
        &self,
        key: CacheHash,
    ) -> Pin<Box<dyn Future<Output = Option<Box<dyn AsyncRead + Unpin + Send>>>>>;

    /// Write the given key to the cache backend, returning a writer for the data.
    ///
    /// Returns `None` if the key already exists in the cache backend, and thus should not be written to.
    fn write_key(
        &self,
        key: CacheHash,
    ) -> Pin<Box<dyn Future<Output = Option<Box<dyn AsyncWrite + Unpin + Send>>>>>;

    /// Retrive a reader to the  `DataCache` from this backend, this does not use `read_key`
    /// as the data cache should always be loaded and is not lazily loaded based on keys.
    fn get_data_cache(
        &self,
    ) -> Pin<Box<dyn Future<Output = Option<Box<dyn AsyncRead + Unpin + Send>>>>>;

    /// Get a writer to the `DataCache` in this backend, this does not use `write_key`
    /// as the data cache should always be saved and is not lazily saved based on keys
    fn get_data_cache_writer(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn AsyncWrite + Unpin + Send>, RuntimeError>>>>;
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
    async fn write(&self, mut writer: impl AsyncWrite + Unpin + Send) -> Result<(), RuntimeError> {
        writer.write_u8(CACHE_COMPATIBILITY_VERSION).await?;
        serpentine_internal::write_postcard_frame(&self, &mut writer).await?;

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

        Ok(cache)
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
    pub async fn save(self) -> Result<(), RuntimeError> {
        let data_cache = self.data_cache.into_inner().map_err(|_| {
            RuntimeError::internal("Failed to lock data cache for saving, mutex poisoned")
        })?;

        let mut writer = self.backend.get_data_cache_writer().await?;
        data_cache.write(&mut writer).await?;

        Ok(())
    }
}

// TODO: Snapshot test hashes

// #[cfg(test)]
// #[expect(clippy::expect_used, reason = "tests")]
// mod tests {
//     use rstest::{fixture, rstest};
//
//     use super::*;
//
//     struct DummyExternal;
//
//     // async-to-`std::future::ready` needs a named lifetime on `_values`, which the trait elides
//     #[expect(clippy::unused_async_trait_impl, reason = "tests")]
//     impl ExternalCache for DummyExternal {
//         type ResourceKey = ();
//
//         async fn export(
//             &self,
//             _values: impl IntoIterator<Item = &Data>,
//             _file: &mut (impl AsyncWrite + Unpin + Send),
//         ) -> Result<(), RuntimeError> {
//             Ok(())
//         }
//
//         async fn import(
//             &self,
//             _file: &mut (impl AsyncRead + Unpin + Send),
//         ) -> Result<(), RuntimeError> {
//             Ok(())
//         }
//
//         fn resource_keys(&self, _data: &Data) -> Vec<Self::ResourceKey> {
//             Vec::new()
//         }
//
//         async fn cleanup(&self, _key: Self::ResourceKey) {}
//     }
//
//     #[fixture]
//     fn external() -> impl ExternalCache {
//         DummyExternal
//     }
//
//     /// Serialize a value with a fresh writer, giving a structural fingerprint to compare against.
//     async fn data_bytes(data: &Data) -> Vec<u8> {
//         let mut out = std::io::Cursor::new(Vec::new());
//         data.write(&mut CacheWriter::new(&mut out))
//             .await
//             .expect("in-memory serialization cannot fail");
//         out.into_inner()
//     }
//
//     /// Content hash of a data value, used to compare cache roundtrips.
//     async fn data_hash(data: &Data) -> CacheHash {
//         let mut hasher = blake3::Hasher::new();
//         data.content_hash(&mut hasher)
//             .await
//             .expect("hashing in-memory data cannot fail");
//         CacheHash(hasher.finalize().into())
//     }
//
//     fn runtime() -> tokio::runtime::Runtime {
//         tokio::runtime::Builder::new_current_thread()
//             .build()
//             .expect("failed to build runtime")
//     }
//
//     #[test]
//     fn different_entries_hash_differently() {
//         let rt = runtime();
//         bolero::check!().with_type().for_each(
//             |(node, data1, data2): &(NodeKindId, Vec<Data>, Vec<Data>)| {
//                 rt.block_on(async {
//                     let key1 = CacheKey {
//                         node: *node,
//                         inputs: data1,
//                     };
//
//                     let key2 = CacheKey {
//                         node: *node,
//                         inputs: data2,
//                     };
//
//                     let hash_1 = key1.content_hash().await.unwrap();
//                     let hash_2 = key2.content_hash().await.unwrap();
//
//                     let mut equal = data1.len() == data2.len();
//                     for (value1, value2) in data1.iter().zip(data2) {
//                         equal &= data_bytes(value1).await == data_bytes(value2).await;
//                     }
//
//                     if equal {
//                         assert_eq!(hash_1, hash_2, "Keys equal expected same hash.");
//                     } else {
//                         assert_ne!(hash_1, hash_2, "Keys different expected different hash.");
//                     }
//                 });
//             },
//         );
//     }
//
//     #[test]
//     fn same_entry_hashes_equal() {
//         let rt = runtime();
//         bolero::check!()
//             .with_type()
//             .for_each(|(node, data): &(NodeKindId, Vec<Data>)| {
//                 rt.block_on(async {
//                     let key = CacheKey {
//                         node: *node,
//                         inputs: data,
//                     };
//
//                     assert_eq!(
//                         key.content_hash().await.unwrap(),
//                         key.content_hash().await.unwrap()
//                     );
//                 });
//             });
//     }
//
//     #[test]
//     fn save_and_load_one_entry() {
//         let rt = runtime();
//         bolero::check!().with_type().for_each(
//             |(node, data, value): &(NodeKindId, Vec<Data>, Data)| {
//                 rt.block_on(async {
//                     let key = CacheKey {
//                         node: *node,
//                         inputs: data,
//                     };
//
//                     let mut cache = DataCache::new();
//                     cache.insert(key.content_hash().await.unwrap(), value.clone());
//
//                     let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
//                     cache
//                         .save_cache(&mut cache_file, &DummyExternal, false, false)
//                         .await
//                         .unwrap();
//
//                     cache_file.set_position(0);
//                     let mut loaded_cache = DataCache::load_cache(&mut cache_file, &DummyExternal)
//                         .await
//                         .unwrap();
//                     let loaded_value = loaded_cache
//                         .get(key.content_hash().await.unwrap())
//                         .expect("Value not found");
//
//                     assert_eq!(data_hash(loaded_value).await, data_hash(value).await);
//                 });
//             },
//         );
//     }
//
//     #[test]
//     fn save_and_load_duplicate() {
//         let rt = runtime();
//         bolero::check!().with_type().for_each(|value: &Data| {
//             rt.block_on(async {
//                 let mut cache = DataCache::new();
//
//                 let key1 = CacheHash(blake3::hash(&[0]).into());
//                 let key2 = CacheHash(blake3::hash(&[1]).into());
//                 let key3 = CacheHash(blake3::hash(&[2]).into());
//
//                 cache.insert(key1, value.clone());
//                 cache.insert(key2, value.clone());
//                 cache.insert(key3, value.clone());
//
//                 let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
//                 cache
//                     .save_cache(&mut cache_file, &DummyExternal, false, false)
//                     .await
//                     .unwrap();
//
//                 cache_file.set_position(0);
//                 let mut loaded_cache = DataCache::load_cache(&mut cache_file, &DummyExternal)
//                     .await
//                     .unwrap();
//                 let expected = data_hash(value).await;
//                 for key in [key1, key2, key3] {
//                     let loaded_value = loaded_cache.get(key).expect("Value not found");
//                     assert_eq!(data_hash(loaded_value).await, expected);
//                 }
//             });
//         });
//     }
//
//     #[tokio::test]
//     #[rstest]
//     #[test_log::test]
//     #[expect(clippy::panic, reason = "tests")]
//     async fn save_and_load_duplicate_rc_is_deduplicated(external: impl ExternalCache) {
//         let mut cache = DataCache::new();
//
//         let value = Data::String(Arc::from("foo"));
//
//         let key1 = CacheHash(blake3::hash(&[0]).into());
//         let key2 = CacheHash(blake3::hash(&[1]).into());
//
//         cache.insert(key1, value.clone());
//         cache.insert(key2, value.clone());
//
//         let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
//         cache
//             .save_cache(&mut cache_file, &external, false, false)
//             .await
//             .unwrap();
//
//         cache_file.set_position(0);
//         let mut loaded_cache = DataCache::load_cache(&mut cache_file, &external)
//             .await
//             .unwrap();
//         let loaded_value1 = loaded_cache.get(key1).expect("Value not found").clone();
//         let loaded_value2 = loaded_cache.get(key1).expect("Value not found").clone();
//
//         let Data::String(value1) = loaded_value1 else {
//             panic!("Unexpected enum variant");
//         };
//         let Data::String(value2) = loaded_value2 else {
//             panic!("Unexpected enum variant");
//         };
//
//         assert!(
//             Arc::ptr_eq(&value1, &value2),
//             "Rcs point to different allocations despite being serialized from the same rc allocation."
//         );
//     }
//
//     #[test]
//     fn save_and_load_multiple_entries() {
//         let rt = runtime();
//         bolero::check!()
//             .with_generator(
//                 bolero::produce::<Vec<(NodeKindId, Vec<Data>, Data)>>()
//                     .with()
//                     .len(0..4_usize),
//             )
//             .for_each(|values: &Vec<(NodeKindId, Vec<Data>, Data)>| {
//                 rt.block_on(async {
//                     let mut cache = DataCache::new();
//                     for (node, data, value) in values {
//                         let key = CacheKey {
//                             node: *node,
//                             inputs: data,
//                         };
//
//                         cache.insert(key.content_hash().await.unwrap(), value.clone());
//                     }
//
//                     let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
//                     cache
//                         .save_cache(&mut cache_file, &DummyExternal, false, false)
//                         .await
//                         .unwrap();
//
//                     cache_file.set_position(0);
//                     let mut loaded_cache = DataCache::load_cache(&mut cache_file, &DummyExternal)
//                         .await
//                         .unwrap();
//
//                     for (node, data, _) in values {
//                         let key = CacheKey {
//                             node: *node,
//                             inputs: data,
//                         };
//
//                         let _ = loaded_cache
//                             .get(key.content_hash().await.unwrap())
//                             .expect("Value not found");
//                         // We do not check what the value is as generation might (and likely will)
//                         // produce duplicate keys.
//                     }
//                 });
//             });
//     }
//
//     /// If a entry in the old cache is used then it should be kept even if `keep_old_cache` is false.
//     /// As `keep_old_cache=false` is for cleaning up cache not used/generated this session.
//     #[test]
//     fn if_cache_used_should_always_be_kept() {
//         let rt = runtime();
//         bolero::check!().with_type().for_each(
//             |(node, data, value): &(NodeKindId, Vec<Data>, Data)| {
//                 rt.block_on(async {
//                     let key = CacheKey {
//                         node: *node,
//                         inputs: data,
//                     };
//
//                     let mut cache = DataCache::new();
//                     cache.insert(key.content_hash().await.unwrap(), value.clone());
//
//                     let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
//                     cache
//                         .save_cache(&mut cache_file, &DummyExternal, false, false)
//                         .await
//                         .unwrap();
//
//                     cache_file.set_position(0);
//                     let mut loaded_cache = DataCache::load_cache(&mut cache_file, &DummyExternal)
//                         .await
//                         .unwrap();
//                     loaded_cache
//                         .get(key.content_hash().await.unwrap())
//                         .expect("Value not found");
//
//                     // Even tho `keep_old_cache` is false it should still keep the entry in there
//                     // since we used it.
//                     cache_file.set_position(0);
//                     cache_file.get_mut().clear();
//                     loaded_cache
//                         .save_cache(&mut cache_file, &DummyExternal, false, false)
//                         .await
//                         .unwrap();
//
//                     cache_file.set_position(0);
//                     let mut second_loaded_cache =
//                         DataCache::load_cache(&mut cache_file, &DummyExternal)
//                             .await
//                             .unwrap();
//                     second_loaded_cache
//                         .get(key.content_hash().await.unwrap())
//                         .expect("Value not found");
//                 });
//             },
//         );
//     }
//
//     #[test]
//     fn old_entry_cleared_if_not_used() {
//         let rt = runtime();
//         bolero::check!().with_type().for_each(
//             |(node, data, value): &(NodeKindId, Vec<Data>, Data)| {
//                 rt.block_on(async {
//                     let key = CacheKey {
//                         node: *node,
//                         inputs: data,
//                     };
//
//                     let mut cache = DataCache::new();
//                     cache.insert(key.content_hash().await.unwrap(), value.clone());
//
//                     let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
//                     cache
//                         .save_cache(&mut cache_file, &DummyExternal, false, false)
//                         .await
//                         .unwrap();
//
//                     cache_file.set_position(0);
//                     let loaded_cache = DataCache::load_cache(&mut cache_file, &DummyExternal)
//                         .await
//                         .unwrap();
//
//                     cache_file.set_position(0);
//                     cache_file.get_mut().clear();
//                     loaded_cache
//                         .save_cache(&mut cache_file, &DummyExternal, false, false)
//                         .await
//                         .unwrap();
//
//                     cache_file.set_position(0);
//                     let mut second_loaded_cache =
//                         DataCache::load_cache(&mut cache_file, &DummyExternal)
//                             .await
//                             .unwrap();
//                     let result = second_loaded_cache.get(key.content_hash().await.unwrap());
//                     assert!(result.is_none(), "unused old_cache value was saved.");
//                 });
//             },
//         );
//     }
//
//     #[test]
//     fn old_entry_kept_if_keep_old_true_even_if_not_used() {
//         let rt = runtime();
//         bolero::check!().with_type().for_each(
//             |(node, data, value): &(NodeKindId, Vec<Data>, Data)| {
//                 rt.block_on(async {
//                     let key = CacheKey {
//                         node: *node,
//                         inputs: data,
//                     };
//
//                     let mut cache = DataCache::new();
//                     cache.insert(key.content_hash().await.unwrap(), value.clone());
//
//                     let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
//                     cache
//                         .save_cache(&mut cache_file, &DummyExternal, false, false)
//                         .await
//                         .unwrap();
//
//                     cache_file.set_position(0);
//                     let loaded_cache = DataCache::load_cache(&mut cache_file, &DummyExternal)
//                         .await
//                         .unwrap();
//
//                     cache_file.set_position(0);
//                     cache_file.get_mut().clear();
//                     loaded_cache
//                         .save_cache(&mut cache_file, &DummyExternal, true, false)
//                         .await
//                         .unwrap();
//
//                     cache_file.set_position(0);
//                     let mut second_loaded_cache =
//                         DataCache::load_cache(&mut cache_file, &DummyExternal)
//                             .await
//                             .unwrap();
//                     second_loaded_cache
//                         .get(key.content_hash().await.unwrap())
//                         .expect("Value not found");
//                 });
//             },
//         );
//     }
// }
