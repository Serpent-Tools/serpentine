//! Contains the node engine, as well as node type definitions.

mod cache;
mod containerd;
pub mod data_model;
mod docker;
pub mod nodes;
mod scheduler;

use std::path::Path;
use std::rc::Rc;

use miette::Diagnostic;
use thiserror::Error;

use crate::snek::CompileResult;
use crate::tui::{TuiMessage, TuiSender};

/// An error encountered while running the source code
#[derive(Debug, Error, Diagnostic)]
pub enum RuntimeError {
    /// A Docker API error
    #[error("Docker API error: {0}")]
    #[diagnostic(code(docker_error))]
    Docker(#[from] bollard::errors::Error),

    /// Containerd API error (for transport)
    #[error("Containerd API error: {0}")]
    #[diagnostic(code(containerd_error))]
    ContainerdTranport(#[from] containerd_client::tonic::transport::Error),

    /// Containerd API error (for containerd itself)
    #[error("Containerd API error: {0}")]
    #[diagnostic(code(containerd_error))]
    ContainerdError(Box<containerd_client::tonic::Status>),

    /// Parsing image error
    #[error("Invalid image name: {0}")]
    #[diagnostic(code(invalid_image))]
    InvalidImageName(#[from] oci_client::ParseError),

    /// Image pull error
    #[error("Error pulling image: {0}")]
    #[diagnostic(code(pull_image))]
    PullError(#[from] oci_client::errors::OciDistributionError),

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
        command: String,
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

impl From<containerd_client::tonic::Status> for RuntimeError {
    fn from(value: containerd_client::tonic::Status) -> Self {
        RuntimeError::ContainerdError(Box::new(value))
    }
}

/// The various providers and interfaces used by the runtime
pub struct RuntimeContext {
    /// The docker client
    containerd: containerd::Client,
    /// The update channel for the TUI
    tui: TuiSender,
    /// Caching of values
    cache: tokio::sync::Mutex<cache::Cache>,
}

impl RuntimeContext {
    /// Create a new runtime context
    async fn new(tui: TuiSender, cli: &crate::Run) -> Result<Self, RuntimeError> {
        log::debug!("Creating runtime context");

        let docker = containerd::Client::new(tui.clone(), cli.jobs).await?;

        let cache = match cache::Cache::load_cache(&cli.get_cache(), &docker).await {
            Ok(cache) => {
                log::info!("Cache loaded, deleting cache file");
                std::fs::remove_file(cli.get_cache())?;
                cache
            }
            Err(error) => {
                log::error!("{error}");
                log::warn!("Error loading cache from disk, creating empty cache");
                cache::Cache::new()
            }
        };

        Ok(Self {
            containerd: docker,
            tui,
            cache: tokio::sync::Mutex::new(cache),
        })
    }

    /// Shutdown the runtime context, cleaning up any resources
    async fn shutdown(self, cli: &crate::Run) {
        log::debug!("Shutting down runtime context");

        let Self {
            containerd: docker,
            tui,
            cache,
        } = self;
        tui.send(TuiMessage::ShuttingDown);
        let _ = cache
            .into_inner()
            .save_cache(
                &cli.get_cache(),
                &docker,
                !cli.clean_old,
                cli.standalone_cache,
            )
            .await;
    }
}

/// Run the given compilation result
pub fn run(
    compile_result: CompileResult,
    tui: TuiSender,
    cli: &crate::Run,
) -> Result<(), crate::SerpentineError> {
    let start_node = compile_result.start_node;

    log::debug!("Nodes: {}", compile_result.graph.len());
    log::debug!("Starting execution at node {start_node:?}");

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| RuntimeError::internal("Failed to start tokio"))?
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

/// Clear out the given cache file
pub fn clear_cache(cache_file: &Path) -> Result<(), RuntimeError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| RuntimeError::internal("Failed to start tokio"))?
        .block_on(async move {
            let docker = containerd::Client::new(TuiSender(None), 1).await?;
            let cache = cache::Cache::load_cache(cache_file, &docker).await?;

            // When `keep_old_cache` is set to false `save_cache` will clean out the data not used
            // this run, which is everything.
            cache.save_cache(cache_file, &docker, false, false).await?;
            std::fs::remove_file(cache_file)?;

            Ok::<_, RuntimeError>(())
        })?;

    Ok(())
}
