//! A content addressable cache.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::engine::RuntimeError;
use crate::engine::data_model::{Data, NodeKindId};

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
const CACHE_COMPATIBILITY_VERSION: u8 = 2;

/// Wrapper around the raw blake3 hash output as its trait implementations (`Hash` and `Eq`) use
/// constant time functions, which we do not require
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheHash([u8; blake3::OUT_LEN]);

// This must only call one `write` method (and only ones <= 64 bits).
// https://docs.rs/nohash/latest/nohash/trait.IsEnabled.html
//
// This function does this by just taking the first 8 bytes of the hash.
// This is okay because they are as evenly distrubted in isolation as the whole hash.
// And secondly because the `HashMap` will compare the full hashes anyway in the unlikely event of
// a collision.
impl std::hash::Hash for CacheHash {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        const U64_BYTES: usize = 64 / 8;

        let first_bytes = self.0.first_chunk::<U64_BYTES>().unwrap_or_else(|| {
            debug_assert!(
                false,
                "blake3 hash ({} bytes), not big enough to construct u64 ({U64_BYTES} bytes)",
                blake3::OUT_LEN
            );

            &[0; U64_BYTES]
        });

        state.write_u64(u64::from_le_bytes(*first_bytes));
    }
}

impl nohash::IsEnabled for CacheHash {}

/// Struct for writing the cache data, provides a thin wrapper over `AsyncWrite` writer, as well as
/// methods for dealing with `Rc`
pub struct CacheWriter<T> {
    /// The file to write to
    file: T,
    /// The map of Rc pointers to ids
    rcs: HashMap<*const u8, u64>,
    /// The next id to use for a rc
    next_rc_id: u64,
}

impl<T: AsyncWrite + Unpin + Send> CacheWriter<T> {
    /// Create a new cache writer
    pub fn new(file: T) -> Self {
        Self {
            file,
            rcs: HashMap::new(),
            next_rc_id: 1, // We start at 1, as 0 is used as a sentinel
        }
    }

    /// Write the given cache map to the cache file, also exports the external cache if
    /// `export_standalone` specified.
    async fn write_map(
        &mut self,
        map: CacheHashMap,
        external: &impl ExternalCache,
        export_standalone: bool,
    ) -> Result<(), RuntimeError> {
        self.file.write_all(&[CACHE_COMPATIBILITY_VERSION]).await?;

        serpentine_internal::write_u64_variable_length(&mut self.file, map.len() as u64).await?;
        for (hash, value) in &map {
            self.file.write_all(&hash.0).await?;
            log::debug!("Writing {value:?}");
            value.write(self).await?;
        }

        if export_standalone {
            self.file.write_all(&[1]).await?;
            log::info!("Exporting standalone cache");
            external.export(map.values(), &mut self.file).await?;
        } else {
            self.file.write_all(&[0]).await?;
        }

        self.file.flush().await?;

        Ok(())
    }

    /// Write the given rc to the file, this will duplicate rcs that point to the same data.
    ///
    /// If the rc hasnt been writen yet calls `writer`
    pub async fn write_rc<Value, Func>(
        &mut self,
        rc: &Rc<Value>,
        writer: Func,
    ) -> Result<(), RuntimeError>
    where
        Func: for<'value> FnOnce(
            &'value mut Self,
            &'value Rc<Value>,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<(), RuntimeError>> + 'value>,
        >,
        Value: ?Sized + 'static,
    {
        let pointer = Rc::as_ptr(rc).cast::<u8>();
        log::debug!(
            "Rc pointer: {pointer:?} for type {:?}",
            std::any::TypeId::of::<Rc<Value>>()
        );
        if let Some(id) = self.rcs.get(&pointer) {
            log::debug!("Pointing to existing rc, {id}");
            serpentine_internal::write_u64_variable_length(&mut self.file, *id).await?;
        } else {
            serpentine_internal::write_u64_variable_length(&mut self.file, 0).await?;
            writer(self, rc).await?;

            let id = self.next_rc_id;
            self.next_rc_id = self.next_rc_id.saturating_add(1);
            self.rcs.insert(pointer, id);
            log::debug!("New rc, using id {id}");
        }

        Ok(())
    }
}

impl<T> Deref for CacheWriter<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.file
    }
}
impl<T> DerefMut for CacheWriter<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.file
    }
}

