//! A content addressable cache.

use std::collections::{HashMap, HashSet};

use sha2::Digest;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::engine::data_model::{Data, NodeKindId};
use crate::engine::{RuntimeError, containerd};

// ==== CACHE FILE FORMAT ====
// The first byte is the `CACHE_COMPATIBILITY_VERSION`, written as a pure u8.
// Followed by a `u64` in big-endian denoting the following sections size.
// Followed by a `CacheHashMap` encoded using `bincode`
//
// Then if the cache contains a standalone cache the next byte is a `1`, otherwise its a `0`.
// if there is a standalone cache then the rest of then the bytes of the docker export follows.

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
const CACHE_COMPATIBILITY_VERSION: u8 = 1;

/// The bincode config to use
const fn bincode_config() -> impl bincode::config::Config {
    bincode::config::standard()
}

/// A key into the cache
#[derive(bincode::Encode, Debug)]
pub struct CacheKey<'caller> {
    /// The kind of node
    pub node: NodeKindId,
    /// The inputs to the node
    pub inputs: &'caller [&'caller Data],
}

impl CacheKey<'_> {
    /// Hash this key with sha256 by encoding it to bincode
    pub fn sha256(&self) -> Result<[u8; 32], RuntimeError> {
        let config = bincode_config();
        let hash = sha2::Sha256::digest(
            &bincode::encode_to_vec(self, config)
                .map_err(|err| RuntimeError::internal(err.to_string()))?,
        );
        Ok(hash.into())
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
type CacheHashMap = HashMap<[u8; 32], Data>;

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
            old_cache: CacheHashMap::new(),
            new_cache: CacheHashMap::new(),
        }
    }

    /// Load the cache from the given path.
    pub async fn load_cache(
        file: &mut (impl AsyncRead + Unpin + Send),
        external: &impl ExternalCache,
    ) -> Result<Self, RuntimeError> {
        log::info!("Attempting to load cache");
        let version = file.read_u8().await?;
        if version != CACHE_COMPATIBILITY_VERSION {
            return Err(RuntimeError::CacheOutOfDate {
                got: version,
                current: CACHE_COMPATIBILITY_VERSION,
            });
        }

        let cache_size = file.read_u64().await?;

        let mut cache_data =
            vec![0; cache_size.try_into().unwrap_or(usize::MAX)].into_boxed_slice();
        file.read_exact(&mut cache_data).await?;
        let old_cache = bincode::decode_from_std_read(&mut &*cache_data, bincode_config())
            .map_err(|err| {
                if let bincode::error::DecodeError::Io { inner, .. } = err {
                    RuntimeError::IoError(inner)
                } else {
                    RuntimeError::internal(err.to_string())
                }
            })?;

        let mut has_standalone_cache = [0_u8; 1];
        file.read_exact(&mut has_standalone_cache).await?;
        let has_standalone_cache = has_standalone_cache[0];

        if has_standalone_cache == 1 {
            log::info!("Loading standalone cache");
            external.import(file).await?;
        } else {
            log::info!("No standalone cache found.");
            debug_assert_eq!(has_standalone_cache, 0, "hash_standalone_cache not 0 or 1");
        }

        Ok(Self {
            old_cache,
            new_cache: CacheHashMap::new(),
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
        file.write_all(&[CACHE_COMPATIBILITY_VERSION]).await?;

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

        let cache_data = bincode::encode_to_vec(&cache, bincode_config())
            .map_err(|err| RuntimeError::internal(err.to_string()))?;
        file.write_all(
            &cache_data
                .len()
                .try_into()
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        )
        .await?;
        file.write_all(&cache_data).await?;

        if export_standalone {
            file.write_all(&[1]).await?;
            log::info!("Exporting standalone cache");
            external.export(cache.values(), file);
        } else {
            file.write_all(&[0]).await?;
        }

        file.flush().await?;

        Ok(())
    }

    /// Store a value in the cache
    pub fn insert(&mut self, key: [u8; 32], value: Data) {
        log::debug!("Saving {key:?}={value:?} in cache");
        self.new_cache.insert(key, value);
    }

    /// Get a value from the cache
    ///
    /// This also moves the value from `old_cache` to `new_cache`
    pub fn get(&mut self, key: &[u8; 32]) -> Option<&Data> {
        log::debug!("Reading {key:?}");
        if let Some(data) = self.old_cache.remove(key) {
            log::debug!("Got {data:?}, moving to new_cache");
            let data = self.new_cache.entry(*key).insert_entry(data).into_mut();
            Some(data)
        } else if let Some(data) = self.new_cache.get(key) {
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
        cache.insert(key.sha256().unwrap(), value.clone());

        let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
        cache
            .save_cache(&mut cache_file, &external, false, false)
            .await
            .unwrap();

        cache_file.set_position(0);
        let mut loaded_cache = Cache::load_cache(&mut cache_file, &external).await.unwrap();
        let loaded_value = loaded_cache
            .get(&key.sha256().unwrap())
            .expect("Value not found");

        assert_eq!(*loaded_value, value);
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

            cache.insert(key.sha256().unwrap(), value.clone());
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
                .get(&key.sha256().unwrap())
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
        cache.insert(key.sha256().unwrap(), value.clone());

        let mut cache_file = std::io::Cursor::new(Vec::<u8>::new());
        cache
            .save_cache(&mut cache_file, &external, false, false)
            .await
            .unwrap();

        cache_file.set_position(0);
        let mut loaded_cache = Cache::load_cache(&mut cache_file, &external).await.unwrap();
        loaded_cache
            .get(&key.sha256().unwrap())
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
            .get(&key.sha256().unwrap())
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
        cache.insert(key.sha256().unwrap(), value.clone());

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
        let result = second_loaded_cache.get(&key.sha256().unwrap());
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
        cache.insert(key.sha256().unwrap(), value.clone());

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
            .get(&key.sha256().unwrap())
            .expect("Value not found");
    }
}
