//! A content addressable cache.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::Path;

use sha2::Digest;

use crate::engine::RuntimeError;
use crate::engine::data_model::{Data, NodeKindId};
use crate::engine::docker::DockerClient;

// ==== CACHE FILE FORMAT ====
// The first byte is the `CACHE_COMPATIBILITY_VERSION`, written as a pure u8.
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
    pub async fn load_cache(
        cache_file: &Path,
        docker: &DockerClient,
    ) -> Result<Self, RuntimeError> {
        log::info!("Attempting to load cache from {}", cache_file.display());
        let mut file = std::fs::File::open(cache_file)?;
        file.lock()?;

        let mut version = [0];
        file.read_exact(&mut version)?;

        if version[0] != CACHE_COMPATIBILITY_VERSION {
            return Err(RuntimeError::CacheOutOfDate {
                got: version[0],
                current: CACHE_COMPATIBILITY_VERSION,
            });
        }

        let old_cache = bincode::decode_from_std_read(&mut file, bincode_config())?;

        let mut has_standalone_cache = [0; 1];
        file.read_exact(&mut has_standalone_cache)?;
        if has_standalone_cache[0] == 1 {
            log::info!("Loading standalone cache");
            docker.import(file).await?;
        } else {
            log::info!("No standalone cache found.");
            debug_assert_eq!(
                has_standalone_cache[0], 0,
                "hash_standalone_cache not 0 or 1"
            );
            debug_assert_eq!(file.read(&mut [0; 1])?, 0, "Not at end of file");
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
        cache_file: &Path,
        docker: &DockerClient,
        keep_old_cache: bool,
        export_standalone: bool,
    ) -> Result<(), RuntimeError> {
        log::info!("Saving cache to {}", cache_file.display());
        if let Some(parent) = cache_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::File::create(cache_file)?;
        file.lock()?;
        file.write_all(&[CACHE_COMPATIBILITY_VERSION])?;

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
                value.cleanup(docker).await;
            }
        }

        bincode::encode_into_std_write(&cache, &mut file, bincode_config())?;

        if export_standalone {
            file.write_all(&[1])?;

            let images = cache.values().filter_map(|data| {
                if let Data::Container(image) = data {
                    Some(image)
                } else {
                    None
                }
            });
            log::info!("Exporting standalone cache");
            docker.export(images, &mut file).await?;
        } else {
            file.write_all(&[0])?;
        }

        file.flush()?;

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
#[cfg(feature = "_test_docker")]
#[expect(clippy::expect_used, reason = "tests")]
mod tests {
    use rstest::{fixture, rstest};

    use super::*;

    #[fixture]
    async fn docker_client() -> DockerClient {
        DockerClient::new(crate::tui::TuiSender(None), 1)
            .await
            .expect("Failed to create Docker client")
    }

    #[tokio::test]
    #[rstest]
    #[proptest::property_test]
    #[test_log::test]
    async fn save_and_load_one_entry(
        #[future] docker_client: DockerClient,
        #[ignore] node: NodeKindId,
        #[ignore] data: Vec<Data>,
        #[ignore] value: Data,
    ) {
        let docker_client = docker_client.await;

        let data = data.iter().collect::<Vec<_>>();
        let key = CacheKey {
            node,
            inputs: &data,
        };

        let mut cache = Cache::new();
        cache.insert(key.sha256().unwrap(), value.clone());

        let cache_file = tempfile::NamedTempFile::new().unwrap();
        let cache_file = cache_file.path();
        cache
            .save_cache(cache_file, &docker_client, false, false)
            .await
            .unwrap();

        let mut loaded_cache = Cache::load_cache(cache_file, &docker_client).await.unwrap();
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
        #[future] docker_client: DockerClient,
        #[ignore] values: Vec<(NodeKindId, Vec<Data>, Data)>,
    ) {
        let docker_client = docker_client.await;

        let mut cache = Cache::new();
        for (node, data, value) in &values {
            let data = data.iter().collect::<Vec<_>>();
            let key = CacheKey {
                node: *node,
                inputs: &data,
            };

            cache.insert(key.sha256().unwrap(), value.clone());
        }

        let cache_file = tempfile::NamedTempFile::new().unwrap();
        let cache_file = cache_file.path();
        cache
            .save_cache(cache_file, &docker_client, false, false)
            .await
            .unwrap();

        let mut loaded_cache = Cache::load_cache(cache_file, &docker_client).await.unwrap();

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
        #[future] docker_client: DockerClient,
        #[ignore] node: NodeKindId,
        #[ignore] data: Vec<Data>,
        #[ignore] value: Data,
    ) {
        let docker_client = docker_client.await;

        let data = data.iter().collect::<Vec<_>>();
        let key = CacheKey {
            node,
            inputs: &data,
        };

        let mut cache = Cache::new();
        cache.insert(key.sha256().unwrap(), value.clone());

        let cache_file = tempfile::NamedTempFile::new().unwrap();
        let cache_file = cache_file.path();
        cache
            .save_cache(cache_file, &docker_client, false, false)
            .await
            .unwrap();

        let mut loaded_cache = Cache::load_cache(cache_file, &docker_client).await.unwrap();
        loaded_cache
            .get(&key.sha256().unwrap())
            .expect("Value not found");

        // Even tho `keep_old_cache` is false it should still keep the entry in there since we used
        // it.
        loaded_cache
            .save_cache(cache_file, &docker_client, false, false)
            .await
            .unwrap();

        let mut second_loaded_cache = Cache::load_cache(cache_file, &docker_client).await.unwrap();
        second_loaded_cache
            .get(&key.sha256().unwrap())
            .expect("Value not found");
    }