/// Struct for reading the cache data, provides a thin wrapper over `AsyncRead` reader, as well as
/// methods for dealing with `Rc`
pub struct CacheReader<T> {
    /// The file to read from
    file: T,
    /// The map from ids to rcs, the trait object is the `Rc`.
    /// This is because we can not convert `Rc<str>` to `Rc<dyn Any>` since string isnt sized.
    /// So instead we convert `Box<Rc<str>>` into `Box<dyn Any>`
    ///
    /// Ids are expected to be sequential
    rcs: Vec<Box<dyn Any>>,
}

impl<T: AsyncRead + Unpin + Send> CacheReader<T> {
    /// Create a new cache reader for the given async reader
    pub fn new(file: T) -> Self {
        Self {
            file,
            rcs: Vec::new(),
        }
    }

    /// Read the cache from this reader
    async fn read_map(
        &mut self,

        external: &impl ExternalCache,
    ) -> Result<CacheHashMap, RuntimeError> {
        log::info!("Attempting to load cache");
        let version = self.file.read_u8().await?;
        if version != CACHE_COMPATIBILITY_VERSION {
            return Err(RuntimeError::CacheOutOfDate {
                got: version,
                current: CACHE_COMPATIBILITY_VERSION,
            });
        }

        let mut map = CacheHashMap::default();
        let count_items = serpentine_internal::read_u64_length_encoded(&mut self.file).await?;
        for _ in 0..count_items {
            let mut hash = [0; blake3::OUT_LEN];
            self.file.read_exact(&mut hash).await?;
            let value = Data::read(self).await?;
            map.insert(CacheHash(hash), value);
        }

        let mut has_standalone_cache = [0_u8; 1];
        self.file.read_exact(&mut has_standalone_cache).await?;
        let has_standalone_cache = has_standalone_cache[0];

        if has_standalone_cache == 1 {
            log::info!("Loading standalone cache");
            external.import(&mut self.file).await?;
        } else {
            log::info!("No standalone cache found.");
            debug_assert_eq!(has_standalone_cache, 0, "hash_standalone_cache not 0 or 1");
        }

        Ok(map)
    }

    /// Read the next part of the file as a Rc of type `R`, this will de-duplicate rcs pointing to
    /// the same data.
    ///
    /// If the rc hasnt been parsed yet calls `reader`
    pub async fn read_rc<Func, Res>(&mut self, reader: Func) -> Result<Rc<Res>, RuntimeError>
    where
        Func: for<'this> FnOnce(
            &'this mut Self,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<Rc<Res>, RuntimeError>> + 'this>,
        >,
        Res: Any + ?Sized + 'static,
    {
        let id = serpentine_internal::read_u64_length_encoded(&mut self.file).await?;
        log::debug!(
            "Reading {:?} with id {id}",
            std::any::TypeId::of::<Rc<Res>>()
        );

        if id == 0 {
            let value = reader(self).await?;
            self.rcs.push(Box::new(Rc::clone(&value)));
            log::debug!("New rc, reading to id {}", self.rcs.len());

            Ok(value)
        } else {
            let id: usize = id
                .try_into()
                .map_err(|_| RuntimeError::internal("Rc id overflows usize for this platform"))?;

            let Some(rc) = self.rcs.get(id.saturating_sub(1)) else {
                return Err(RuntimeError::internal("Rc id out of bounds"));
            };

            let Some(rc) = rc.downcast_ref() else {
                return Err(RuntimeError::internal(format!(
                    "Rc type mismatch, expected {:?} got {:?}",
                    std::any::TypeId::of::<Rc<Res>>(),
                    (**rc).type_id()
                )));
            };

            Ok(Rc::clone(rc))
        }
    }
}

impl<T> Deref for CacheReader<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.file
    }
}
impl<T> DerefMut for CacheReader<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.file
    }
}

/// Trait for structs and types that will need to be written to and from cache storage.
pub trait CacheData: Sized {
    /// Parse this value from the given reader.
    async fn read(
        reader: &mut CacheReader<impl AsyncRead + Unpin + Send>,
    ) -> Result<Self, RuntimeError>;

    /// Write this value to the reader.
    async fn write(
        &self,
        writer: &mut CacheWriter<impl AsyncWrite + Unpin + Send>,
    ) -> Result<(), RuntimeError>;

    /// Hash this value
    ///
    /// Enums snould make sure to include their discriminants.
    async fn content_hash(&self, hasher: &mut blake3::Hasher) -> Result<(), RuntimeError>;
}

