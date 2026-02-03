//! Internal crate for serpentine, Nothing in this crate follows semantic versioning.

use std::io::{Error, Result};
use std::path::Path;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Write a `u64` using variable-length encoding
///
/// # Errors
/// If writing the value causes IO error
pub async fn write_u64_variable_length(
    writer: &mut (impl AsyncWrite + Unpin),
    mut value: u64,
) -> Result<()> {
    loop {
        let mut byte = (value & 0b0111_1111) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0b1000_0000;
        }
        writer.write_u8(byte).await?;

        if value == 0 {
            break;
        }
    }

    Ok(())
}

/// Read a variable-length encoded u64
///
/// # Errors
/// If reading the value causes IO error
pub async fn read_u64_length_encoded(reader: &mut (impl AsyncRead + Unpin)) -> Result<u64> {
    let mut value: u64 = 0;
    let mut shift_amount: u8 = 0;

    loop {
        let byte = reader.read_u8().await?;
        value |= (u64::from(byte) & 0b0111_1111) << shift_amount;
        shift_amount = shift_amount.saturating_add(7);

        if byte & 0b1000_0000 == 0 {
            break;
        }
    }

    Ok(value)
}

/// Write a length-prefixed `Vec<u8>`
///
/// # Errors
/// If writing the value causes IO error
pub async fn write_length_prefixed(
    writer: &mut (impl AsyncWrite + Unpin),
    values: impl AsRef<[u8]>,
) -> Result<()> {
    let values = values.as_ref();
    write_u64_variable_length(writer, values.len() as u64).await?;
    writer.write_all(values).await?;

    Ok(())
}

