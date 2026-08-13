//! `CacheBackend` that writes to the local filesystem.

use std::pin::Pin;

use base64::Engine;
use serpentine_internal::platform_to_std;
use tokio::io::{AsyncRead, AsyncWrite};
use typed_path::PlatformPathBuf;

use crate::engine::cache::{CacheBackend, CacheHash};

/// The extension to use for cache files.
const CACHE_EXTENSION: &str = ".serpentine";

/// The file to store the data blob in.
const DATA_BLOB_NAME: &str = "data";

/// A `CacheBackend` that writes to the local filesystem.
pub struct LocalCacheBackend {
    /// The caching directory to use.
    cache_dir: PlatformPathBuf,
}

impl LocalCacheBackend {
    /// Create a new `LocalCacheBackend` with the given cache directory.
    ///
    /// This will create the directory if it does not exist.
    pub async fn new(cache_dir: PlatformPathBuf) -> Result<Self, std::io::Error> {
        tokio::fs::create_dir_all(platform_to_std(&cache_dir).map_err(std::io::Error::other)?)
            .await?;

        Ok(Self { cache_dir })
    }

    /// Get the file path for the data blob
    fn file_path_for_data(&self) -> PlatformPathBuf {
        self.cache_dir
            .join(format!("{DATA_BLOB_NAME}{CACHE_EXTENSION}"))
    }

    /// Get the file path for a given key
    fn file_path_for_key(&self, key: CacheHash) -> PlatformPathBuf {
        let filename = base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(key.0);
        let filename = format!("{filename}{CACHE_EXTENSION}");

        self.cache_dir.join(filename)
    }
}

impl CacheBackend for LocalCacheBackend {
    fn read_key(
        &self,
        key: CacheHash,
    ) -> Pin<Box<dyn Future<Output = Option<Box<dyn AsyncRead + Unpin + Send>>>>> {
        let path = self.file_path_for_key(key);

        Box::pin(async move {
            let file = tokio::fs::File::open(platform_to_std(&path).ok()?)
                .await
                .ok()?;
            Some(Box::new(file) as Box<dyn AsyncRead + Unpin + Send>)
        })
    }

    fn write_key(
        &self,
        key: CacheHash,
    ) -> Pin<Box<dyn Future<Output = Option<Box<dyn AsyncWrite + Unpin + Send>>>>> {
        let path = self.file_path_for_key(key);

        Box::pin(async move {
            let file = tokio::fs::File::create_new(platform_to_std(&path).ok()?)
                .await
                .ok()?;

            Some(Box::new(file) as Box<dyn AsyncWrite + Unpin + Send>)
        })
    }

    fn get_data_cache(
        &self,
    ) -> Pin<Box<dyn Future<Output = Option<Box<dyn AsyncRead + Unpin + Send>>>>> {
        let path = self.file_path_for_data();

        Box::pin(async move {
            let file = tokio::fs::File::open(platform_to_std(&path).ok()?)
                .await
                .ok()?;
            Some(Box::new(file) as Box<dyn AsyncRead + Unpin + Send>)
        })
    }

    fn get_data_cache_writer(
        &self,
    ) -> Pin<
        Box<
            dyn Future<
                Output = Result<Box<dyn AsyncWrite + Unpin + Send>, crate::engine::RuntimeError>,
            >,
        >,
    > {
        let path = self.file_path_for_data();

        Box::pin(async move {
            let file =
                tokio::fs::File::create(platform_to_std(&path).map_err(|_| {
                    crate::engine::RuntimeError::internal("Failed to convert path")
                })?)
                .await?;

            Ok(Box::new(file) as Box<dyn AsyncWrite + Unpin + Send>)
        })
    }
}
