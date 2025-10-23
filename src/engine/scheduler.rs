//! Handles the execution of a graph

use std::rc::Rc;

use tokio::sync::OnceCell;

use super::RuntimeContext;
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
    /// The runtime context
    context: Rc<RuntimeContext>,
}

impl Scheduler {
    /// Create a new scheduler to run the given graph
    pub async fn new(nodes: NodeStorage, graph: Graph) -> Result<Self, RuntimeError> {
        Ok(Self {
            data: vec![OnceCell::new(); graph.len()],
            nodes,
            graph,
            context: Rc::new(RuntimeContext::new().await?),
        })
    }

    /// Return the runtime context
    pub fn context(&self) -> Rc<RuntimeContext> {
        Rc::clone(&self.context)
    }

    /// Retrieve a data from a node.
    /// If the node hasn't started starts running it, then awaits on the result.
    pub async fn get_output(&self, node: NodeInstanceId) -> Result<&Data, RuntimeError> {
        let Some(cell) = self.data.get(node.index()) else {
            return Err(RuntimeError::internal("NodeInstanceId out of bounds"));
        };

        let res = cell
            .get_or_try_init(async || {
                let node = self.graph.get(node);
                futures_util::future::try_join_all(
                    node.phantom_inputs.iter().map(|id| self.get_output(*id)),
                )
                .await?;

                let node_impl = self.nodes.get(node.kind);
                log::debug!("Executing node {}", node.kind.index());
                node_impl.execute(self, &node.inputs).await
            })
            .await;
        log::trace!("Got output of node {node:?}: {res:?}");
        res
    }
}