/// Read a length-prefixed `Vec<u8>`.
///
/// # Errors
/// If writing the value causes IO error.
pub async fn read_length_prefixed(reader: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>> {
    let length = read_u64_length_encoded(reader)
        .await?
        .try_into()
        .map_err(Error::other)?;
    let mut result = vec![0; length];
    reader.read_exact(&mut result).await?;

    Ok(result)
}

/// Read a length-prefixed `String`.
///
/// # Errors
/// If writing the value causes IO error.
/// Or if the data read isnt utf8.
pub async fn read_length_prefixed_string(reader: &mut (impl AsyncRead + Unpin)) -> Result<String> {
    let bytes = read_length_prefixed(reader).await?;
    String::from_utf8(bytes).map_err(Error::other)
}

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
pub enum FileSystemEntryHeader {
    /// A file
    File {
        /// The name of the file (including extension)
        name: Box<str>,
        /// The amount of following bytes to read as this files contents
        length: u64,
    },
    /// A folder is a container of files
    Folder {
        /// The name of the folder
        name: Box<str>,
        /// The number of other entries in this folder (direct children only)
        entries: u64,
    },
}

impl FileSystemEntryHeader {
    /// Write this header to the writer.
    ///
    /// # Errors
    /// If an io error occurs.
    pub async fn write(self, writer: &mut (impl AsyncWrite + Unpin + Send)) -> Result<()> {
        match self {
            Self::File { name, length } => {
                writer.write_u8(0).await?;
                write_length_prefixed(writer, name.as_bytes()).await?;
                write_u64_variable_length(writer, length).await?;
            }
            Self::Folder { name, entries } => {
                writer.write_u8(1).await?;
                write_length_prefixed(writer, name.as_bytes()).await?;
                write_u64_variable_length(writer, entries).await?;
            }
        }

        Ok(())
    }

    /// Read a header from the given reader.
    ///
    /// # Errors
    /// If the reader returns a error.
    pub async fn read(reader: &mut (impl AsyncRead + Unpin + Send)) -> Result<Self> {
        let kind = reader.read_u8().await?;
        let name = read_length_prefixed_string(reader).await?.into();
        let length = read_u64_length_encoded(reader).await?;
        match kind {
            0 => Ok(Self::File { name, length }),
            1 => Ok(Self::Folder {
                name,
                entries: length,
            }),
            _ => Err(Error::other("Unknown file header kind")),
        }
    }
}

/// Read the given path into the given reader according to the filesystem format.
///
/// The absolute path species the file location of the structure being written.
/// the relative path specifies the specific sub item being written right now (in most cases this
/// should be `.`)
///
/// The given filter is given each path and bool indicating whether it is a directory, if the
/// returned value is false the item is not emitted.
///
/// # Errors
/// If the `writer` returns a error or reading from the filesystem runs into a error.
pub async fn read_disk_to_filesystem_stream(
    absolute_path: &Path,
    relative_path: &Path,
    writer: &mut (impl AsyncWrite + Unpin + Send),
    filter: impl Fn(&Path, bool) -> bool + Copy,
) -> Result<()> {
    let name = relative_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into();

    let absolute_path_to_item = if relative_path.to_string_lossy().is_empty() {
        absolute_path.into()
    } else {
        absolute_path.join(relative_path)
    };

    log::debug!("Exporting {}", absolute_path_to_item.display());
    let metadata = tokio::fs::metadata(&absolute_path_to_item).await?;

    if metadata.is_file() {
        let header = FileSystemEntryHeader::File {
            name,
            length: metadata.len(),
        };
        header.write(writer).await?;

        let mut file = tokio::fs::File::open(absolute_path_to_item).await?;
        tokio::io::copy(&mut file, writer).await?;
    } else if metadata.is_dir() {
        let entries = {
            let mut entries = Vec::new();
            let mut entry_stream = tokio::fs::read_dir(&absolute_path_to_item).await?;

            while let Some(entry) = entry_stream.next_entry().await? {
                if filter(&entry.path(), entry.metadata().await?.is_dir()) {
                    entries.push(entry);
                } else {
                    log::debug!("File {} ignored", absolute_path_to_item.display());
                }
            }

            entries
        };
        let header = FileSystemEntryHeader::Folder {
            name,
            entries: entries.len() as u64,
        };
        header.write(writer).await?;

        for entry in entries {
            let relative_path = relative_path.join(entry.file_name());
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
    target_path: &Path,
    reader: &mut (impl AsyncRead + Unpin + Send),
    permissive_permissions: bool,
) -> Result<()> {
    let header = FileSystemEntryHeader::read(reader).await?;
    match header {
        FileSystemEntryHeader::File { name, length } => {
            let target_path = if name.is_empty() {
                target_path.into()
            } else {
                target_path.join(&*name)
            };
            log::debug!("Writing file at {}", target_path.display());

            if let Some(parent) = target_path.parent() {
                tokio::fs::create_dir_all(&parent).await?;
            }

            let mut open_options = tokio::fs::File::options();
            open_options.create(true).truncate(true).write(true);

            #[cfg(unix)]
            if permissive_permissions {
                open_options.mode(0o777);
            }

            let mut file = open_options.open(&target_path).await?;

            tokio::io::copy(&mut reader.take(length), &mut file).await?;
        }
        FileSystemEntryHeader::Folder { name, entries } => {
            let target_path = target_path.join(&*name);
            log::debug!("Writing directory at {}", target_path.display());
            tokio::fs::create_dir_all(&target_path).await?;

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

/// Code implementing parts of the sidecar protocol, see `sidecar/src/main.rs` and
/// `serpentine/src/engine/sidecar_clinet.rs` for the server and client sides, this module exists to
/// share certain values.
pub mod sidecar {
    use std::io::{Error, Result};

    use tokio::io::{AsyncRead, AsyncWrite};

    use super::{
        read_length_prefixed_string,
        read_u64_length_encoded,
        write_length_prefixed,
        write_u64_variable_length,
    };

    /// Magic number to protect sidecar from garbage data as well as XSRF attacks.
    pub const MAGIC_NUMBER: &str = "danger noodle";

    /// The port the sidecar listens on
    pub const PORT: u16 = 8000;

    /// The kind of events the sidecar supports.
    #[cfg_attr(test, derive(proptest_derive::Arbitrary))]
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    #[repr(u8)]
    pub enum RequestKind {
        /// Proxy the containerd socket
        Proxy,
        /// Create a fifo pipe
        CreateFifo,
        /// Create a network namespace
        CreateNetwork,
        /// Delete a network interface
        DeleteNetwork,
        /// Export files from a mount.
        ExportFiles,
        /// Import files to a mount.
        ImportFiles,
    }

    impl TryFrom<u8> for RequestKind {
        type Error = ();

        fn try_from(value: u8) -> std::result::Result<Self, Self::Error> {
            match value {
                0 => Ok(Self::Proxy),
                1 => Ok(Self::CreateFifo),
                2 => Ok(Self::CreateNetwork),
                3 => Ok(Self::DeleteNetwork),
                4 => Ok(Self::ExportFiles),
                5 => Ok(Self::ImportFiles),
                _ => Err(()),
            }
        }
    }

    /// Mounts options for mounting a snapshot in the sidecar manually.
    pub struct Mount {
        /// The kind of mount
        pub type_: String,
        /// The source to mount from
        pub source: String,
        /// The target to mount to
        pub target: String,
        /// The options for the mount
        pub options: Vec<String>,
    }

    impl Mount {
        /// Write this mount to the given writer.
        ///
        /// # Errors
        /// If the underlying writer errors
        pub async fn write(self, writer: &mut (impl AsyncWrite + Unpin + Send)) -> Result<()> {
            write_length_prefixed(writer, self.type_.as_bytes()).await?;
            write_length_prefixed(writer, self.source.as_bytes()).await?;
            write_length_prefixed(writer, self.target.as_bytes()).await?;

            write_u64_variable_length(writer, self.options.len() as u64).await?;
            for option in self.options {
                write_length_prefixed(writer, option.as_bytes()).await?;
            }

            Ok(())
        }

        /// Read a mount from a reader
        ///
        /// # Errors
        /// If underlying reader errors or data is corrupted
        pub async fn read(reader: &mut (impl AsyncRead + Unpin + Send)) -> Result<Self> {
            let type_ = read_length_prefixed_string(reader).await?;
            let source = read_length_prefixed_string(reader).await?;
            let target = read_length_prefixed_string(reader).await?;

            let length = read_u64_length_encoded(reader)
                .await?
                .try_into()
                .map_err(Error::other)?;
            let mut options = Vec::with_capacity(length);
            for _ in 0..length {
                let option = read_length_prefixed_string(reader).await?;
                options.push(option);
            }

            Ok(Self {
                type_,
                source,
                target,
                options,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[rstest::rstest]
    #[proptest::property_test]
    async fn variable_length_encoding_roundtrips(#[ignore] value: u64) {
        let mut buf = std::io::Cursor::new(Vec::new());
        write_u64_variable_length(&mut buf, value).await.unwrap();

        buf.set_position(0);
        let decoded = read_u64_length_encoded(&mut buf).await.unwrap();

        assert_eq!(decoded, value, "failed for {value}");
    }

    #[tokio::test]
    #[rstest::rstest]
    #[proptest::property_test]
    async fn length_prefixed_roundtrips(#[ignore] value: Vec<u8>) {
        let mut buf = std::io::Cursor::new(Vec::new());
        write_length_prefixed(&mut buf, &value).await.unwrap();

        buf.set_position(0);
        let decoded = read_length_prefixed(&mut buf).await.unwrap();

        assert_eq!(decoded, value, "failed for {value:?}");
    }

    #[tokio::test]
    #[rstest::rstest]
    #[proptest::property_test]
    async fn length_prefixed_str_roundtrips(#[ignore] value: String) {
        let mut buf = std::io::Cursor::new(Vec::new());
        write_length_prefixed(&mut buf, value.as_bytes())
            .await
            .unwrap();

        buf.set_position(0);
        let decoded = read_length_prefixed_string(&mut buf).await.unwrap();

        assert_eq!(decoded, value, "failed for {value:?}");
    }

    #[proptest::property_test]
    fn request_kind_round_trip(kind: sidecar::RequestKind) {
        let parsed_kind = sidecar::RequestKind::try_from(kind as u8);

        assert_eq!(parsed_kind, Ok(kind));
    }

    #[test]
    fn request_kind_invalid_value() {
        let parsed_kind = sidecar::RequestKind::try_from(255);

        assert_eq!(parsed_kind, Err(()));
    }
}
