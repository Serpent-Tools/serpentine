//! Client to serpentines sidecar, handles setting up the connections etc.

use std::net::SocketAddr;

use miette::{Context, IntoDiagnostic};
use serpentine_internal::network::{AbstractTopology, ConcreteTopology};
use serpentine_internal::sidecar::{MAGIC_NUMBER, Mount, Request};
use serpentine_internal::{TypedPathSerdeWrapper, read_postcard_frame, write_postcard_frame};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net;
use typed_path::{UnixPath, UnixPathBuf};

/// A sidecar client, holds the location to connect to for each connection.
#[derive(Clone, Copy)]
pub struct Client(SocketAddr);

impl Client {
    /// Create a new client for the specified address.
    pub fn new(addr: SocketAddr) -> Self {
        Self(addr)
    }

    /// Connect to serpentine and send the needed magic bytes
    async fn connect(&self, request: Request) -> miette::Result<net::TcpStream> {
        let mut socket = net::TcpStream::connect(self.0)
            .await
            .into_diagnostic()
            .with_context(|| format!("connecting to the sidecar at {}", self.0))?;
        socket
            .write_all(MAGIC_NUMBER.as_bytes())
            .await
            .into_diagnostic()
            .context("greeting the sidecar")?;
        write_postcard_frame(&request, &mut socket)
            .await
            .into_diagnostic()
            .context("sending the request to the sidecar")?;

        Ok(socket)
    }

    /// Connect to the sidecar and setup a containerd proxy.
    pub async fn containerd(&self) -> miette::Result<net::TcpStream> {
        let socket = self.connect(Request::Proxy).await?;
        Ok(socket)
    }

    /// Connected to the sidecar and request it create a fifo pipe, returns its (in container) path and a reader of the contents.
    pub async fn fifo_pipe(
        &self,
    ) -> miette::Result<(UnixPathBuf, impl AsyncRead + Unpin + Send + 'static)> {
        log::debug!("Creating fifo pipe");
        let mut socket = self.connect(Request::CreateFifo).await?;

        let path: TypedPathSerdeWrapper<typed_path::UnixEncoding> =
            read_postcard_frame(&mut socket)
                .await
                .into_diagnostic()
                .context("reading the fifo path from the sidecar")?;

        Ok((path.0, socket))
    }

    /// Create a network namespace and return its (container) path
    pub async fn create_network(
        &self,
        topology: AbstractTopology,
    ) -> miette::Result<ConcreteTopology> {
        log::debug!("Creating network topology");
        let mut socket = self.connect(Request::CreateNetwork(topology)).await?;

        let network = read_postcard_frame(&mut socket)
            .await
            .into_diagnostic()
            .context("reading the created network from the sidecar")?;
        Ok(network)
    }

    /// Delete a network namespace
    pub async fn delete_network(&self, network: ConcreteTopology) -> miette::Result<()> {
        self.connect(Request::DeleteNetwork(network)).await?;

        Ok(())
    }

    /// Export a file/folder from the given mounts in the sidecar container
    pub async fn export_files(
        &self,
        mounts: impl IntoIterator<Item = containerd_client::types::Mount>,
        path: UnixPathBuf,
    ) -> miette::Result<impl AsyncRead + Unpin + Send + 'static> {
        let mounts = mounts
            .into_iter()
            .map(containerd_to_sidecar_mount)
            .collect();

        let mut socket = self.connect(Request::ExportFiles { mounts, path }).await?;

        let status = socket
            .read_u8()
            .await
            .into_diagnostic()
            .context("reading the export status from the sidecar")?;
        if status != 0 {
            let message: String = read_postcard_frame(&mut socket)
                .await
                .into_diagnostic()
                .context("reading the export failure from the sidecar")?;

            return Err(miette::miette!("sidecar failed to export files: {message}"));
        }

        Ok(socket)
    }

    /// Import a file/folder from the given mounts in the sidecar container
    ///
    /// Returns a writer to write the filesystem stream to, which will be copied into the sidecar container.
    pub async fn import_files(
        &self,
        mounts: impl IntoIterator<Item = containerd_client::types::Mount>,
        path: UnixPathBuf,
    ) -> miette::Result<impl AsyncWrite + Unpin + Send + 'static> {
        let mounts = mounts
            .into_iter()
            .map(containerd_to_sidecar_mount)
            .collect();

        let socket = self.connect(Request::ImportFiles { mounts, path }).await?;

        Ok(socket)
    }

    /// Export the given overlayfs mount from the sidecar.
    ///
    /// returns a reader of the tar stream.
    pub async fn export_layer(
        &self,
        mount: containerd_client::types::Mount,
    ) -> miette::Result<impl AsyncRead + Unpin + Send + 'static> {
        let mount = containerd_to_sidecar_mount(mount);
        let socket = self.connect(Request::ExportLayer(mount)).await?;
        Ok(socket)
    }
}

/// Convert the container mount type to the mount type of the sidecar protocol
fn containerd_to_sidecar_mount(mount: containerd_client::types::Mount) -> Mount {
    Mount {
        type_: mount.r#type.into(),
        source: UnixPath::new(&mount.source).to_path_buf(),
        target: UnixPath::new(&mount.target).to_path_buf(),
        options: mount.options.into_iter().map(Into::into).collect(),
    }
}
