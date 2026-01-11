//! Internal crate for serpentine, Nothing in this crate follows semantic versioning.

use std::io::{Error, Result};

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
/// If writting the value causes IO error.
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
/// If writting the value causes IO error.
/// Or if the data read isnt utf8.
pub async fn read_length_prefixed_string(reader: &mut (impl AsyncRead + Unpin)) -> Result<String> {
    let bytes = read_length_prefixed(reader).await?;
    String::from_utf8(bytes).map_err(Error::other)
}

/// Code implementing parts of the sidecar protocol, see `sidecar/src/main.rs` and
/// `serpentine/src/engine/sidecar_clinet.rs` for the server and client sides, this module exists to
/// share certain values as well as keep writing and reading code close together.
pub mod sidecar {
    /// Magic number to protect sidecar from garbage data as well as XSRF attacks.
    pub const MAGIC_NUMBER: &str = "danger noodle";

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
    }

    impl TryFrom<u8> for RequestKind {
        type Error = ();

        fn try_from(value: u8) -> Result<Self, Self::Error> {
            match value {
                0 => Ok(Self::Proxy),
                1 => Ok(Self::CreateFifo),
                2 => Ok(Self::CreateNetwork),
                3 => Ok(Self::DeleteNetwork),
                _ => Err(()),
            }
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
