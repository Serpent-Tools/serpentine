//! contains the definitions of the various core node types and structures.

use std::hash::Hash;
use std::rc::Rc;

use crate::engine::containerd;
use crate::engine::nodes::NodeImpl;

/// The name of the file in the tar archives for `FileSystem` representing a single file.
pub const FILE_SYSTEM_FILE_TAR_NAME: &str = "file";

/// A exported filesystem
#[derive(Hash, PartialEq, Eq, Clone, bincode::Encode, bincode::Decode)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub enum FileSystem {
    /// A tar entry containing a file name `FILE_SYSTEM_FILE_TAR_NAME` at `/{FILE_SYSTEM_FILE_TAR_NAME}`
    File(Rc<[u8]>),
    /// A tar containing the contents of a folder directly in the root
    Folder(Rc<[u8]>),
}

impl std::fmt::Debug for FileSystem {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(_) => fmt.debug_tuple("File").finish_non_exhaustive(),
            Self::Folder(_) => fmt.debug_tuple("Folder").finish_non_exhaustive(),
        }
    }
}

/// Holds the various forms of data that the node engine uses
#[derive(Debug, Clone, Hash, PartialEq, Eq, bincode::Encode, bincode::Decode)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub enum Data {
    /// A numeric whole number value
    Int(i128),
    /// A string, usually a short literal
    String(Rc<str>),
    /// A docker container
    Container(containerd::ContainerState),
    /// A file/folder
    FileSystem(FileSystem),
}

impl Data {
    /// Return the type of this data.
    #[must_use]
    pub fn type_(&self) -> DataType {
        match self {
            Data::Int(_) => DataType::Int,
            Data::String(_) => DataType::String,
            Data::Container(_) => DataType::Container,
            Data::FileSystem(_) => DataType::FileSystem,
        }
    }

    /// Check if this data is still valid
    ///
    /// Used by caching system to know if a docker state is delete externally for example.
    pub async fn health_check(&self, containerd: &containerd::Client) -> bool {
        if let Self::Container(state) = self {
            containerd.healthcheck(state).await
        } else {
            true
        }
    }

    /// Cleanup this data
    pub async fn cleanup(self, containerd: &containerd::Client) {
        if let Self::Container(state) = self {
            containerd.delete_state(&state).await;
        }
    }
}

/// A companion enum to `Data` denoting the variant/type
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum DataType {
    /// A integer
    Int,
    /// A string
    String,
    /// A docker container
    Container,
    /// A file or folder
    FileSystem,
}

impl DataType {
    /// Return a human friendly version of this type
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Int => "integer",
            Self::String => "string",
            Self::Container => "container",
            Self::FileSystem => "file/folder",
        }
    }
}

/// A push-only store of T, returning stable IDs.
pub struct Store<T> {
    /// The backing storage of the items
    items: Vec<T>,
}

/// Id into a store of T.
///
/// This is generic over the type T to prevent mixing ids from different stores.
/// Although in theory a program can have multiple stores of the same type T, in which case
/// we would need to be careful to not mix the ids.
/// in practice serpentine only has one store per type T.
pub struct StoreId<T> {
    /// The index into the store
    index: usize,
    /// Phantom data to tie this id to the type T
    _marker: std::marker::PhantomData<T>,
}

#[cfg(test)]
impl<T> proptest::arbitrary::Arbitrary for StoreId<T> {
    type Parameters = ();
    type Strategy = proptest::strategy::BoxedStrategy<Self>;

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        use proptest::prelude::Strategy;

        proptest::arbitrary::any::<usize>()
            .prop_map(|index| StoreId {
                index,
                _marker: std::marker::PhantomData,
            })
            .boxed()
    }
}

impl<T> bincode::Encode for StoreId<T> {
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> Result<(), bincode::error::EncodeError> {
        self.index.encode(encoder)
    }
}

impl<T> StoreId<T> {
    /// Return the index of this id
    ///
    /// This should only be used for secondary maps.
    #[must_use]
    pub fn index(self) -> usize {
        self.index
    }
}

impl<T> Clone for StoreId<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for StoreId<T> {}
impl<T> std::fmt::Debug for StoreId<T> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            fmt,
            "StoreId<{}>({})",
            std::any::type_name::<T>(),
            self.index
        )
    }
}

impl<T> PartialEq for StoreId<T> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}
impl<T> Eq for StoreId<T> {}
impl<T> Hash for StoreId<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.index.hash(state);
    }
}

impl<T> Store<T> {
    /// Create a new empty store
    #[must_use]
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    /// Push a new item to the store, returning its id
    #[must_use = "The id is the only way to refer to the item."]
    pub fn push(&mut self, item: T) -> StoreId<T> {
        let id = StoreId {
            index: self.items.len(),
            _marker: std::marker::PhantomData,
        };
        self.items.push(item);
        id
    }

    /// Get an item from its id.
    ///
    /// This will panic if a id from a different store is used.
    /// In general no stores over the same T should be active in the program at the same time to
    /// make this case impossible.
    #[expect(clippy::expect_used, reason = "Store ids are always valid")]
    #[must_use]
    pub fn get(&self, id: StoreId<T>) -> &T {
        self.items
            .get(id.index)
            .expect("Store id out of bounds of store.")
    }

    /// Get the length of the store
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }
}

impl<T> IntoIterator for Store<T> {
    type IntoIter = std::vec::IntoIter<T>;
    type Item = T;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

/// Id for referencing the node implementation
pub type NodeKindId = StoreId<Box<dyn NodeImpl>>;

/// Stores the node implementations
pub type NodeStorage = Store<Box<dyn NodeImpl>>;

/// A node in the graph
#[derive(Hash, PartialEq, Eq, Clone)]
pub struct Node {
    /// The kind of this node
    pub kind: NodeKindId,
    /// The node ids for this inputs
    pub inputs: Box<[NodeInstanceId]>,
    /// Phantom inputs to this node, these will be resolved before the nodes actual logic runs.
    pub phantom_inputs: Box<[NodeInstanceId]>,
}

/// Id for referencing a node in the graph
pub type NodeInstanceId = StoreId<Node>;

/// Contains the graph
pub type Graph = Store<Node>;
