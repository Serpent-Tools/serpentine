//! Contains the node engine, as well as node type definitions.

mod cache;
pub mod data_model;
mod docker;
pub mod nodes;
mod scheduler;

use std::rc::Rc;

use miette::Diagnostic;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::snek::CompileResult;
use crate::tui::{TuiMessage, TuiSender};

/// An error encountered while running the source code
#[derive(Debug, Error, Diagnostic)]
pub enum RuntimeError {
    /// A Docker API error
    #[error("Container API error: {0}")]
    #[diagnostic(code(docker_error))]
    Docker(#[from] bollard::errors::Error),

    /// Error establishing connection to docker/podman
    #[error("Docker/Podman not found")]
    #[diagnostic(code(docker_not_found))]
    #[diagnostic(help(
        "If docker or podman is installed try setting `DOCKER_HOST` environment variable explicitly."
    ))]
    DockerNotFound {
        /// The inner error
        #[diagnostic_source]
        inner: Box<dyn Diagnostic + Send + Sync>,
    },

    /// A bincode deserialization error
    #[error("Error reading cache, please report: {0}")]
    #[diagnostic(code(bincode::decode))]
    BincodeDe(#[from] bincode::error::DecodeError),

    /// A bincode serialization error
    #[error("Error writing cache, please report: {0}")]
    #[diagnostic(code(bincode::encode))]
    BincodeEn(#[from] bincode::error::EncodeError),

    /// The cache was out of date.
    #[error("Cache format version {got} doesn't match current version {current}")]
    CacheOutOfDate {
        /// The version in the cache file
        got: u8,
        /// The version of this binary
        current: u8,
    },

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
    #[error("Io error: {0}")]
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
    /// Caching of values
    cache: Mutex<cache::Cache>,
}

impl RuntimeContext {
    /// Create a new runtime context
    async fn new(tui: TuiSender, cli: &crate::Cli) -> Result<Self, RuntimeError> {
        log::debug!("Creating runtime context");

        let cache = match cache::Cache::load_cache(cli.cache_file().as_ref()) {
            Ok(cache) => cache,
            Err(error) => {
                log::error!("{error}");
                log::warn!("Error loading cache from disk, creating empty cache");
                cache::Cache::new()
            }
        };

        Ok(Self {
            docker: docker::DockerClient::new(tui.clone()).await?,
            tui,
            cache: Mutex::new(cache),
        })
    }

    /// Shutdown the runtime context, cleaning up any resources
    async fn shutdown(self, cli: &crate::Cli) {
        log::debug!("Shutting down runtime context");

        let Self { docker, tui, cache } = self;
        tui.send(TuiMessage::ShuttingDown);
        match cache
            .into_inner()
            .save_cache(cli.cache_file().as_ref(), !cli.clean_old)
        {
            Ok(stale) => {
                for data in stale {
                    data.cleanup(&docker).await;
                }
            }
            Err(err) => {
                log::error!("Failed saving cache: {err}");
            }
        }
        docker.shutdown().await;
    }
}

/// Run the given compilation result
pub fn run(
    compile_result: CompileResult,
    tui: TuiSender,
    cli: &crate::Cli,
) -> Result<(), crate::SerpentineError> {
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
            let context = Rc::new(RuntimeContext::new(tui, cli).await?);
            let scheduler = scheduler::Scheduler::new(
                compile_result.nodes,
                compile_result.graph,
                Rc::clone(&context),
            );
            let result = tokio::select!(
                res = scheduler.get_output(start_node) => res.map(|_| ()),
                _ = tokio::signal::ctrl_c() => {
                    log::warn!("Execution interrupted by user");
                    Err(RuntimeError::CtrlC)
                }
            );

            // Ensure the scheduler context rc is dropped.
            drop(scheduler);

            if let Some(context) = Rc::into_inner(context) {
                context.shutdown(cli).await;
            } else {
                debug_assert!(false, "Context still referenced at shutdown");
                log::warn!("Context still referenced, cant run shutdown");
            }

            result
        })
        .map_err(crate::SerpentineError::Runtime)?;

    Ok(())
}
