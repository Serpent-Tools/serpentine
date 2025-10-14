//! contains the definitions of the various core node types and structures.

use smallvec::SmallVec;

use crate::engine::RuntimeError;
use crate::engine::nodes::NodeImpl;

/// Holds the various forms of data that the node engine uses
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum Data {
    /// A numeric whole number value
    Int(i128),
    /// A string, usually a short literal
    String(Box<str>),
}

/// A companion enum to `Data` denoting the variant/type
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum DataType {
    /// A integer
    Int,
    /// A string
    String,
}

impl Data {
    /// Return a user friendly description of this value.
    pub fn describe(&self) -> String {
        match self {
            Self::Int(value) => format!("{value}"),
            Self::String(value) => format!("{value:?}"),
        }
    }
}

impl DataType {
    /// Return a human friendly version of this type
    pub fn describe(self) -> &'static str {
        match self {
            Self::Int => "integer",
            Self::String => "string",
        }
    }
}

/// Id for referencing the node implementation
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NodeKindId(usize);

/// Id for referencing a node in the graph
#[derive(Clone, Copy, Hash, PartialEq, Eq, Debug)]
pub struct NodeInstanceId(pub usize);

/// Stores the node implementations
pub struct NodeStorage {
    /// The list
    nodes: Vec<Box<dyn NodeImpl>>,
}

impl std::fmt::Debug for NodeStorage {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_list()
            .entries(std::iter::repeat_n("*", self.nodes.len()))
            .finish()
    }
}

impl NodeStorage {
    /// Create a new node storage
    pub fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    /// add a new node to the storage, returning its new id
    pub fn push(&mut self, node: Box<dyn NodeImpl>) -> NodeKindId {
        let id = NodeKindId(self.nodes.len());
        self.nodes.push(node);
        id
    }

    /// returns the node of the given id if found.
    pub fn get(&self, id: NodeKindId) -> Option<&dyn NodeImpl> {
        self.nodes.get(id.0).map(|node| &**node)
    }
}

/// A node in the graph
#[derive(Debug)]
pub struct Node {
    /// The kind of this node
    pub kind: NodeKindId,
    /// The node ids for this inputs
    pub inputs: SmallVec<[NodeInstanceId; 2]>, // 2 usize / usize
    /// Phantom inputs to this node, these will be resolved before the nodes actual logic runs.
    pub phantom_inputs: SmallVec<[NodeInstanceId; 2]>,
}

/// Contains the graph
#[derive(Debug)]
pub struct Graph {
    /// Indexes by `NodeInstanceId`, contains the inputs for the given node
    nodes: Vec<Node>,
}

impl Graph {
    /// Create a new graph
    pub fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    /// Return the number of nodes
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Get the node at this index
    pub fn get(&self, index: NodeInstanceId) -> Result<&Node, RuntimeError> {
        self.nodes
            .get(index.0)
            .ok_or_else(|| RuntimeError::internal("Node id out of bounds"))
    }

    /// Push a new node to the graph and return its id.
    pub fn push(&mut self, node: Node) -> NodeInstanceId {
        let index = self.nodes.len();
        self.nodes.push(node);
        NodeInstanceId(index)
    }
}