impl CacheData for Rc<[u8]> {
    async fn read(
        reader: &mut CacheReader<impl AsyncRead + Unpin + Send>,
    ) -> Result<Self, RuntimeError> {
        reader
            .read_rc(|reader| {
                Box::pin(async move {
                    let data = serpentine_internal::read_length_prefixed(&mut **reader).await?;
                    Ok(data.into())
                })
            })
            .await
    }
    async fn write(
        &self,
        writer: &mut CacheWriter<impl AsyncWrite + Unpin + Send>,
    ) -> Result<(), RuntimeError> {
        writer
            .write_rc(self, |writer, this| {
                Box::pin(async move {
                    serpentine_internal::write_length_prefixed(&mut **writer, this).await?;

                    Ok(())
                })
            })
            .await
    }

    async fn content_hash(&self, hasher: &mut blake3::Hasher) -> Result<(), RuntimeError> {
        hasher.update(&(self.len() as u64).to_le_bytes());
        hasher.update(self);

        Ok(())
    }
}
impl CacheData for Rc<str> {
    async fn read(
        reader: &mut CacheReader<impl AsyncRead + Unpin + Send>,
    ) -> Result<Self, RuntimeError> {
        reader
            .read_rc(|reader| {
                Box::pin(async move {
                    let data =
                        serpentine_internal::read_length_prefixed_string(&mut **reader).await?;

                    Ok(data.into())
                })
            })
            .await
    }
    async fn write(
        &self,
        writer: &mut CacheWriter<impl AsyncWrite + Unpin + Send>,
    ) -> Result<(), RuntimeError> {
        writer
            .write_rc(self, |writer, this| {
                Box::pin(async move {
                    serpentine_internal::write_length_prefixed(&mut **writer, this.as_bytes())
                        .await?;

                    Ok(())
                })
            })
            .await
    }

    async fn content_hash(&self, hasher: &mut blake3::Hasher) -> Result<(), RuntimeError> {
        hasher.update(&(self.len() as u64).to_le_bytes());
        hasher.update(self.as_bytes());

        Ok(())
    }
}

impl<T: CacheData + Clone + 'static> CacheData for Rc<T> {
    async fn read(
        reader: &mut CacheReader<impl AsyncRead + Unpin + Send>,
    ) -> Result<Self, RuntimeError> {
        reader
            .read_rc(|reader| Box::pin(async move { Ok(Rc::new(T::read(reader).await?)) }))
            .await
    }
    async fn write(
        &self,
        writer: &mut CacheWriter<impl AsyncWrite + Unpin + Send>,
    ) -> Result<(), RuntimeError> {
        writer
            .write_rc(self, |writer, this| Box::pin(T::write(this, writer)))
            .await
    }

    async fn content_hash(&self, hasher: &mut blake3::Hasher) -> Result<(), RuntimeError> {
        T::content_hash(self, hasher).await
    }
}

/// A key into the cache
#[derive(Debug, PartialEq, Eq)]
pub struct CacheKey<'caller> {
    /// The kind of node
    pub node: NodeKindId,
    /// The inputs to the node
    pub inputs: &'caller [&'caller Data],
}

impl CacheKey<'_> {
    /// Hash this key
    pub async fn content_hash(&self) -> Result<CacheHash, RuntimeError> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.node.index().to_le_bytes());
        hasher.update(&(self.inputs.len() as u64).to_le_bytes());

        for input in self.inputs {
            input.content_hash(&mut hasher).await?;
        }

        Ok(CacheHash(hasher.finalize().into()))
    }
}

/// A external cache is data stored in another service like a docker volume that our cache system
/// needs to take into account.
pub trait ExternalCache {
    /// Export data from the external cache to this file
    async fn export(
        &self,
        values: impl IntoIterator<Item = &Data>,
        file: &mut (impl AsyncWrite + Unpin + Send),
    ) -> Result<(), RuntimeError>;

    /// Import data from the given file to this external cache
    async fn import(&self, file: &mut (impl AsyncRead + Unpin + Send)) -> Result<(), RuntimeError>;

    /// Delete the given data from this external cache
    async fn cleanup(&self, data: Data);
}

/// A hashmap storing the cache data
type CacheHashMap = nohash::IntMap<CacheHash, Data>;

