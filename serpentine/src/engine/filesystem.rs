//! Code for working with the files, defines traits for abstracting over file providers and
//! ensuring they can be cached if needed.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serpentine_internal::FileSystemEntryHeader;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

use crate::engine::RuntimeError;
use crate::engine::cache::{CacheData, CacheReader, CacheWriter};

/// Type alias for a boxed async reader.
pub type Reader<'this> = Box<dyn AsyncRead + Send + Unpin + 'this>;

/// Trait for a object that can provide file system data.
pub trait FileSystemProvider {
    /// Get a reader matching the format specified in `serpentine_internal` from this file system
    /// source.
    fn get_reader<'this>(
        &'this self,
    ) -> Pin<Box<dyn Future<Output = Result<Reader<'this>, RuntimeError>> + 'this>>;

    /// Hash this content.
    ///
    /// The default implementation simply hashes the output of `read`
    fn hash_data<'this>(
        &'this self,
        hasher: &'this mut blake3::Hasher,
    ) -> Pin<Box<dyn Future<Output = Result<(), RuntimeError>> + 'this>> {
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
                            reason = "We cannot not read more data than what fits in the buffer"
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

/// New type wrapper around a `dyn FileSystemProvider` which implements stubs for `PartialEq` and
/// `Hash`, as well as a implementation of `CacheData`
pub struct FileSystem(Box<dyn FileSystemProvider>);

impl Deref for FileSystem {
    type Target = dyn FileSystemProvider;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl<T: FileSystemProvider + 'static> From<T> for FileSystem {
    fn from(value: T) -> Self {
        Self(Box::new(value))
    }
}

impl Clone for FileSystem {
    fn clone(&self) -> Self {
        Self(self.0.dyn_clone())
    }
}

impl std::fmt::Debug for FileSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("FileSystem").finish_non_exhaustive()
    }
}

impl PartialEq for FileSystem {
    fn eq(&self, _other: &Self) -> bool {
        log::warn!("Comparing filesystem instances does not fully comply with `Eq` spec");

        false
    }
}
impl Eq for FileSystem {}

impl std::hash::Hash for FileSystem {
    fn hash<H: std::hash::Hasher>(&self, _state: &mut H) {
        log::warn!("Can not hash FileSystem");
    }
}

/// A in memory file system stream.
///
/// Should be avoided when possible, used when a filesystem is restored from cache.
#[derive(Clone)]
struct InMemoryFile(Rc<[u8]>);

impl FileSystemProvider for InMemoryFile {
    fn get_reader<'this>(
        &'this self,
    ) -> Pin<Box<dyn Future<Output = Result<Reader<'this>, RuntimeError>> + 'this>> {
        let reader: Reader<'this> = Box::new(self.0.as_ref());
        Box::pin(std::future::ready(Ok(reader)))
    }
    fn hash_data<'this>(
        &'this self,
        hasher: &'this mut blake3::Hasher,
    ) -> Pin<Box<dyn Future<Output = Result<(), RuntimeError>> + 'this>> {
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

        let mut reader = self.0.get_reader().await?;
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
        self.0.hash_data(hasher).await
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

/// A `FileSystemProvider` that reads from the given path on the current system
pub struct LocalFiles(pub PathBuf);

impl FileSystemProvider for LocalFiles {
    fn get_reader<'this>(
        &'this self,
    ) -> Pin<Box<dyn Future<Output = Result<Reader<'this>, RuntimeError>> + 'this>> {
        Box::pin(async move {
            let (mut writer, reader) = tokio::io::duplex(4048);
            let path = self.0.clone();
            let ignore = discover_gitignore(&self.0);

            tokio::spawn(async move {
                let res = serpentine_internal::read_disk_to_filesystem_stream(
                    &path,
                    Path::new(""),
                    &mut writer,
                    |path, is_dir| {
                        if let Some(ignore) = &ignore {
                            !ignore.matched(path, is_dir).is_ignore()
                        } else {
                            true
                        }
                    },
                )
                .await;

                if let Err(res) = res {
                    log::error!("{res}");
                }
            });

            Ok(Reader::from(Box::new(reader)))
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
