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
    pub fn new(nodes: NodeStorage, graph: Graph, context: Rc<RuntimeContext>) -> Self {
        Self {
            data: vec![OnceCell::new(); graph.len()],
            nodes,
            graph,
            context,
        }
    }

    /// Return the runtime context
    pub fn context(&self) -> &Rc<RuntimeContext> {
        &self.context
    }

    /// Retrieve a data from a node. AAAAAAAAAA BBBBBBBBBBBBBB CCCCC
    /// If the node hasn't started starts running it, then awaits on the result.
    pub async fn get_output(&self, node_id: NodeInstanceId) -> Result<&Data, RuntimeError> {
        let Some(cell) = self.data.get(node_id.index()) else {
            return Err(RuntimeError::internal("NodeInstanceId out of bounds"));
        };

        let res = cell
            .get_or_try_init(async || {
                let node = self.graph.get(node_id);
                self.context.tui.send(crate::tui::TuiMessage::PendingNode);

                futures_util::future::try_join_all(
                    node.phantom_inputs.iter().map(|id| self.get_output(*id)),
                )
                .await?;
                let node_impl = self.nodes.get(node.kind);

                log::debug!("Executing node {node_id:?}",);
                let res = node_impl.execute_raw(node.kind, self, &node.inputs).await;
                self.context.tui.send(crate::tui::TuiMessage::NodeFinished);
                res
            })
            .await;
        log::debug!("Got output of node {node_id:?}: {res:?}");
        res
    }
}
