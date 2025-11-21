//! A content addressable cache.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::Path;

use sha2::Digest;

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
const CACHE_COMPATIBILITY_VERSION: u8 = 0;

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
        let hash = sha2::Sha256::digest(&bincode::encode_to_vec(self, config)?);
        Ok(hash.into())
    }
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
    pub fn load_cache(cache_file: &Path) -> Result<Self, RuntimeError> {
        log::info!("Attempting to load cache from {}", cache_file.display());
        let mut file = std::fs::File::open(cache_file)?;

        let mut version = [0];
        file.read_exact(&mut version)?;

        if version[0] != CACHE_COMPATIBILITY_VERSION {
            return Err(RuntimeError::CacheOutOfDate {
                got: version[0],
                current: CACHE_COMPATIBILITY_VERSION,
            });
        }

        let old_cache = bincode::decode_from_std_read(&mut file, bincode_config())?;

        Ok(Self {
            old_cache,
            new_cache: CacheHashMap::new(),
        })
    }

    /// Write the cache to disk.
    ///
    /// If `keep_old_cache` is false will only write caches generated from this session.
    /// Returns a vector of stale data that can be safely deleted.
    pub fn save_cache(
        self,
        cache_file: &Path,
        keep_old_cache: bool,
    ) -> Result<Vec<Data>, RuntimeError> {
        log::info!("Saving cache to {}", cache_file.display());
        if let Some(parent) = cache_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::File::create(cache_file)?;
        file.write_all(&[CACHE_COMPATIBILITY_VERSION])?;

        let Self {
            old_cache,
            new_cache: mut cache,
        } = self;

        let stale_data = if keep_old_cache {
            // Entries in old_cache can overwrite entries in new cache.
            // But it would be a larger bug if the two values werent semantically equivalent.
            cache.extend(old_cache);
            vec![]
        } else {
            let in_use: HashSet<_> = cache.values().collect();
            old_cache
                .into_values()
                .filter(|value| !in_use.contains(value))
                .collect()
        };

        bincode::encode_into_std_write(cache, &mut file, bincode_config())?;

        Ok(stale_data)
    }

    /// Store a value in the cache
    pub fn insert(&mut self, key: [u8; 32], value: Data) {
        log::debug!("Saving {key:?}={value:?} in cache");
        self.new_cache.insert(key, value);
    }

    /// Get a value from the cache
    ///
    /// This also moves the value from `old_cache` to `new_cache`
    pub fn get(&mut self, key: &CacheKey<'_>) -> Result<Option<&Data>, RuntimeError> {
        let key = key.sha256()?;
        log::debug!("Reading {key:?}");
        if let Some(data) = self.old_cache.remove(&key) {
            log::debug!("Got {data:?}, moving to new_cache");
            let data = self.new_cache.entry(key).insert_entry(data).into_mut();
            Ok(Some(data))
        } else if let Some(data) = self.new_cache.get(&key) {
            log::debug!("Got {data:?}");
            Ok(Some(data))
        } else {
            log::debug!("Key {key:?} not in cache");
            Ok(None)
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    #[proptest::property_test]
    #[test_log::test]
    fn save_and_load_one_entry(node: NodeKindId, data: Vec<Data>, value: Data) {
        let data = data.iter().collect::<Vec<_>>();
        let key = CacheKey {
            node,
            inputs: &data,
        };

        let mut cache = Cache::new();
        cache.insert(key.sha256()?, value.clone());

        let cache_file = tempfile::NamedTempFile::new()?;
        let cache_file = cache_file.path();
        cache.save_cache(cache_file, false)?;

        let mut loaded_cache = Cache::load_cache(cache_file)?;
        let loaded_value = loaded_cache.get(&key)?.expect("Value not found");

        assert_eq!(*loaded_value, value);
    }

    #[proptest::property_test(config = proptest::prelude::ProptestConfig {cases: 50, ..Default::default()})]
    #[test_log::test]
    fn save_and_load_multiple_entries(values: Vec<(NodeKindId, Vec<Data>, Data)>) {
        let mut cache = Cache::new();
        for (node, data, value) in &values {
            let data = data.iter().collect::<Vec<_>>();
            let key = CacheKey {
                node: *node,
                inputs: &data,
            };

            cache.insert(key.sha256()?, value.clone());
        }

        let cache_file = tempfile::NamedTempFile::new()?;
        let cache_file = cache_file.path();
        cache.save_cache(cache_file, false)?;

        let mut loaded_cache = Cache::load_cache(cache_file)?;

        for (node, data, _) in &values {
            let data = data.iter().collect::<Vec<_>>();
            let key = CacheKey {
                node: *node,
                inputs: &data,
            };

            let _ = loaded_cache.get(&key)?.expect("Value not found");
            // We do not check what the value is as proptest might (and likely will) generate
            // duplicate keys.
        }
    }

    /// If a entry in the old cache is used then it should be kept even if `keep_old_cache` is false.
    /// As `keep_old_cache=false` is for cleaning up cache not used/generated this session.
    #[proptest::property_test]
    #[test_log::test]
    fn if_cache_used_should_always_be_kept(node: NodeKindId, data: Vec<Data>, value: Data) {
        let data = data.iter().collect::<Vec<_>>();
        let key = CacheKey {
            node,
            inputs: &data,
        };

        let mut cache = Cache::new();
        cache.insert(key.sha256()?, value.clone());

        let cache_file = tempfile::NamedTempFile::new()?;
        let cache_file = cache_file.path();
        cache.save_cache(cache_file, false)?;

        let mut loaded_cache = Cache::load_cache(cache_file)?;
        loaded_cache.get(&key)?.expect("Value not found");

        // Even tho `keep_old_cache` is false it should still keep the entry in there since we used
        // it.
        loaded_cache.save_cache(cache_file, false)?;

        let mut second_loaded_cache = Cache::load_cache(cache_file)?;
        second_loaded_cache.get(&key)?.expect("Value not found");
    }

    #[proptest::property_test]
    #[test_log::test]
    fn old_entry_cleared_if_not_used(node: NodeKindId, data: Vec<Data>, value: Data) {
        let data = data.iter().collect::<Vec<_>>();
        let key = CacheKey {
            node,
            inputs: &data,
        };

        let mut cache = Cache::new();
        cache.insert(key.sha256()?, value.clone());

        let cache_file = tempfile::NamedTempFile::new()?;
        let cache_file = cache_file.path();
        cache.save_cache(cache_file, false)?;

        let loaded_cache = Cache::load_cache(cache_file)?;
        loaded_cache.save_cache(cache_file, false)?;

        let mut second_loaded_cache = Cache::load_cache(cache_file)?;
        let result = second_loaded_cache.get(&key)?;
        assert!(result.is_none(), "unused old_cache value was saved.");
    }

    #[proptest::property_test]
    #[test_log::test]
    fn old_entry_kept_if_keep_old_true_even_if_not_used(
        node: NodeKindId,
        data: Vec<Data>,
        value: Data,
    ) {
        let data = data.iter().collect::<Vec<_>>();
        let key = CacheKey {
            node,
            inputs: &data,
        };

        let mut cache = Cache::new();
        cache.insert(key.sha256()?, value.clone());

        let cache_file = tempfile::NamedTempFile::new()?;
        let cache_file = cache_file.path();
        cache.save_cache(cache_file, false)?;

        let loaded_cache = Cache::load_cache(cache_file)?;
        loaded_cache.save_cache(cache_file, true)?;

        let mut second_loaded_cache = Cache::load_cache(cache_file)?;
        second_loaded_cache.get(&key)?.expect("Value not found");
    }

    /// If a entry in the old cache is used then it should be kept even if `keep_old_cache` is false.
    /// As `keep_old_cache=false` is for cleaning up cache not used/generated this session.
    #[proptest::property_test]
    #[test_log::test]
    fn stale_entries_doesnt_include_re_generated_values(
        node_1: NodeKindId,
        data_1: Vec<Data>,
        node_2: NodeKindId,
        data_2: Vec<Data>,
        value: Data,
    ) {
        let data_1 = data_1.iter().collect::<Vec<_>>();
        let key_1 = CacheKey {
            node: node_1,
            inputs: &data_1,
        };
        let data_2 = data_2.iter().collect::<Vec<_>>();
        let key_2 = CacheKey {
            node: node_2,
            inputs: &data_2,
        };

        let mut cache = Cache::new();
        cache.insert(key_1.sha256()?, value.clone());

        let cache_file = tempfile::NamedTempFile::new()?;
        let cache_file = cache_file.path();
        cache.save_cache(cache_file, false)?;

        let mut loaded_cache = Cache::load_cache(cache_file)?;
        loaded_cache.insert(key_2.sha256()?, value);
        let stale = loaded_cache.save_cache(cache_file, false)?;
        assert_eq!(stale.len(), 0, "Data should not have been in stale vec");
    }

    /// If a entry in the old cache is used then it should be kept even if `keep_old_cache` is false.
    /// As `keep_old_cache=false` is for cleaning up cache not used/generated this session.
    #[proptest::property_test]
    #[test_log::test]
    fn stale_entries_returned(
        node_1: NodeKindId,
        data_1: Vec<Data>,
        node_2: NodeKindId,
        data_2: Vec<Data>,
        value_1: Data,
        value_2: Data,
    ) {
        proptest::prop_assume!(value_1 != value_2);

        let data_1 = data_1.iter().collect::<Vec<_>>();
        let key_1 = CacheKey {
            node: node_1,
            inputs: &data_1,
        };
        let data_2 = data_2.iter().collect::<Vec<_>>();
        let key_2 = CacheKey {
            node: node_2,
            inputs: &data_2,
        };

        let mut cache = Cache::new();
        cache.insert(key_1.sha256()?, value_1.clone());
        cache.insert(key_2.sha256()?, value_2.clone());

        let cache_file = tempfile::NamedTempFile::new()?;
        let cache_file = cache_file.path();
        cache.save_cache(cache_file, false)?;

        let mut loaded_cache = Cache::load_cache(cache_file)?;
        loaded_cache.get(&key_1)?;
        let stale = loaded_cache.save_cache(cache_file, false)?;
        assert_eq!(stale, vec![value_2], "value_2 should be in stale entries.");
    }
}