/// A content addressable cache using sha256
/// And allows serializing to disk
pub struct Cache {
    /// The cache that was loaded from disk, might not be serialized.
    old_cache: CacheHashMap,
    /// The cache generated from this run.
    new_cache: CacheHashMap,
}

impl Cache {
    /// Create a new empty cache
    pub fn new() -> Self {
        Self {
            old_cache: CacheHashMap::default(),
            new_cache: CacheHashMap::default(),
        }
    }

    /// Load the cache from the given path.
    pub async fn load_cache(
        file: &mut (impl AsyncRead + Unpin + Send),
        external: &impl ExternalCache,
    ) -> Result<Self, RuntimeError> {
        Ok(Self {
            old_cache: CacheReader::new(file).read_map(external).await?,
            new_cache: CacheHashMap::default(),
        })
    }

    /// Write the cache to disk.
    ///
    /// If `keep_old_cache` is false will only write caches generated from this session.
    /// Returns a vector of stale data that can be safely deleted.
    pub async fn save_cache(
        self,
        file: &mut (impl AsyncWrite + Unpin + Send),
        external: &impl ExternalCache,
        keep_old_cache: bool,
        export_standalone: bool,
    ) -> Result<(), RuntimeError> {
        log::info!("Saving cache");

        let Self {
            old_cache,
            new_cache: mut cache,
        } = self;

        if keep_old_cache {
            // Entries in old_cache can overwrite entries in new cache.
            // But it would be a larger bug if the two values werent semantically equivalent.
            cache.extend(old_cache);
        } else {
            let in_use: HashSet<_> = cache.values().collect();
            for value in old_cache
                .into_values()
                .filter(|value| !in_use.contains(value))
            {
                external.cleanup(value).await;
            }
        }

        CacheWriter::new(file)
            .write_map(cache, external, export_standalone)
            .await?;

        Ok(())
    }

    /// Store a value in the cache
    pub fn insert(&mut self, key: CacheHash, value: Data) {
        log::debug!("Saving {key:?}={value:?} in cache");
        self.new_cache.insert(key, value);
    }