    #[tokio::test]
    #[rstest]
    #[proptest::property_test]
    #[test_log::test]
    async fn old_entry_cleared_if_not_used(
        #[future] docker_client: DockerClient,
        #[ignore] node: NodeKindId,
        #[ignore] data: Vec<Data>,
        #[ignore] value: Data,
    ) {
        let docker_client = docker_client.await;

        let data = data.iter().collect::<Vec<_>>();
        let key = CacheKey {
            node,
            inputs: &data,
        };

        let mut cache = Cache::new();
        cache.insert(key.sha256().unwrap(), value.clone());

        let cache_file = tempfile::NamedTempFile::new().unwrap();
        let cache_file = cache_file.path();
        cache
            .save_cache(cache_file, &docker_client, false, false)
            .await
            .unwrap();

        let loaded_cache = Cache::load_cache(cache_file, &docker_client).await.unwrap();
        loaded_cache
            .save_cache(cache_file, &docker_client, false, false)
            .await
            .unwrap();

        let mut second_loaded_cache = Cache::load_cache(cache_file, &docker_client).await.unwrap();
        let result = second_loaded_cache.get(&key.sha256().unwrap());
        assert!(result.is_none(), "unused old_cache value was saved.");
    }

    #[tokio::test]
    #[rstest]
    #[proptest::property_test]
    #[test_log::test]
    async fn old_entry_kept_if_keep_old_true_even_if_not_used(
        #[future] docker_client: DockerClient,
        #[ignore] node: NodeKindId,
        #[ignore] data: Vec<Data>,
        #[ignore] value: Data,
    ) {
        let docker_client = docker_client.await;

        let data = data.iter().collect::<Vec<_>>();
        let key = CacheKey {
            node,
            inputs: &data,
        };

        let mut cache = Cache::new();
        cache.insert(key.sha256().unwrap(), value.clone());

        let cache_file = tempfile::NamedTempFile::new().unwrap();
        let cache_file = cache_file.path();
        cache
            .save_cache(cache_file, &docker_client, false, false)
            .await
            .unwrap();

        let loaded_cache = Cache::load_cache(cache_file, &docker_client).await.unwrap();
        loaded_cache
            .save_cache(cache_file, &docker_client, true, false)
            .await
            .unwrap();

        let mut second_loaded_cache = Cache::load_cache(cache_file, &docker_client).await.unwrap();
        second_loaded_cache
            .get(&key.sha256().unwrap())
            .expect("Value not found");
    }

    #[tokio::test]
    #[rstest]
    #[test_log::test]
    async fn export_standalone_to_existing_file(#[future] docker_client: DockerClient) {
        let docker_client = docker_client.await;

        let mut cache = Cache::new();
        let image = docker_client
            .pull_image("quay.io/toolbx-images/alpine-toolbox:latest")
            .await
            .unwrap();
        cache.insert([0; 32], Data::Container(image));

        let cache_file = tempfile::NamedTempFile::new().unwrap();
        let cache_file = cache_file.path();
        cache
            .save_cache(cache_file, &docker_client, true, true)
            .await
            .unwrap();

        let loaded_cache = Cache::load_cache(cache_file, &docker_client).await.unwrap();
        loaded_cache
            .save_cache(cache_file, &docker_client, true, true)
            .await
            .unwrap();

        Cache::load_cache(cache_file, &docker_client).await.unwrap();
    }
}
