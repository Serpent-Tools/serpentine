//! Code implementing parts of the sidecar protocol, see `sidecar/src/main.rs` and
//! `serpentine/src/engine/sidecar_client.rs` for the server and client sides, this module exists to
//! share certain values.

use typed_path::UnixPathBuf;

use crate::network::{AbstractTopology, ConcreteTopology};

/// Magic number to protect sidecar from garbage data as well as XSRF attacks.
pub const MAGIC_NUMBER: &str = "danger noodle";

/// The port the sidecar listens on
pub const PORT: u16 = 8000;

/// The opening frame serpentine sends the sidecar, naming the operation and its parameters.
///
/// Most operations continue past this frame with data streamed over the same connection, in one or
/// both directions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Request {
    /// Proxy the containerd socket
    Proxy,
    /// Create a fifo pipe
    CreateFifo,
    /// Create a network topology
    CreateNetwork(AbstractTopology),
    /// Delete a network topology
    DeleteNetwork(ConcreteTopology),
    /// Export files from a mount.
    ExportFiles {
        /// The mount stack to export file from.
        mounts: Box<[Mount]>,
        /// The path relative to the mount stack to export.
        #[serde(with = "crate::TypedPathBufRemote")]
        path: UnixPathBuf,
    },
    /// Import files to a mount.
    ///
    /// Stream filesystem afterwards.
    ImportFiles {
        /// The mount stack to import file to.
        mounts: Box<[Mount]>,
        /// The path relative to the mount stack to import to.
        #[serde(with = "crate::TypedPathBufRemote")]
        path: UnixPathBuf,
    },
    /// Export a overlayfs layer given by the given mounts.
    ExportLayer(Mount),
}

/// Mounts options for mounting a snapshot in the sidecar manually.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Mount {
    /// The kind of mount
    pub type_: Box<str>,
    /// The source to mount from
    #[serde(with = "crate::TypedPathBufRemote")]
    pub source: UnixPathBuf,
    /// The target to mount to
    #[serde(with = "crate::TypedPathBufRemote")]
    pub target: UnixPathBuf,
    /// The options for the mount
    pub options: Box<[Box<str>]>,
}