    /// Get a value from the cache
    ///
    /// This also moves the value from `old_cache` to `new_cache`
    pub fn get(&mut self, key: CacheHash) -> Option<&Data> {
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
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests")]
mod tests {
    use rstest::{fixture, rstest};

    use super::*;

    struct DummyExternal;

    impl ExternalCache for DummyExternal {
        async fn export(
            &self,
            _values: impl IntoIterator<Item = &Data>,
            _file: &mut (impl AsyncWrite + Unpin + Send),
        ) -> Result<(), RuntimeError> {
            Ok(())
        }

        async fn import(
            &self,
            _file: &mut (impl AsyncRead + Unpin + Send),
        ) -> Result<(), RuntimeError> {
            Ok(())
        }

        async fn cleanup(&self, _data: Data) {}
    }

    #[fixture]
    fn external() -> impl ExternalCache {
        DummyExternal
    }

    #[tokio::test]
    #[rstest]
    #[proptest::property_test(config = proptest::prelude::ProptestConfig {cases: 100, ..Default::default()})]
    #[test_log::test]

    async fn different_entries_hash_differently(
        #[ignore] node: NodeKindId,
        #[ignore] data1: Vec<Data>,
        #[ignore] data2: Vec<Data>,
    ) {
        let data1 = data1.iter().collect::<Vec<_>>();
        let key1 = CacheKey {
            node,
            inputs: &data1,
        };

        let data2 = data2.iter().collect::<Vec<_>>();
        let key2 = CacheKey {
            node,
            inputs: &data2,
        };

        let hash_1 = key1.content_hash().await.unwrap();
        let hash_2 = key2.content_hash().await.unwrap();

        if key1 != key2 {
            assert_ne!(hash_1, hash_2, "Keys different expected different hash.");
        } else {
            assert_eq!(hash_1, hash_2, "Keys equal expected same hash.");
        }
    }

    #[tokio::test]
    #[rstest]
    #[proptest::property_test]
    #[test_log::test]
    async fn same_entry_hashes_equal(#[ignore] node: NodeKindId, #[ignore] data: Vec<Data>) {
        let data = data.iter().collect::<Vec<_>>();
        let key = CacheKey {
            node,
            inputs: &data,
        };

        assert_eq!(
            key.content_hash().await.unwrap(),
            key.content_hash().await.unwrap()
        );
    }

    #[tokio::test]
    #[rstest]
    #[proptest::property_test]
    #[test_log::test]
    async fn save_and_load_one_entry(
        external: impl ExternalCache,
        #[ignore] node: NodeKindId,
        #[ignore] data: Vec<Data>,
        #[ignore] value: Data,
    ) {
        let data = data.iter().collect::<Vec<_>>();
        let key = CacheKey {
            node,
            inputs: &data,
        };

        let mut cache = Cache::new();
        cache.insert(key.content_hash().await.unwrap(), value.clone());

        let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
        cache
            .save_cache(&mut cache_file, &external, false, false)
            .await
            .unwrap();

        cache_file.set_position(0);
        let mut loaded_cache = Cache::load_cache(&mut cache_file, &external).await.unwrap();
        let loaded_value = loaded_cache
            .get(key.content_hash().await.unwrap())
            .expect("Value not found");

        assert_eq!(*loaded_value, value);
    }

    #[tokio::test]
    #[rstest]
    #[proptest::property_test]
    #[test_log::test]
    async fn save_and_load_duplicate(external: impl ExternalCache, #[ignore] value: Data) {
        let mut cache = Cache::new();

        let key1 = CacheHash(blake3::hash(&[0]).into());
        let key2 = CacheHash(blake3::hash(&[1]).into());
        let key3 = CacheHash(blake3::hash(&[2]).into());

        cache.insert(key1, value.clone());
        cache.insert(key2, value.clone());
        cache.insert(key3, value.clone());

        let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
        cache
            .save_cache(&mut cache_file, &external, false, false)
            .await
            .unwrap();

        cache_file.set_position(0);
        let mut loaded_cache = Cache::load_cache(&mut cache_file, &external).await.unwrap();
        let loaded_value1 = loaded_cache.get(key1).expect("Value not found").clone();
        let loaded_value2 = loaded_cache.get(key1).expect("Value not found").clone();
        let loaded_value3 = loaded_cache.get(key1).expect("Value not found").clone();

        assert_eq!(loaded_value1, value);
        assert_eq!(loaded_value2, value);
        assert_eq!(loaded_value3, value);
    }

    #[tokio::test]
    #[rstest]
    #[test_log::test]
    #[expect(clippy::panic, reason = "tests")]
    async fn save_and_load_duplicate_rc_is_deduplicated(external: impl ExternalCache) {
        let mut cache = Cache::new();

        let value = Data::String(Rc::from("foo"));

        let key1 = CacheHash(blake3::hash(&[0]).into());
        let key2 = CacheHash(blake3::hash(&[1]).into());

        cache.insert(key1, value.clone());
        cache.insert(key2, value.clone());

        let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
        cache
            .save_cache(&mut cache_file, &external, false, false)
            .await
            .unwrap();

        cache_file.set_position(0);
        let mut loaded_cache = Cache::load_cache(&mut cache_file, &external).await.unwrap();
        let loaded_value1 = loaded_cache.get(key1).expect("Value not found").clone();
        let loaded_value2 = loaded_cache.get(key1).expect("Value not found").clone();

        let Data::String(value1) = loaded_value1 else {
            panic!("Unexpected enum variant");
        };
        let Data::String(value2) = loaded_value2 else {
            panic!("Unexpected enum variant");
        };

        assert!(
            Rc::ptr_eq(&value1, &value2),
            "Rcs point to different allocations despite being serialized from the same rc allocation."
        );
    }

    #[tokio::test]
    #[rstest]
    #[proptest::property_test(config = proptest::prelude::ProptestConfig {cases: 5, ..Default::default()})]
    #[test_log::test]
    async fn save_and_load_multiple_entries(
        external: impl ExternalCache,
        #[ignore] values: Vec<(NodeKindId, Vec<Data>, Data)>,
    ) {
        let mut cache = Cache::new();
        for (node, data, value) in &values {
            let data = data.iter().collect::<Vec<_>>();
            let key = CacheKey {
                node: *node,
                inputs: &data,
            };

            cache.insert(key.content_hash().await.unwrap(), value.clone());
        }

        let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
        cache
            .save_cache(&mut cache_file, &external, false, false)
            .await
            .unwrap();

        cache_file.set_position(0);
        let mut loaded_cache = Cache::load_cache(&mut cache_file, &external).await.unwrap();

        for (node, data, _) in &values {
            let data = data.iter().collect::<Vec<_>>();
            let key = CacheKey {
                node: *node,
                inputs: &data,
            };

            let _ = loaded_cache
                .get(key.content_hash().await.unwrap())
                .expect("Value not found");
            // We do not check what the value is as proptest might (and likely will) generate
            // duplicate keys.
        }
    }

    /// If a entry in the old cache is used then it should be kept even if `keep_old_cache` is false.
    /// As `keep_old_cache=false` is for cleaning up cache not used/generated this session.
    #[tokio::test]
    #[rstest]
    #[proptest::property_test]
    #[test_log::test]
    async fn if_cache_used_should_always_be_kept(
        external: impl ExternalCache,
        #[ignore] node: NodeKindId,
        #[ignore] data: Vec<Data>,
        #[ignore] value: Data,
    ) {
        let data = data.iter().collect::<Vec<_>>();
        let key = CacheKey {
            node,
            inputs: &data,
        };

        let mut cache = Cache::new();
        cache.insert(key.content_hash().await.unwrap(), value.clone());

        let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
        cache
            .save_cache(&mut cache_file, &external, false, false)
            .await
            .unwrap();

        cache_file.set_position(0);
        let mut loaded_cache = Cache::load_cache(&mut cache_file, &external).await.unwrap();
        loaded_cache
            .get(key.content_hash().await.unwrap())
            .expect("Value not found");

        // Even tho `keep_old_cache` is false it should still keep the entry in there since we used
        // it.
        cache_file.set_position(0);
        cache_file.get_mut().clear();
        loaded_cache
            .save_cache(&mut cache_file, &external, false, false)
            .await
            .unwrap();

        cache_file.set_position(0);
        let mut second_loaded_cache = Cache::load_cache(&mut cache_file, &external).await.unwrap();
        second_loaded_cache
            .get(key.content_hash().await.unwrap())
            .expect("Value not found");
    }

    #[tokio::test]
    #[rstest]
    #[proptest::property_test]
    #[test_log::test]
    async fn old_entry_cleared_if_not_used(
        external: impl ExternalCache,
        #[ignore] node: NodeKindId,
        #[ignore] data: Vec<Data>,
        #[ignore] value: Data,
    ) {
        let data = data.iter().collect::<Vec<_>>();
        let key = CacheKey {
            node,
            inputs: &data,
        };

        let mut cache = Cache::new();
        cache.insert(key.content_hash().await.unwrap(), value.clone());

        let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
        cache
            .save_cache(&mut cache_file, &external, false, false)
            .await
            .unwrap();

        cache_file.set_position(0);
        let loaded_cache = Cache::load_cache(&mut cache_file, &external).await.unwrap();

        cache_file.set_position(0);
        cache_file.get_mut().clear();
        loaded_cache
            .save_cache(&mut cache_file, &external, false, false)
            .await
            .unwrap();

        cache_file.set_position(0);
        let mut second_loaded_cache = Cache::load_cache(&mut cache_file, &external).await.unwrap();
        let result = second_loaded_cache.get(key.content_hash().await.unwrap());
        assert!(result.is_none(), "unused old_cache value was saved.");
    }

    #[tokio::test]
    #[rstest]
    #[proptest::property_test]
    #[test_log::test]
    async fn old_entry_kept_if_keep_old_true_even_if_not_used(
        external: impl ExternalCache,
        #[ignore] node: NodeKindId,
        #[ignore] data: Vec<Data>,
        #[ignore] value: Data,
    ) {
        let data = data.iter().collect::<Vec<_>>();
        let key = CacheKey {
            node,
            inputs: &data,
        };

        let mut cache = Cache::new();
        cache.insert(key.content_hash().await.unwrap(), value.clone());

        let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
        cache
            .save_cache(&mut cache_file, &external, false, false)
            .await
            .unwrap();

        cache_file.set_position(0);
        let loaded_cache = Cache::load_cache(&mut cache_file, &external).await.unwrap();

        cache_file.set_position(0);
        cache_file.get_mut().clear();
        loaded_cache
            .save_cache(&mut cache_file, &external, true, false)
            .await
            .unwrap();

        cache_file.set_position(0);
        let mut second_loaded_cache = Cache::load_cache(&mut cache_file, &external).await.unwrap();
        second_loaded_cache
            .get(key.content_hash().await.unwrap())
            .expect("Value not found");
    }
}
