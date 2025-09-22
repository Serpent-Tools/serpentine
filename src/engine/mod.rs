//! Contains the node engine, as well as node type definitions.

pub mod data_model;
pub mod nodes;
mod scheduler;

use miette::Diagnostic;
use thiserror::Error;

use crate::snek::CompileResult;

/// An error encountered while running the source code
#[derive(Debug, Error, Diagnostic)]
pub enum RuntimeError {
    /// The graph has no end node
    #[error("No end node in graph")]
    #[diagnostic(code(runtime::no_end))]
    NoEnd,

    /// Unhandled internal error.
    #[error("INTERNAL ERROR - this is a bug, please report it.\n{0}")]
    #[diagnostic(code(internal_error))]
    InternalError(String),
}

impl RuntimeError {
    /// Create a `ParsingError::InternalError`, but panic in debug mode instead
    fn internal(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        debug_assert!(false, "{msg}");
        Self::InternalError(msg)
    }
}

/// Run the given compilation result
pub fn run(compile_result: CompileResult) -> Result<(), crate::SerpentineError> {
    let end_node = compile_result
        .end_node
        .ok_or(crate::SerpentineError::Runtime(RuntimeError::NoEnd))?;

    let scheduler = scheduler::Scheduler::new(compile_result.nodes, compile_result.graph);
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| {
            crate::SerpentineError::Runtime(RuntimeError::internal(format!(
                "Failed to start tokio {err}"
            )))
        })?
        .block_on(scheduler.get_output(end_node))
        .map_err(crate::SerpentineError::Runtime)?;

    println!("{}", result.describe());

    Ok(())
}
