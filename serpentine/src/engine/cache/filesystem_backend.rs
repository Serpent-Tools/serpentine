//! `CacheBackend` that writes to the local filesystem.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll, ready};

use base64::Engine;
use futures_util::future::BoxFuture;
use nohash::IntSet;
use serpentine_internal::platform_to_std;
use tokio::io::AsyncWrite;
use typed_path::{PlatformPath, PlatformPathBuf};

use crate::engine::cache::{CacheBackend, CacheHash};
use crate::engine::{BoxedReader, BoxedWriter};

/// The extension to use for cache files.
const CACHE_EXTENSION: &str = ".serpentine";

/// A `CacheBackend` that writes to the local filesystem.
pub struct LocalCacheBackend {
    /// The caching directory to use.
    cache_dir: PlatformPathBuf,
    /// Where entries are written before they are complete.
    scratch_dir: PlatformPathBuf,
    /// Locks for blob store.
    ///
    /// If another task is already attempting to write the key we will return None for further
    /// readers
    locks: Mutex<IntSet<CacheHash>>,
}

impl LocalCacheBackend {
    /// Create a new `LocalCacheBackend` with the given cache directory.
    ///
    /// This will create the directory if it does not exist.
    pub async fn new(cache_dir: PlatformPathBuf) -> Result<Self, std::io::Error> {
        log::info!("Saving caches to {}", cache_dir.display());

        log::debug!("Creating local cache backend with directory: {cache_dir:?}");
        tokio::fs::create_dir_all(platform_to_std(&cache_dir).map_err(std::io::Error::other)?)
            .await?;

        let scratch_dir = cache_dir.join(PlatformPath::new("scratch"));
        tokio::fs::create_dir_all(platform_to_std(&scratch_dir).map_err(std::io::Error::other)?)
            .await?;

        Ok(Self {
            cache_dir,
            scratch_dir,
            locks: Mutex::new(IntSet::default()),
        })
    }

    /// Get the file path for a given key
    fn file_path_for_key(&self, key: CacheHash) -> PlatformPathBuf {
        let filename = base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(key.0);
        let filename = format!("{filename}{CACHE_EXTENSION}");

        self.cache_dir.join(filename)
    }

    /// Get a scratch path to write an entry to before it is complete.
    ///
    /// The name carries a uuid so concurrent instances writing the same entry get a scratch file
    /// each rather than colliding on one.
    fn scratch_path(&self) -> PlatformPathBuf {
        self.scratch_dir.join(uuid::Uuid::new_v4().to_string())
    }
}

impl CacheBackend for LocalCacheBackend {
    fn read_key(&self, key: CacheHash) -> BoxFuture<'static, Option<BoxedReader>> {
        log::debug!("Reading key {key:?} from local cache backend");
        let path = self.file_path_for_key(key);

        Box::pin(async move {
            let file = tokio::fs::File::open(platform_to_std(&path).ok()?)
                .await
                .ok()?;
            Some(BoxedReader::new(file))
        })
    }

    fn write_key(&self, key: CacheHash) -> BoxFuture<'static, Option<BoxedWriter>> {
        {
            let Ok(mut lock) = self.locks.lock() else {
                log::error!("Failed to get mutex on cache lock");
                return Box::pin(std::future::ready(None));
            };

            let new = lock.insert(key);
            if !new {
                return Box::pin(std::future::ready(None));
            }
        }

        log::debug!("Writing key {key:?} to local cache backend");
        let path = self.file_path_for_key(key);
        let scratch = self.scratch_path();

        Box::pin(async move {
            let destination = platform_to_std(&path).ok()?.to_path_buf();
            if tokio::fs::try_exists(&destination).await.unwrap_or(false) {
                log::debug!("Key {key:?} is already in the local cache backend");
                return None;
            }

            let scratch = platform_to_std(&scratch).ok()?.to_path_buf();
            ScratchFile::create(scratch, destination)
                .await
                .ok()
                .map(BoxedWriter::new)
        })
    }
}

/// A cache entry that only appears under its real name once it has been shut down cleanly.
///
/// Entries stream straight from containerd, so writing to the destination directly would leave a
/// truncated file under the name of a complete one whenever a run is interrupted, and every later
/// run would read that back as a corrupt entry. Renaming a finished scratch file into place means
/// an interrupted run leaves nothing for the next one to find.
///
/// The rename happens in `poll_shutdown`, so callers must shut the writer down; dropping it, as a
/// cancelled task does, leaves the entry unwritten.
struct ScratchFile {
    /// How far along finishing the entry is.
    state: State,
    /// The scratch file being written.
    scratch: PathBuf,
    /// The name the entry takes once it is complete.
    destination: PathBuf,
}

/// The stage a [`ScratchFile`] has reached.
enum State {
    /// Still taking writes.
    Writing(tokio::fs::File),
    /// Moving the finished scratch file to its destination.
    Renaming(BoxFuture<'static, std::io::Result<()>>),
    /// Renamed.
    Done,
}

impl ScratchFile {
    /// Open `scratch`, to be renamed to `destination` once it is shut down.
    async fn create(scratch: PathBuf, destination: PathBuf) -> std::io::Result<Self> {
        let file = tokio::fs::File::create_new(&scratch).await?;

        Ok(Self {
            state: State::Writing(file),
            scratch,
            destination,
        })
    }
}

impl AsyncWrite for ScratchFile {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut self.state {
            State::Writing(file) => Pin::new(file).poll_write(cx, buf),
            State::Renaming(_) | State::Done => Poll::Ready(Err(std::io::Error::other(
                "cache entry is already finished",
            ))),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut self.state {
            State::Writing(file) => Pin::new(file).poll_flush(cx),
            State::Renaming(_) | State::Done => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // Each stage builds the next one's future, which has to be polled before we can yield, so
        // the state machine is driven here rather than across calls.
        loop {
            self.state = match &mut self.state {
                State::Writing(file) => {
                    ready!(Pin::new(file).poll_shutdown(cx))?;
                    State::Renaming(Box::pin(tokio::fs::rename(
                        self.scratch.clone(),
                        self.destination.clone(),
                    )))
                }
                State::Renaming(rename) => {
                    ready!(rename.as_mut().poll(cx))?;
                    State::Done
                }
                State::Done => return Poll::Ready(Ok(())),
            };
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    crate::test_well_behaved_cache!({
        let folder = tempfile::tempdir()
            .expect("Failed to create temp dir")
            .keep();
        let folder = PlatformPathBuf::from(folder.as_os_str().as_encoded_bytes());

        LocalCacheBackend::new(folder)
            .await
            .expect("Failed to create cache backend")
    });
}
