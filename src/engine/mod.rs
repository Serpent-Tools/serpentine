//! Contains the node engine, as well as node type definitions.

pub mod data_model;
pub mod nodes;
mod scheduler;

use miette::Diagnostic;
use thiserror::Error;

use crate::{
    docker,
    snek::CompileResult,
    tui::{TuiMessage, TuiSender},
};

/// An error encountered while running the source code
#[derive(Debug, Error, Diagnostic)]
pub enum RuntimeError {
    /// A Docker API error
    #[error("Docker API error: {0}")]
    #[diagnostic(code(docker_error))]
    Docker(#[from] bollard::errors::Error),

    /// A command failed to execute
    #[error("Failed to execute command (exit code {code}): {command:?} \n{output}")]
    #[diagnostic(code(command_execution_error))]
    CommandExecution {
        /// The exit code
        code: i64,
        /// The command that was run
        command: Vec<String>,
        /// The stdout/stderr of the command
        output: String,
    },

    /// A exec command failed to parse
    #[error("Failed to parse command: {0}")]
    ExecParse(#[from] shell_words::ParseError),

    /// A filesystem read error
    #[error("Io error")]
    #[diagnostic(code(filesystem_read_error))]
    IoError(#[from] std::io::Error),

    /// Ctrl-C was pressed
    #[error("Execution interrupted by user (Ctrl-C)")]
    #[diagnostic(code(execution_interrupted))]
    CtrlC,

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
    /// The update channel for the TUI
    tui: TuiSender,
}

impl RuntimeContext {
    /// Create a new runtime context
    async fn new(tui: TuiSender) -> Result<Self, RuntimeError> {
        log::debug!("Creating runtime context");
        Ok(Self {
            docker: docker::DockerClient::new(tui.clone()).await?,
            tui,
        })
    }

    /// Shutdown the runtime context, cleaning up any resources
    async fn shutdown(&self) {
        log::debug!("Shutting down runtime context");

        self.tui.send(TuiMessage::ShuttingDown);

        self.docker.shutdown().await;
    }
}

/// Run the given compilation result
pub fn run(compile_result: CompileResult, tui: TuiSender) -> Result<(), crate::SerpentineError> {
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
            let scheduler =
                scheduler::Scheduler::new(compile_result.nodes, compile_result.graph, tui).await?;
            let result = tokio::select!(
                res = scheduler.get_output(start_node) => res.map(|_| ()),
                _ = tokio::signal::ctrl_c() => {
                    log::warn!("Execution interrupted by user");
                    Err(RuntimeError::CtrlC)
                }
            );
            scheduler.context().shutdown().await;
            result
        })
        .map_err(crate::SerpentineError::Runtime)?;

    Ok(())
}
