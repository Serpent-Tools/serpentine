//! Contains the node engine, as well as node type definitions.

pub mod data_model;
pub mod nodes;
mod scheduler;

use miette::Diagnostic;
use thiserror::Error;

use crate::{docker, snek::CompileResult};

/// An error encountered while running the source code
#[derive(Debug, Error, Diagnostic)]
pub enum RuntimeError {
    /// A Docker API error
    #[error("Docker API error: {0}")]
    #[diagnostic(code(docker_error))]
    Docker(#[from] bollard::errors::Error),

    /// A command failed to execute
    #[error("Failed to execute command (exit code {code}): {command:?}")]
    #[diagnostic(code(command_execution_error))]
    CommandExecution {
        /// The exit code
        code: i64,
        /// The command that was run
        command: Vec<String>,
    },

    /// A exec command failed to parse
    #[error("Failed to parse command: {0}")]
    ExecParse(#[from] shell_words::ParseError),

    /// Reading the filesystem into a tarball failed
    #[error("Failed to read filesystem into tarball: {0}")]
    #[diagnostic(code(filesystem_read_error))]
    FilesystemRead(#[from] std::io::Error),

    /// Unhandled internal error.
    #[error("INTERNAL ERROR - this is a bug, please report it.\n{0}")]
    #[diagnostic(code(internal_error))]
    InternalError(String),
}

impl RuntimeError {
    /// Create a `ParsingError::InternalError`, but panic in debug mode instead
    pub fn internal(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        debug_assert!(false, "{msg}");
        Self::InternalError(msg)
    }
}

/// The various providers and interfaces used by the runtime
pub struct RuntimeContext {
    /// The docker client
    docker: docker::DockerClient,
}

impl RuntimeContext {
    /// Create a new runtime context
    fn new() -> Result<Self, RuntimeError> {
        log::debug!("Creating runtime context");
        Ok(Self {
            docker: docker::DockerClient::new()?,
        })
    }

    /// Shutdown the runtime context, cleaning up any resources
    async fn shutdown(&self) {
        log::debug!("Shutting down runtime context");
        self.docker.shutdown().await;
    }
}

/// Run the given compilation result
pub fn run(compile_result: CompileResult) -> Result<(), crate::SerpentineError> {
    let start_node = compile_result.start_node;

    log::debug!("Nodes: {}", compile_result.graph.len());
    log::debug!("Starting execution at node {start_node:?}");

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| {
            crate::SerpentineError::Runtime(RuntimeError::internal(format!(
                "Failed to start tokio {err}"
            )))
        })?
        .block_on(async {
            let scheduler = scheduler::Scheduler::new(compile_result.nodes, compile_result.graph)?;

            let res = scheduler.get_output(start_node).await.cloned();
            scheduler.context().shutdown().await;
            res
        })
        .map_err(crate::SerpentineError::Runtime)?;

    Ok(())
}
