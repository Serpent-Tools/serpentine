//! Code for working with the files, defines traits for abstracting over file providers and
//! ensuring they can be cached if needed.

use std::io;
use std::ops::Deref;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serpentine_internal::{FileSystemEntryHeader, WireFormat};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, DuplexStream, ReadBuf};
use tokio::sync::OnceCell;
use typed_path::{PlatformPath, PlatformPathBuf};

use crate::engine::RuntimeError;
use crate::engine::cache::{CacheData, CacheReader, CacheWriter};

/// Type alias for a boxed async reader.
pub type Reader<'this> = Box<dyn AsyncRead + Send + Unpin + 'this>;

/// Trait for a object that can provide file system data.
///
/// `Send + Sync` because a `FileSystem` (which holds one) lives in `Data`, which is shared across
/// worker threads through the scheduler.
pub trait FileSystemProvider: Send + Sync {
    /// Get a reader matching the format specified in `serpentine_internal` from this file system
    /// source.
    fn get_reader<'this>(
        &'this self,
    ) -> Pin<Box<dyn Future<Output = Result<Reader<'this>, RuntimeError>> + Send + 'this>>;

    /// Hash this content.
    ///
    /// The default implementation simply hashes the output of `read`
    fn hash_data<'this>(
        &'this self,
        hasher: &'this mut blake3::Hasher,
    ) -> Pin<Box<dyn Future<Output = Result<(), RuntimeError>> + Send + 'this>> {
        Box::pin(async move {
            let mut reader = self.get_reader().await?;
            let mut buffer = [0_u8; 4048];

            loop {
                match reader.read(&mut buffer).await {
                    Err(err) => return Err(err.into()),
                    Ok(0) => break,
                    Ok(bytes_read) => {
                        #[expect(
                            clippy::indexing_slicing,
                            reason = "We cannot read more data than what fits in the buffer"
                        )]
                        hasher.update(&buffer[..bytes_read]);
                    }
                }
            }

            Ok(())
        })
    }

    /// Clone yourself into a new box dyn
    fn dyn_clone(&self) -> Box<dyn FileSystemProvider>;
}

/// New type wrapper around a `dyn FileSystemProvider` with identity-based `PartialEq` and `Hash`,
/// as well as an implementation of `CacheData`
pub struct FileSystem {
    /// The inner filesystem provider
    provider: Box<dyn FileSystemProvider>,
    /// The cached hash of the data.
    hash: Arc<OnceCell<blake3::Hash>>,
}

impl PartialEq for FileSystem {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.hash, &other.hash)
    }
}
impl Eq for FileSystem {}

impl std::hash::Hash for FileSystem {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.hash).hash(state);
    }
}

impl Deref for FileSystem {
    type Target = dyn FileSystemProvider;

    fn deref(&self) -> &Self::Target {
        &*self.provider
    }
}

impl<T: FileSystemProvider + 'static> From<T> for FileSystem {
    fn from(value: T) -> Self {
        Self {
            provider: Box::new(value),
            hash: Arc::new(OnceCell::new()),
        }
    }
}

impl Clone for FileSystem {
    fn clone(&self) -> Self {
        Self {
            provider: self.provider.dyn_clone(),
            hash: Arc::clone(&self.hash),
        }
    }
}

impl std::fmt::Debug for FileSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("FileSystem").finish_non_exhaustive()
    }
}

/// An in-memory file system stream.
///
/// Should be avoided when possible, used when a filesystem is restored from cache.
#[derive(Clone)]
struct InMemoryFile(Arc<[u8]>);

impl FileSystemProvider for InMemoryFile {
    fn get_reader<'this>(
        &'this self,
    ) -> Pin<Box<dyn Future<Output = Result<Reader<'this>, RuntimeError>> + Send + 'this>> {
        let reader: Reader<'this> = Box::new(self.0.as_ref());
        Box::pin(std::future::ready(Ok(reader)))
    }
    fn hash_data<'this>(
        &'this self,
        hasher: &'this mut blake3::Hasher,
    ) -> Pin<Box<dyn Future<Output = Result<(), RuntimeError>> + Send + 'this>> {
        hasher.update(&self.0);
        Box::pin(std::future::ready(Ok(())))
    }

    fn dyn_clone(&self) -> Box<dyn FileSystemProvider> {
        Box::new(self.clone())
    }
}

impl CacheData for FileSystem {
    async fn write(
        &self,
        writer: &mut CacheWriter<impl AsyncWrite + Unpin + Send>,
    ) -> Result<(), RuntimeError> {
        log::warn!("Storing filesystem in cache, this is often overkill.");

        let mut reader = self.provider.get_reader().await?;
        tokio::io::copy(&mut reader, &mut **writer).await?;

        Ok(())
    }

    async fn read(
        reader: &mut CacheReader<impl AsyncRead + Unpin + Send>,
    ) -> Result<Self, RuntimeError> {
        let mut data = Vec::new();

        copy_filesystem_stream(&mut **reader, &mut data).await?;

        Ok(InMemoryFile(data.into()).into())
    }

    async fn content_hash(&self, hasher: &mut blake3::Hasher) -> Result<(), RuntimeError> {
        let hash = self
            .hash
            .get_or_try_init::<RuntimeError, _, _>(async || {
                let mut filesystem_hasher = blake3::Hasher::new();
                self.provider.hash_data(&mut filesystem_hasher).await?;
                Ok(filesystem_hasher.finalize())
            })
            .await?;

        hasher.update(hash.as_bytes());
        Ok(())
    }
}

