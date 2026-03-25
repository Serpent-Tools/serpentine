//! Code implementing parts of the sidecar protocol, see `sidecar/src/main.rs` and
//! `serpentine/src/engine/sidecar_client.rs` for the server and client sides, this module exists to
//! share certain values.

use std::io::{Error, Result};

use tokio::io::{AsyncRead, AsyncWrite};

use super::{
    WireFormat,
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
    /// Create a network topology
    CreateNetwork,
    /// Delete a network topology
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
    pub type_: Box<str>,
    /// The source to mount from
    pub source: Box<str>,
    /// The target to mount to
    pub target: Box<str>,
    /// The options for the mount
    pub options: Box<[Box<str>]>,
}

impl WireFormat for Mount {
    async fn write(self, writer: &mut (impl AsyncWrite + Unpin + Send)) -> Result<()> {
        write_length_prefixed(writer, self.type_.as_bytes()).await?;
        write_length_prefixed(writer, self.source.as_bytes()).await?;
        write_length_prefixed(writer, self.target.as_bytes()).await?;

        write_u64_variable_length(writer, self.options.len() as u64).await?;
        for option in self.options {
            write_length_prefixed(writer, option.as_bytes()).await?;
        }

        Ok(())
    }

    async fn read(reader: &mut (impl AsyncRead + Unpin + Send)) -> Result<Self> {
        let type_ = read_length_prefixed_string(reader).await?.into_boxed_str();
        let source = read_length_prefixed_string(reader).await?.into_boxed_str();
        let target = read_length_prefixed_string(reader).await?.into_boxed_str();

        let length = read_u64_length_encoded(reader)
            .await?
            .try_into()
            .map_err(Error::other)?;
        let mut options = Vec::with_capacity(length);
        for _ in 0..length {
            let option: Box<str> = read_length_prefixed_string(reader).await?.into();
            options.push(option);
        }

        Ok(Self {
            type_,
            source,
            target,
            options: options.into_boxed_slice(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[proptest::property_test]
    fn request_kind_round_trip(kind: RequestKind) {
        let parsed_kind = RequestKind::try_from(kind as u8);

        assert_eq!(parsed_kind, Ok(kind));
    }

    #[test]
    fn request_kind_invalid_value() {
        let parsed_kind = RequestKind::try_from(255);

        assert_eq!(parsed_kind, Err(()));
    }
}
