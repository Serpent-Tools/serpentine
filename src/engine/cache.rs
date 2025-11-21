//! A content addressable cache.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;

use sha2::Digest;

use crate::engine::RuntimeError;
use crate::engine::data_model::{Data, NodeKindId};

/// Version number for the cache.
///
/// This is not purely the version of the cache struct, but a indicator of the caches validity in
/// general. As such any changes to serpentine that can cause the cache to be invalid must
/// increment this version number. Changes that do not dont have to.
///
/// In general the following changes require modifying the version number:
/// * Modifying the cache structure
/// * Modifying a builtin node in a way that causes changes to the output.
/// * Adding or removing builtin nodes as this can shift the node kind ids.
const CACHE_VERSION: u8 = 0;

/// The bincode config to use
const fn bincode_config() -> impl bincode::config::Config {
    bincode::config::standard()
}

/// A key into the cache
#[derive(bincode::Encode)]
pub struct CacheKey<'caller> {
    /// The kind of node
    node: NodeKindId,
    /// The inputs to the node
    inputs: &'caller [&'caller Data],
}

impl CacheKey<'_> {
    /// Hash this key with sha256 by encoding it to bincode
    fn sha256(&self) -> Result<[u8; 32], RuntimeError> {
        let config = bincode_config();
        let hash = sha2::Sha256::digest(&bincode::encode_to_vec(self, config)?);
        Ok(hash.into())
    }
}

/// A hashmap for the cache.
///
/// Holds `Data` based on sha256 hashes.
#[derive(bincode::Encode, bincode::Decode)]
struct CacheHashMap(HashMap<[u8; 32], Data>);

impl CacheHashMap {
    /// Create a new hashmap
    fn new() -> Self {
        CacheHashMap(HashMap::new())
    }

    /// Insert a value into the hashmap
    fn insert(&mut self, key: &CacheKey<'_>, value: Data) -> Result<(), RuntimeError> {
        let hash = key.sha256()?;
        log::debug!("Commiting {value:?} to cache under {hash:?}");
        self.0.insert(hash, value);
        Ok(())
    }

    /// Get a value from the hashmap.
    fn get(&self, key: &CacheKey<'_>) -> Result<Option<&Data>, RuntimeError> {
        let hash = key.sha256()?;
        log::debug!("Looking up {hash:?} in cache");
        Ok(self.0.get(&hash))
    }

    /// Extend another cache hashmap into this one
    fn consume(&mut self, other: Self) {
        self.0.extend(other.0);
    }
}

/// A content addressable cache using sha256
/// And allows serializing to disk
pub struct Cache {
    /// The cache that was loaded from disk, might not be seralized.
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

        if version[0] != CACHE_VERSION {
            return Err(RuntimeError::CacheOutOfDate {
                got: version[0],
                current: CACHE_VERSION,
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
    pub fn save_cache(self, cache_file: &Path, keep_old_cache: bool) -> Result<(), RuntimeError> {
        let mut file = std::fs::File::create(cache_file)?;
        file.write_all(&[CACHE_VERSION])?;

        let Self {
            old_cache,
            new_cache: mut cache,
        } = self;

        if keep_old_cache {
            cache.consume(old_cache);
        }

        bincode::encode_into_std_write(cache, &mut file, bincode_config())?;

        Ok(())
    }

    /// Store a value in the cache
    pub fn insert(&mut self, key: &CacheKey<'_>, value: Data) -> Result<(), RuntimeError> {
        self.new_cache.insert(key, value)?;

        Ok(())
    }

    /// Get a value from the cache
    pub fn get(&mut self, key: &CacheKey<'_>) -> Result<Option<&Data>, RuntimeError> {
        if let Some(data) = self.old_cache.get(key)? {
            Ok(Some(data))
        } else if let Some(data) = self.new_cache.get(key)? {
            log::warn!(
                "Cache hit in new cache, this meant the same value got generated twice without triggering the graph optimizer, this generally points to two different code paths doing the same thing in slightly different things."
            );
            Ok(Some(data))
        } else {
            Ok(None)
        }
    }
}
