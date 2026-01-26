//! Contains the node engine, as well as node type definitions.

mod cache;
mod containerd;
pub mod data_model;
mod docker;
mod filesystem;
pub mod nodes;
mod scheduler;
mod sidecar_client;

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
    ContainerdTransport(#[from] containerd_client::tonic::transport::Error),

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

    /// Attempted to capture non-utf8 output
    #[error("Failed to capture stdout of command as non-utf8 was found: \n{output}")]
    #[diagnostic(code(command_execution_error))]
    NonUtf8Capture {
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

/// Attempt to load the cache from the settings in the cli
async fn load_cache_from_cli(
    cli: &crate::Run,
    external: &impl cache::ExternalCache,
) -> Result<cache::Cache, RuntimeError> {
    let mut cache_file = tokio::io::BufReader::new(tokio::fs::File::open(cli.get_cache()).await?);
    cache::Cache::load_cache(&mut cache_file, external).await
}

impl RuntimeContext {
    /// Create a new runtime context
    async fn new(tui: TuiSender, cli: &crate::Run) -> Result<Self, RuntimeError> {
        log::debug!("Creating runtime context");

        let containerd = containerd::Client::new(tui.clone(), cli.jobs).await?;
        let cache = match load_cache_from_cli(cli, &containerd).await {
            Ok(cache) => {
                log::info!("Cache loaded, deleting cache file");
                let _ = tokio::fs::remove_file(cli.get_cache()).await;
                cache
            }
            Err(error) => {
                log::error!("{error}");
                log::warn!("Error loading cache from disk, creating empty cache");
                cache::Cache::new()
            }
        };

        Ok(Self {
            containerd,
            tui,
            cache: tokio::sync::Mutex::new(cache),
        })
    }

    /// Shutdown the runtime context, cleaning up any resources
    async fn shutdown(self, cli: &crate::Run) {
        log::debug!("Shutting down runtime context");

        let Self {
            containerd,
            tui,
            cache,
        } = self;
        tui.send(TuiMessage::ShuttingDown);
        let _ = tokio::fs::create_dir_all(cli.get_cache().parent().unwrap_or(Path::new(""))).await;
        match tokio::fs::File::create(cli.get_cache()).await {
            Ok(cache_file) => {
                let _ = cache
                    .into_inner()
                    .save_cache(
                        &mut tokio::io::BufWriter::new(cache_file),
                        &containerd,
                        !cli.clean_old,
                        cli.standalone_cache,
                    )
                    .await;
            }
            Err(err) => {
                log::error!("Failed to create cache file {err}");
            }
        }

        containerd.shutdown().await;
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
pub fn clear_cache(file_path: &Path) -> Result<(), RuntimeError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| RuntimeError::internal("Failed to start tokio"))?
        .block_on(async move {
            let cache_file_read = tokio::fs::File::open(file_path).await?;

            let docker = containerd::Client::new(TuiSender(None), 1).await?;
            let cache =
                cache::Cache::load_cache(&mut tokio::io::BufReader::new(cache_file_read), &docker)
                    .await?;

            // When `keep_old_cache` is set to false `save_cache` will clean out the data not used
            // this run, which is everything.
            let cache_file_write = tokio::fs::File::open(file_path).await?;
            cache
                .save_cache(
                    &mut tokio::io::BufWriter::new(cache_file_write),
                    &docker,
                    false,
                    false,
                )
                .await?;
            tokio::fs::remove_file(file_path).await?;

            Ok::<_, RuntimeError>(())
        })?;

    Ok(())
}
