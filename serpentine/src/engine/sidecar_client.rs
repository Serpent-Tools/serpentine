//! Client to serpentines sidecar, handles setting up the connections etc.

use std::net::SocketAddr;

use serpentine_internal::sidecar::{MAGIC_NUMBER, RequestKind};
use tokio::io::{AsyncRead, AsyncWriteExt};
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
    async fn connect(&self, kind: RequestKind) -> Result<net::TcpStream, RuntimeError> {
        let mut socket = net::TcpStream::connect(self.0).await?;
        socket.write_all(MAGIC_NUMBER.as_bytes()).await?;
        socket.write_u8(kind as u8).await?;
        Ok(socket)
    }

    /// Connect to the sidecar and setup a containerd proxy.
    pub async fn containerd(&self) -> Result<net::TcpStream, RuntimeError> {
        self.connect(RequestKind::Proxy).await
    }

    /// Connected to the sidecar and request it create a fifo pipe, returns its (in container) path and a reader of the contents.
    pub async fn fifo_pipe(
        &self,
    ) -> Result<(Box<str>, impl AsyncRead + Unpin + Send + 'static), RuntimeError> {
        log::debug!("Creating fifo pipe");
        let mut socket = self.connect(RequestKind::CreateFifo).await?;

        let path = serpentine_internal::read_length_prefixed_string(&mut socket).await?;

        Ok((path.into(), socket))
    }

    /// Create a network namespace and return its (container) path
    pub async fn create_network_namespace(&self) -> Result<Box<str>, RuntimeError> {
        log::debug!("Creating network namespace");
        let mut socket = self.connect(RequestKind::CreateNetwork).await?;

        let namespace = serpentine_internal::read_length_prefixed_string(&mut socket).await?;

        Ok(namespace.into())
    }

    /// Delete a network namespace
    pub async fn delete_network_namespace(&self, namespace: &str) -> Result<(), RuntimeError> {
        let mut socket = self.connect(RequestKind::DeleteNetwork).await?;

        serpentine_internal::write_length_prefixed(&mut socket, namespace.as_bytes()).await?;

        Ok(())
    }
}
