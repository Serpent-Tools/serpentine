//! Internal crate for serpentine, Nothing in this crate follows semantic versioning.

use std::io;
use std::marker::PhantomData;
use std::path::Path;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use typed_path::PlatformPath;

pub mod network;
pub mod sidecar;

/// Write a postcard serialized frame to the given writer, prefixed with the length of the frame in bytes.
///
/// # Errors
/// If the `writer` returns a error or the serialization fails.
pub async fn write_postcard_frame<T: serde::Serialize>(
    value: &T,
    writer: &mut (impl AsyncWrite + Unpin + Send),
) -> io::Result<()> {
    let buffer = postcard::to_stdvec(value).map_err(io::Error::other)?;
    let length = buffer.len() as u64;

    writer.write_u64_le(length).await?;
    writer.write_all(&buffer).await?;

    Ok(())
}

/// Read a postcard serialized frame from the given reader, that has been prefixed with the length of the frame in bytes.
///
/// # Errors
/// If the `reader` returns a error or the deserialization fails.
pub async fn read_postcard_frame<T: serde::de::DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin + Send),
) -> io::Result<T> {
    let length = reader.read_u64_le().await?;

    let mut buffer = vec![0u8; length.try_into().unwrap_or(usize::MAX)];
    reader.read_exact(&mut buffer).await?;

    let value = postcard::from_bytes(&buffer).map_err(io::Error::other)?;
    Ok(value)
}

/// Convert a platform typed path into a [`std::path::PathBuf`] for real filesystem access.
///
/// This is the single airlock where a typed path degrades into the OS's untyped path ABI. The result
/// is meant to be ephemeral (handed straight to a syscall), never stored or flowed through logic.
///
/// On unix the path is raw bytes, identical to those of an [`std::ffi::OsStr`], so this is lossless
/// for every path. On other platforms there is no safe way to build an `OsStr` from arbitrary bytes,
/// so only valid UTF-8 (which maps losslessly onto an `OsStr`) is accepted.
///
/// # Errors
/// On non-unix platforms, if the path bytes are not valid UTF-8 we cannot represent them losslessly,
/// so this errors rather than silently transcoding.
pub fn platform_to_std(path: &PlatformPath) -> Result<&Path, std::str::Utf8Error> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        Ok(Path::new(std::ffi::OsStr::from_bytes(path.as_bytes())))
    }

    #[cfg(not(unix))]
    {
        let path = std::str::from_utf8(path.as_bytes()).map_err(Error::other)?;
        Ok(Path::new(path))
    }
}

/// Convert a `typed_path::PathBuf` into a `Vec<u8>` as serde wants a single function and the normal
/// getter returns a slice.
fn path_to_vec<T: typed_path::Encoding>(path: &typed_path::PathBuf<T>) -> Vec<u8> {
    path.as_bytes().to_vec()
}

/// Remote type for `typed_path` has it doesnt implement `serde::Serialize` or `serde::Deserialize` itself.
///
/// Used as `#[serde(with = "crate::TypedPathBufRemote")]`
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(remote = "typed_path::PathBuf")]
pub struct TypedPathBufRemote<T: typed_path::Encoding> {
    /// The encoding used.
    #[serde(skip)]
    _encoding: PhantomData<T>,
    /// The actual bytes
    #[serde(getter = "path_to_vec")]
    inner: Vec<u8>,
}

impl<T: typed_path::Encoding> From<TypedPathBufRemote<T>> for typed_path::PathBuf<T> {
    fn from(remote: TypedPathBufRemote<T>) -> Self {
        typed_path::PathBuf::from(remote.inner)
    }
}

/// A newtype around a `TypedPath` to allow serlizing/deserializing it directly when its not part of another type.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct TypedPathSerdeWrapper<T: typed_path::Encoding>(
    #[serde(with = "TypedPathBufRemote::<T>")] pub typed_path::PathBuf<T>,
);

/// Header for each entry in a file system stream.
///
/// Files and Folders specify the relative path relative to the previous folder.
/// i.e
///
/// * Folder(name="foo", length=2) -> /foo
/// * Folder(name="bar", length=1) -> /foo/bar
/// * File(name="a") -> /foo/bar/a
/// * <a data ...>
/// * File(name="b") -> /foo/b
/// * <b data ...>
/// * File(name="c") -> /foo/c
/// * <c data ...>
#[derive(serde::Serialize, serde::Deserialize)]
pub enum FileSystemEntryHeader {
    /// A file
    File {
        /// The name of the file (including extension)
        name: Box<[u8]>,
        /// The amount of following bytes to read as this files contents
        length: u64,
    },
    /// A folder is a container of files
    Folder {
        /// The name of the folder
        name: Box<[u8]>,
        /// The number of other entries in this folder (direct children only)
        entries: u64,
    },
}

