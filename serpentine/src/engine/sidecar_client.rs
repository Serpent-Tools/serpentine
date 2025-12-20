//! Client to serpentines sidecar, handles setting up the connections etc.

use std::net::SocketAddr;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net;

use crate::engine::RuntimeError;

/// A sidecar client, holds the location to connect to for each connection.
#[derive(Clone, Copy)]
pub struct Client(SocketAddr);

impl Client {
    /// Create a new client for the specified address.
    pub fn new(addr: SocketAddr) -> Self {
        Self(addr)
    }

    /// Connect to serpentine and send the needed magic bytes
    async fn connect(&self) -> Result<net::TcpStream, RuntimeError> {
        let mut socket = net::TcpStream::connect(self.0).await?;
        socket.write_all("danger noodle".as_bytes()).await?;
        Ok(socket)
    }

    /// Connect to the sidecar and setup a containerd proxy.
    pub async fn containerd(&self) -> Result<net::TcpStream, RuntimeError> {
        let mut socket = self.connect().await?;
        socket.write_u8(0).await?;
        Ok(socket)
    }

    /// Connected to the sidecar and request it create a fifo pipe, returns its (in container) path and a reader of the contents.
    pub async fn fifo_pipe(
        &self,
    ) -> Result<(String, impl AsyncRead + Unpin + Send + 'static), RuntimeError> {
        let mut socket = self.connect().await?;
        socket.write_u8(1).await?;

        let length = socket.read_u8().await?;
        let mut path = vec![0; length.into()];
        socket.read_exact(&mut path).await?;

        let path = String::from_utf8(path)
            .map_err(|_| RuntimeError::internal("Sidecar responded with a non-utf8 path"))?;

        Ok((path, socket))
    }
}
