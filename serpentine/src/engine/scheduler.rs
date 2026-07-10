//! Handles the execution of a graph

use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::OnceCell;
use tokio_util::task::AbortOnDropHandle;

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
    data: Box<[OnceCell<Data>]>,
    /// The runtime context
    context: Arc<RuntimeContext>,
}

impl Scheduler {
    /// Create a new scheduler to run the given graph
    pub fn new(nodes: NodeStorage, graph: Graph, context: Arc<RuntimeContext>) -> Self {
        Self {
            data: std::iter::repeat_with(OnceCell::new)
                .take(graph.len())
                .collect(),
            nodes,
            graph,
            context,
        }
    }

    /// Return the runtime context
    pub fn context(&self) -> &Arc<RuntimeContext> {
        &self.context
    }

    /// Resolve the outputs of several nodes, each on its own task so independent branches of the
    /// graph run across worker threads.
    pub(crate) async fn resolve_all(
        self: &Arc<Self>,
        nodes: &[NodeInstanceId],
    ) -> Result<Vec<Data>, RuntimeError> {
        let handles = nodes.iter().map(|&node_id| {
            let scheduler = Arc::clone(self);
            AbortOnDropHandle::new(tokio::spawn(
                async move { scheduler.get_output(node_id).await },
            ))
        });

        futures_util::future::try_join_all(handles)
            .await
            .map_err(|err| RuntimeError::internal(format!("Node task panicked: {err}")))?
            .into_iter()
            .collect()
    }

    /// Retrieve the output of a node, running it (and its dependencies) if it hasn't started yet.
    ///
    /// The result is memoized, so concurrent callers share a single execution.
    ///
    /// Returns a boxed future with an explicit `Send` bound to anchor the recursive
    /// `get_output` -> `execute_node` -> `resolve_all` -> `spawn(get_output)` cycle, which the
    /// compiler cannot otherwise prove `Send` through.
    pub fn get_output(
        self: Arc<Self>,
        node_id: NodeInstanceId,
    ) -> Pin<Box<dyn Future<Output = Result<Data, RuntimeError>> + Send>> {
        Box::pin(async move {
            let Some(cell) = self.data.get(node_id.index()) else {
                return Err(RuntimeError::internal("NodeInstanceId out of bounds"));
            };

            let data = cell
                .get_or_try_init(|| {
                    let scheduler = Arc::clone(&self);
                    async move { scheduler.execute_node(node_id).await }
                })
                .await?;

            log::debug!("Got output of node {node_id:?}: {data:?}");
            Ok(data.clone())
        })
    }

    /// Run a single node: resolve its phantom inputs, then execute it.
    async fn execute_node(self: Arc<Self>, node_id: NodeInstanceId) -> Result<Data, RuntimeError> {
        let node = self.graph.get(node_id);
        self.context
            .reporter
            .node(crate::events::NodeTransition::Queued);

        self.resolve_all(&node.phantom_inputs).await?;

        let node_impl = self.nodes.get(node.kind);
        log::debug!("Executing node {node_id:?}");
        let res = node_impl
            .execute_raw(node.kind, Arc::clone(&self), &node.inputs)
            .await;

        res.map_err(|err| {
            if matches!(err, RuntimeError::NodeError { .. }) {
                err
            } else {
                RuntimeError::NodeError {
                    node_id,
                    span: node.span(),
                    inner: Box::new(err),
                }
            }
        })
    }
}
