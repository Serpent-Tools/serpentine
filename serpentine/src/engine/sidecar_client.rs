//! Client to serpentines sidecar, handles setting up the connections etc.

use std::net::SocketAddr;

use serpentine_internal::WireFormat;
use serpentine_internal::network::{AbstractTopology, ConcreteTopology};
use serpentine_internal::sidecar::{MAGIC_NUMBER, Mount, RequestKind};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufWriter};
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
    async fn connect(&self, kind: RequestKind) -> Result<BufWriter<net::TcpStream>, RuntimeError> {
        let socket = net::TcpStream::connect(self.0).await?;
        let mut socket = BufWriter::new(socket);
        socket.write_all(MAGIC_NUMBER.as_bytes()).await?;
        socket.write_u8(kind as u8).await?;
        socket.flush().await?;
        Ok(socket)
    }

    /// Connect to the sidecar and setup a containerd proxy.
    pub async fn containerd(&self) -> Result<net::TcpStream, RuntimeError> {
        let socket = self.connect(RequestKind::Proxy).await?;
        Ok(socket.into_inner())
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
    pub async fn create_network(
        &self,
        topology: AbstractTopology,
    ) -> Result<ConcreteTopology, RuntimeError> {
        log::debug!("Creating network topology");
        let mut socket = self.connect(RequestKind::CreateNetwork).await?;
        topology.write(&mut socket).await?;
        socket.flush().await?;

        let concrete_topology = ConcreteTopology::read(&mut socket).await?;
        Ok(concrete_topology)
    }

    /// Delete a network namespace
    pub async fn delete_network(&self, network: ConcreteTopology) -> Result<(), RuntimeError> {
        let mut socket = self.connect(RequestKind::DeleteNetwork).await?;
        network.write(&mut socket).await?;
        socket.flush().await?;

        Ok(())
    }

    /// Export a file/folder from the given mounts in the sidecar container
    pub async fn export_files(
        &self,
        mounts: Vec<containerd_client::types::Mount>,
        path: &str,
    ) -> Result<impl AsyncRead + Unpin + Send, RuntimeError> {
        let mut socket = self.connect(RequestKind::ExportFiles).await?;

        serpentine_internal::write_u64_variable_length(&mut socket, mounts.len() as u64).await?;
        for mount in mounts {
            let mount = Mount {
                type_: mount.r#type.into(),
                source: mount.source.into(),
                target: mount.target.into(),
                options: mount.options.into_iter().map(Into::into).collect(),
            };
            mount.write(&mut socket).await?;
        }

        serpentine_internal::write_length_prefixed(&mut socket, path.as_bytes()).await?;
        socket.flush().await?;

        let status = socket.read_u8().await?;
        if status != 0 {
            let os_error = socket.read_u8().await?;
            let message = serpentine_internal::read_length_prefixed_string(&mut socket).await?;
            let error = if os_error != 0 {
                std::io::Error::from_raw_os_error(i32::from(os_error))
            } else {
                std::io::Error::other(message)
            };
            return Err(RuntimeError::IoError(error));
        }

        Ok(socket)
    }

    /// Import a file/folder from the given mounts in the sidecar container
    pub async fn import_files(
        &self,
        mounts: Vec<containerd_client::types::Mount>,
        path: &str,
        fs_reader: &mut (impl AsyncRead + Send + Unpin),
    ) -> Result<(), RuntimeError> {
        let mut socket = self.connect(RequestKind::ImportFiles).await?;

        serpentine_internal::write_u64_variable_length(&mut socket, mounts.len() as u64).await?;
        for mount in mounts {
            let mount = Mount {
                type_: mount.r#type.into(),
                source: mount.source.into(),
                target: mount.target.into(),
                options: mount.options.into_iter().map(Into::into).collect(),
            };
            mount.write(&mut socket).await?;
        }

        serpentine_internal::write_length_prefixed(&mut socket, path.as_bytes()).await?;

        crate::engine::filesystem::copy_filesystem_stream(fs_reader, &mut socket).await?;
        socket.flush().await?;

        Ok(())
    }
}
