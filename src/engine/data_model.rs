//! contains the definitions of the various core node types and structures.

use smallvec::SmallVec;

use crate::docker;
use crate::engine::RuntimeError;
use crate::engine::nodes::NodeImpl;

/// Holds the various forms of data that the node engine uses
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum Data {
    /// A numeric whole number value
    Int(i128),
    /// A string, usually a short literal
    String(Box<str>),
    /// A docker container (well in reality an image)
    Container(docker::ContainerState),
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
}

impl DataType {
    /// Return a human friendly version of this type
    pub fn describe(self) -> &'static str {
        match self {
            Self::Int => "integer",
            Self::String => "string",
            Self::Container => "container",
        }
    }
}

/// A push only store of T, returning stable IDs.
pub struct Store<T> {
    /// The backing storage of the items
    items: Vec<T>,
}

/// Id into a store of T.
///
/// This is generic over the type T to prevent mixing ids from different stores.
/// Althought in theory a program can have multiple stores of the same type T, in which case
/// we would need to be careful to not mix the ids.
/// in practice serpentine only has one store per type T.
pub struct StoreId<T> {
    /// The index into the store
    index: usize,
    /// Phantom data to tie this id to the type T
    _marker: std::marker::PhantomData<T>,
}

impl<T> StoreId<T> {
    /// Return the index of this id
    ///
    /// This should only be used for secondary maps.
    pub fn index(&self) -> usize {
        self.index
    }
}

impl<T> Clone for StoreId<T> {
    fn clone(&self) -> Self {
        Self {
            index: self.index,
            _marker: std::marker::PhantomData,
        }
    }
}
impl<T> Copy for StoreId<T> {}
impl<T> std::fmt::Debug for StoreId<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StoreId({})", self.index)
    }
}

impl<T> Store<T> {
    /// Create a new empty store
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    /// Push a new item to the store, returning its id
    pub fn push(&mut self, item: T) -> StoreId<T> {
        let id = StoreId {
            index: self.items.len(),
            _marker: std::marker::PhantomData,
        };
        self.items.push(item);
        id
    }

    /// Get a item from its id.
    pub fn get(&self, id: StoreId<T>) -> Option<&T> {
        self.items.get(id.index)
    }

    /// Get the length of the store
    pub fn len(&self) -> usize {
        self.items.len()
    }
}

/// Id for referencing the node implementation
pub type NodeKindId = StoreId<Box<dyn NodeImpl>>;

/// Stores the node implementations
pub type NodeStorage = Store<Box<dyn NodeImpl>>;

/// A node in the graph
pub struct Node {
    /// The kind of this node
    pub kind: NodeKindId,
    /// The node ids for this inputs
    pub inputs: SmallVec<[NodeInstanceId; 2]>, // 2 usize / usize
    /// Phantom inputs to this node, these will be resolved before the nodes actual logic runs.
    pub phantom_inputs: Vec<NodeInstanceId>,
}

/// Id for referencing a node in the graph
pub type NodeInstanceId = StoreId<Node>;

/// Contains the graph
pub type Graph = Store<Node>;