/// Copy the following file system stream from reader to writer.
/// Leaving the data after the filesystem in the reader.
pub async fn copy_filesystem_stream(
    reader: &mut (impl AsyncRead + Unpin + Send),
    writer: &mut (impl AsyncWrite + Unpin + Send),
) -> Result<(), RuntimeError> {
    let mut folder_stack = vec![1_u64];
    while let Some(current_folder) = folder_stack.last_mut() {
        if *current_folder == 0 {
            folder_stack.pop();
        } else {
            *current_folder = current_folder.saturating_sub(1);

            let header = FileSystemEntryHeader::read(reader).await?;
            match header {
                FileSystemEntryHeader::File { length, .. } => {
                    header.write(writer).await?;
                    tokio::io::copy(&mut reader.take(length), writer).await?;
                }
                FileSystemEntryHeader::Folder { entries, .. } => {
                    header.write(writer).await?;
                    folder_stack.push(entries);
                }
            }
        }
    }

    Ok(())
}

/// The size of the in-process pipe buffer bridging a producer to its reader.
const PIPE_BUFFER: usize = 64 * 1024;

/// An [`AsyncRead`] that owns its producer future and drives it inline on every `poll_read`,
/// using a [`DuplexStream`] purely as the byte buffer. This keeps producer and consumer on a
/// single task (no spawn), so the producer needs no `'static` bound and may borrow from its
/// surroundings.
///
/// The future is boxed because `Reader` requires `Unpin` (its consumers go through
/// `AsyncReadExt`/`tokio::io::copy`) and an inline async future is not.
struct InlineReader<Fut> {
    /// The read half of the duplex; the producer owns the write half.
    reader: DuplexStream,
    /// The producer feeding the write half, or `None` once it has finished (which closes the
    /// write half and lets the reader reach EOF).
    producer: Option<Pin<Box<Fut>>>,
    /// A producer error, surfaced once the buffered bytes ahead of it have been drained.
    error: Option<io::Error>,
}

impl<Fut: Future<Output = io::Result<()>>> InlineReader<Fut> {
    /// Wrap `producer`, a future handed the write half of a pipe to write its content into.
    fn new(producer: impl FnOnce(DuplexStream) -> Fut) -> Self {
        let (writer, reader) = tokio::io::duplex(PIPE_BUFFER);
        Self {
            reader,
            producer: Some(Box::pin(producer(writer))),
            error: None,
        }
    }

    /// Drive the producer once so it can push more bytes into the buffer, recording its result
    /// and releasing the write half once it finishes.
    fn poll_producer(&mut self, cx: &mut Context<'_>) {
        let Some(producer) = &mut self.producer else {
            return;
        };
        let Poll::Ready(result) = producer.as_mut().poll(cx) else {
            return;
        };
        self.producer = None;
        self.error = result.err();
    }
}

impl<Fut: Future<Output = io::Result<()>>> AsyncRead for InlineReader<Fut> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.poll_producer(cx);

        let filled = buf.filled().len();
        let poll = Pin::new(&mut this.reader).poll_read(cx, buf);

        // Surface a stored producer error only once the bytes buffered ahead of it have drained.
        let reached_eof = matches!(poll, Poll::Ready(Ok(())))
            && this.producer.is_none()
            && buf.filled().len() == filled;
        if reached_eof && let Some(error) = this.error.take() {
            return Poll::Ready(Err(error));
        }

        poll
    }
}

/// A `FileSystemProvider` that reads from the given path on the current system
pub struct LocalFiles(pub PlatformPathBuf);

impl FileSystemProvider for LocalFiles {
    fn get_reader<'this>(
        &'this self,
    ) -> Pin<Box<dyn Future<Output = Result<Reader<'this>, RuntimeError>> + Send + 'this>> {
        Box::pin(async move {
            let ignore = discover_gitignore(&serpentine_internal::platform_to_std(&self.0)?);

            let reader: Reader<'_> = Box::new(InlineReader::new(move |mut writer| async move {
                serpentine_internal::read_disk_to_filesystem_stream(
                    &self.0,
                    PlatformPath::new(""),
                    &mut writer,
                    |path, is_dir| {
                        if let Some(ignore) = &ignore {
                            !ignore.matched(path, is_dir).is_ignore()
                        } else {
                            true
                        }
                    },
                )
                .await
            }));

            Ok(reader)
        })
    }

    fn dyn_clone(&self) -> Box<dyn FileSystemProvider> {
        Box::new(Self(self.0.clone()))
    }
}

/// Find all relevant ignore files and construct a ignore matcher.
fn discover_gitignore(within_dir: &Path) -> Option<Gitignore> {
    let within_dir = within_dir.canonicalize().ok()?;
    let repo_root = within_dir
        .ancestors()
        .find(|path| path.join(".git").exists())?;
    log::debug!("Found git root at {}", repo_root.display());
    let mut builder = GitignoreBuilder::new(repo_root);

    if let Some(user_dirs) = directories::UserDirs::new() {
        let _ = builder.add(user_dirs.home_dir().join(".config/git/ignore"));
    }

    let _ = builder.add(repo_root.join(".git/info/exclude"));
    for ancestor in within_dir.ancestors() {
        let _ = builder.add(ancestor.join(".gitignore"));
        if ancestor == repo_root {
            break;
        }
    }

    builder.build().ok()
}
