//! Handles the execution of a graph

use tokio::sync::OnceCell;

use crate::engine::RuntimeError;
use crate::engine::data_model::{Data, Graph, NodeInstanceId, NodeStorage};

/// Executes the various nodes
pub struct Scheduler {
    /// The graph we are running
    graph: Graph,
    /// Node implementations
    nodes: NodeStorage,
    /// The list of outputs of nodes, indexes by node instance ids
    data: Vec<OnceCell<Data>>,
}

impl Scheduler {
    /// Create a new scheduler to run the given graph
    pub fn new(nodes: NodeStorage, graph: Graph) -> Self {
        Self {
            data: vec![OnceCell::new(); graph.len()],
            nodes,
            graph,
        }
    }

    /// Retrieve a data from a node.
    /// If the node hasnt started starts running it, then awaits on the result.
    pub async fn get_output(&self, node: NodeInstanceId) -> Result<&Data, RuntimeError> {
        let Some(cell) = self.data.get(node.0) else {
            return Err(RuntimeError::internal("NodeInstanceId out of bounds"));
        };

        cell.get_or_try_init(async || {
            let node = self.graph.get(node)?;
            let node_impl = self
                .nodes
                .get(node.kind)
                .ok_or_else(|| RuntimeError::internal("NodeKind out of bounds"))?;
            node_impl.execute(self, &node.inputs).await
        })
        .await
    }
}