/// Read the given path into the given reader according to the filesystem format.
///
/// The absolute path specifies the file location of the structure being written.
/// the relative path specifies the specific sub item being written right now (in most cases this
/// should be `.`)
///
/// The given filter is given each path and bool indicating whether it is a directory, if the
/// returned value is false the item is not emitted.
///
/// # Errors
/// If the `writer` returns a error or reading from the filesystem runs into a error.
pub async fn read_disk_to_filesystem_stream(
    absolute_path: &PlatformPath,
    relative_path: &PlatformPath,
    writer: &mut (impl AsyncWrite + Unpin + Send),
    filter: impl Fn(&Path, bool) -> bool + Copy,
) -> io::Result<()> {
    let name = relative_path.file_name().unwrap_or_default().into();

    let absolute_path_to_item = if relative_path.as_bytes().is_empty() {
        absolute_path.to_path_buf()
    } else {
        absolute_path.join(relative_path)
    };

    let std_path = platform_to_std(&absolute_path_to_item).map_err(io::Error::other)?;
    log::trace!("Exporting {}", absolute_path_to_item.display());
    let metadata = tokio::fs::metadata(&std_path).await?;

    if metadata.is_file() {
        let header = FileSystemEntryHeader::File {
            name,
            length: metadata.len(),
        };
        write_postcard_frame(&header, writer).await?;

        let mut file = tokio::fs::File::open(&std_path).await?;
        tokio::io::copy(&mut file, writer).await?;
    } else if metadata.is_dir() {
        let entries = {
            let mut entries = Vec::new();
            let mut entry_stream = tokio::fs::read_dir(&std_path).await?;

            while let Some(entry) = entry_stream.next_entry().await? {
                if filter(&entry.path(), entry.metadata().await?.is_dir()) {
                    entries.push(entry);
                } else {
                    log::trace!("File {} ignored", absolute_path_to_item.display());
                }
            }

            entries
        };
        let header = FileSystemEntryHeader::Folder {
            name,
            entries: entries.len() as u64,
        };
        write_postcard_frame(&header, writer).await?;

        for entry in entries {
            let relative_path =
                relative_path.join(PlatformPath::new(entry.file_name().as_encoded_bytes()));
            Box::pin(read_disk_to_filesystem_stream(
                absolute_path,
                &relative_path,
                writer,
                filter,
            ))
            .await?;
        }
    }
    Ok(())
}

/// Read the given file system stream onto the disk
///
/// if `permissive_permissions` is set than all permission bits will be set on unix (read, write,
/// executable), if not set then the platform defaults will be used.
///
/// # Errors
/// If the `reader` returns a error or writing to the filesystem runs into a error.
pub async fn read_filesystem_stream_to_disk(
    target_path: &PlatformPath,
    reader: &mut (impl AsyncRead + Unpin + Send),
    permissive_permissions: bool,
) -> io::Result<()> {
    let header = read_postcard_frame(reader).await?;

    match header {
        FileSystemEntryHeader::File { name, length } => {
            let target_path = if name.is_empty() {
                target_path.to_path_buf()
            } else {
                target_path.join(PlatformPath::new(&*name))
            };
            let std_path = platform_to_std(&target_path).map_err(io::Error::other)?;
            log::trace!("Writing file at {}", target_path.display());

            if let Some(parent) = target_path.parent() {
                tokio::fs::create_dir_all(&platform_to_std(parent).map_err(io::Error::other)?)
                    .await?;
            }

            let mut open_options = tokio::fs::File::options();
            open_options.create(true).truncate(true).write(true);

            #[cfg(unix)]
            if permissive_permissions {
                open_options.mode(0o777);
            }

            let mut file = open_options.open(&std_path).await?;

            tokio::io::copy(&mut reader.take(length), &mut file).await?;
        }
        FileSystemEntryHeader::Folder { name, entries } => {
            let target_path = target_path.join(PlatformPath::new(&*name));
            log::trace!("Writing directory at {}", target_path.display());
            tokio::fs::create_dir_all(&platform_to_std(&target_path).map_err(io::Error::other)?)
                .await?;

            for _ in 0..entries {
                Box::pin(read_filesystem_stream_to_disk(
                    &target_path,
                    reader,
                    permissive_permissions,
                ))
                .await?;
            }
        }
    }

    Ok(())
}
