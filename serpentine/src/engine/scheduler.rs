//! Handles the execution of a graph

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::{FutureExt, TryFutureExt};
use miette::{Diagnostic, IntoDiagnostic, LabeledSpan, Report, Severity, SourceCode};
use thiserror::Error;
use tokio::sync::OnceCell;
use tokio_util::task::AbortOnDropHandle;

use super::RuntimeContext;
use crate::engine::data_model::{Data, Graph, NodeInstanceId, NodeStorage};

/// A `Arc<miette::Report>` to allow cloning.
///
/// Requires implementing a bunch of stuff to allow converting transparently into a `miette::Report`
#[derive(Clone)]
pub struct SharedReport(Arc<Report>);

impl From<Report> for SharedReport {
    fn from(report: Report) -> Self {
        Self(Arc::new(report))
    }
}

impl fmt::Debug for SharedReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl fmt::Display for SharedReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&*self.0, f)
    }
}

impl std::error::Error for SharedReport {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        (**self.0).source()
    }
}

impl Diagnostic for SharedReport {
    fn code<'this>(&'this self) -> Option<Box<dyn fmt::Display + 'this>> {
        (**self.0).code()
    }
    fn severity(&self) -> Option<Severity> {
        (**self.0).severity()
    }
    fn help<'this>(&'this self) -> Option<Box<dyn fmt::Display + 'this>> {
        (**self.0).help()
    }
    fn url<'this>(&'this self) -> Option<Box<dyn fmt::Display + 'this>> {
        (**self.0).url()
    }
    fn source_code(&self) -> Option<&dyn SourceCode> {
        (**self.0).source_code()
    }
    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        (**self.0).labels()
    }
    fn related<'this>(
        &'this self,
    ) -> Option<Box<dyn Iterator<Item = &'this dyn Diagnostic> + 'this>> {
        (**self.0).related()
    }
    fn diagnostic_source(&self) -> Option<&dyn Diagnostic> {
        (**self.0).diagnostic_source()
    }
}

/// An error from a node, i.e. a runtime error with an associated span.
#[derive(Debug, Error, Diagnostic)]
#[error("Error in node")]
#[diagnostic(code(node_error))]
pub struct NodeError {
    /// The location of the node
    #[label("Error occurred in this node")]
    span: crate::snek::span::Span,
    /// The callstack of the node
    #[label(collection, "In inlined call to")]
    stack_trace: Box<[crate::snek::span::Span]>,
    /// The inner error
    #[diagnostic_source]
    inner: Box<dyn Diagnostic + Send + Sync>,
}

/// Executes the various nodes
pub struct Scheduler {
    /// The graph we are running
    graph: Graph,
    /// Node implementations
    nodes: NodeStorage,
    /// The list of outputs of nodes, indexes by node instance ids
    data: Box<[OnceCell<Result<Data, SharedReport>>]>,
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
    ) -> miette::Result<Vec<Data>> {
        let handles = nodes.iter().map(|&node_id| {
            let scheduler = Arc::clone(self);
            scheduler.get_output(node_id)
        });

        futures_util::future::try_join_all(handles).await
    }

    /// Attach `node_id`'s span to an error from the work that node did itself.
    pub fn node_error(&self, node_id: NodeInstanceId, error: Report) -> Report {
        let node_metadata = &self.graph.get(node_id).1;
        NodeError {
            span: node_metadata.location,
            stack_trace: node_metadata.stack_trace.clone(),
            inner: error.into(),
        }
        .into()
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
    ) -> Pin<Box<dyn Future<Output = miette::Result<Data>> + Send>> {
        Box::pin(async move {
            let Some(cell) = self.data.get(node_id.index()) else {
                return Err(crate::engine::internal("NodeInstanceId out of bounds"));
            };

            let data = cell
                .get_or_init(|| {
                    let scheduler = Arc::clone(&self);
                    AbortOnDropHandle::new(tokio::spawn(scheduler.execute_node(node_id)))
                        .map(|result| result.into_diagnostic().flatten())
                        .map_err(Into::into)
                        .map_ok(|mut data| {
                            data.set_producer(self.graph.get(node_id).1.location);
                            data
                        })
                })
                .await
                .clone();

            log::debug!("Got output of node {node_id:?}: {data:?}");
            data.map_err(miette::Report::new)
        })
    }

    /// Run a single node: resolve its phantom inputs, then execute it.
    async fn execute_node(self: Arc<Self>, node_id: NodeInstanceId) -> miette::Result<Data> {
        let node = &self.graph.get(node_id).0;
        self.context
            .reporter
            .node(crate::events::NodeTransition::Queued);

        self.resolve_all(&node.phantom_inputs).await?;

        let node_impl = self.nodes.get(node.kind);
        log::debug!("Executing node {node_id:?}");
        node_impl
            .execute_raw(node_id, node.kind, Arc::clone(&self), &node.inputs)
            .await
    }
}
