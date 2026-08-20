//! Contains the node engine, as well as node type definitions.

mod cache;
mod containerd;
pub mod data_model;
mod docker;
mod filesystem;
pub mod nodes;
mod scheduler;
mod sidecar_client;
mod userdb;

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use miette::Diagnostic;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::engine::cache::CacheBackend;
use crate::events::{Lifecycle, Reporter};
use crate::snek::CompileResult;

/// An error encountered while running the source code
#[derive(Debug, Error, Diagnostic)]
pub enum RuntimeError {
    /// An error from a node, i.e. a runtime error with an associated span.
    #[error("Error in node {node_id:?}")]
    #[diagnostic(code(node_error))]
    NodeError {
        /// Which node the error occurred in
        node_id: crate::engine::data_model::NodeInstanceId,
        /// The location of the node
        #[label("Error occurred in this node")]
        span: crate::snek::span::Span,
        /// The inner error
        #[diagnostic_source]
        inner: Box<dyn Diagnostic + Send + Sync>,
    },

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
        code: u32,
        /// The command that was run
        command: String,
        /// The stdout/stderr of the command
        output: String,
    },

    /// A healthcheck didnt pass in time
    #[error("Healthcheck {check:?} did not pass in {timeout:?}")]
    #[diagnostic(code(healthcheck_timeout))]
    HealthcheckTimeout {
        /// Which healthcheck didnt pass
        check: String,
        /// How long we waited for it to pass
        timeout: std::time::Duration,
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

    /// Could not parse / could not find user info in /etc/passwd
    #[error("Could not resolve user {user:?}: {msg}")]
    #[diagnostic(code(execution_interrupted))]
    UserNotFound {
        /// Which user couldnt be found
        user: userdb::OciUser,
        /// What specific part wasnt found.
        msg: &'static str,
    },

    /// Failed to serialize/deserialize data
    #[error("Failed to serialize/deserialize data: {0}")]
    #[diagnostic(code(serialization_error))]
    SerializationError(#[from] postcard::Error),

    /// Unhandled internal error.
    #[error("INTERNAL ERROR - this is a bug, please report it.\n{0}")]
    #[diagnostic(code(internal_error))]
    InternalError(String),
}

impl RuntimeError {
    /// Create a `RuntimeError::InternalError`, but panic in debug mode instead
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

impl<T> From<std::sync::PoisonError<T>> for RuntimeError {
    fn from(value: std::sync::PoisonError<T>) -> Self {
        RuntimeError::internal(format!("Poisoned lock: {value}"))
    }
}

/// A boxed `AsyncRead`-er.
///
/// A newtype rather than a type alias, as an alias leaves the `dyn`'s region in the type, which
/// rustc universally quantifies and then fails to discharge once the reader reaches the bounds of a
/// future that must be `Send` (rust-lang/rust#102870).
pub(crate) struct BoxedReader(Box<dyn AsyncRead + Send + Unpin>);

impl BoxedReader {
    /// Erase `reader` behind the box.
    pub(crate) fn new(reader: impl AsyncRead + Send + Unpin + 'static) -> Self {
        Self(Box::new(reader))
    }
}

impl AsyncRead for BoxedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

/// A boxed `AsyncWrite`-er.
///
/// A newtype for the same reason as [`BoxedReader`].
pub(crate) struct BoxedWriter(Box<dyn AsyncWrite + Send + Unpin>);

impl BoxedWriter {
    /// Erase `writer` behind the box.
    pub(crate) fn new(writer: impl AsyncWrite + Send + Unpin + 'static) -> Self {
        Self(Box::new(writer))
    }
}

impl AsyncWrite for BoxedWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// Handle to a held cross-process advisory lock. Dropping it releases the lock.
struct FileLockGuard {
    /// The locked file; its handle is unlocked on drop, then closed as the file itself drops.
    file: std::fs::File,
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        // Closing the handle would release the lock on its own, but unlock explicitly so the
        // release is a deliberate step rather than an implicit side effect of the descriptor
        // closing. A failure here still leaves the lock to be freed when the handle closes.
        if let Err(err) = self.file.unlock() {
            log::warn!("Failed to release advisory lock: {err}");
        }
    }
}

impl FileLockGuard {
    /// Release the lock now rather than waiting for the guard to fall out of scope.
    pub(crate) fn unlock(self) {
        // Consuming `self` runs the `Drop` impl above, which releases the lock.
        drop(self);
    }
}

/// Sanitize a lock key into a portable file name.
///
/// Keys such as image digests contain characters like `:` that are legal on Unix but rejected by
/// Windows file systems. Mapping anything outside a conservative set to `_` keeps a stable, unique
/// name on every platform.
fn lock_file_name(key: &str) -> String {
    let sanitized: String = key
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!("{sanitized}.lock")
}

/// Acquire a cross-process advisory lock identified by `key`.
///
/// Serpentine processes share host level state such as the containerd container and its content
/// and snapshot stores. Locking on a stable key lets the first process do a create or fetch while
/// the others wait and then reuse the result, avoiding duplicated work and races on that shared
/// state. The lock is released when the returned guard is dropped.
async fn acquire_file_lock(key: &str) -> Result<FileLockGuard, RuntimeError> {
    let file_name = lock_file_name(key);
    let file = tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
        let dir = std::env::temp_dir().join("serpent-tools").join("locks");
        std::fs::create_dir_all(&dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(dir.join(file_name))?;
        file.lock()?;
        Ok(file)
    })
    .await
    .map_err(std::io::Error::other)??;
    Ok(FileLockGuard { file })
}

/// The various providers and interfaces used by the runtime
pub struct RuntimeContext {
    /// The docker client
    containerd: containerd::Client,
    /// The channel run events are reported through
    reporter: Reporter,
    /// Caching of values
    cache: cache::Cache,
}

impl RuntimeContext {
    /// Create a new runtime context
    async fn new(
        reporter: Reporter,
        cli: &crate::Run,
        cache_backend: Arc<dyn CacheBackend + Send + Sync>,
    ) -> Result<Self, RuntimeError> {
        log::debug!("Creating runtime context");

        let containerd = containerd::Client::new(
            reporter.clone(),
            Arc::clone(&cache_backend),
            cli.jobs,
            "serpentine",
        )
        .await?;
        let cache = cache::Cache::new(cache_backend).await?;

        Ok(Self {
            containerd,
            reporter,
            cache,
        })
    }

    /// Shutdown the runtime context, cleaning up any resources
    async fn shutdown(self, cli: &crate::Run) {
        log::debug!("Shutting down runtime context");

        let Self {
            containerd, cache, ..
        } = self;

        match cache.save(!cli.clean_old).await {
            Err(err) => {
                log::warn!("Failed to save cache: {err}");
            }
            Ok(resources_to_remove) => {
                for resource in resources_to_remove {
                    log::debug!("Removing resource {resource:?}");
                    if let Err(err) = resource.clean(&containerd).await {
                        log::error!("Failed to remove resource {err}");
                    }
                }
            }
        }

        containerd.shutdown().await;
    }
}

/// Run the given compilation result
pub fn run(
    compile_result: CompileResult,
    reporter: Reporter,
    cli: &crate::Run,
) -> Result<(), crate::SerpentineError> {
    let start_node = compile_result.start_node;

    log::debug!("Nodes: {}", compile_result.graph.len());
    log::debug!("Starting execution at node {start_node:?}");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build();
    let Ok(runtime) = runtime else {
        return Err(crate::SerpentineError::Runtime {
            source_code: compile_result.source_code,
            error: vec![RuntimeError::internal("Failed to start tokio")],
        });
    };

    runtime
        .block_on(async {
            let backend = cache::LocalCacheBackend::new(cli.get_cache().into_owned()).await?;

            let context = Arc::new(RuntimeContext::new(reporter, cli, Arc::new(backend)).await?);
            let scheduler = Arc::new(scheduler::Scheduler::new(
                compile_result.nodes,
                compile_result.graph,
                Arc::clone(&context),
            ));
            let result = tokio::select!(
                res = Arc::clone(&scheduler).get_output(start_node) => res.map(|_| ()),
                _ = tokio::signal::ctrl_c() => {
                    log::warn!("Execution interrupted by user");
                    Err(RuntimeError::CtrlC)
                }
            );

            context.reporter.lifecycle(Lifecycle::ShuttingDown);

            // On a clean finish every spawned node task was awaited to completion, so dropping our
            // handle leaves us the sole owner and the first attempt succeeds. On Ctrl-C the run
            // future drops and aborts the in-flight tasks; give them a bounded window to release
            // their scheduler references before reclaiming the context by value for shutdown.
            drop(scheduler);
            let reclaimed = tokio::time::timeout(Duration::from_secs(5), async move {
                let mut context = context;
                loop {
                    match Arc::try_unwrap(context) {
                        Ok(context) => break context,
                        Err(returned) => {
                            context = returned;
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                }
            })
            .await;

            match reclaimed {
                Ok(runtime_context) => runtime_context.shutdown(cli).await,
                Err(_) => {
                    log::warn!("Tasks still in flight after timeout, skipping clean shutdown");
                }
            }

            result
        })
        .map_err(|err| crate::SerpentineError::Runtime {
            source_code: compile_result.source_code,
            error: vec![err],
        })?;

    Ok(())
}

/// Benchmarks for the engine pipeline.
#[cfg(all(feature = "_bench", feature = "_test_docker"))]
#[expect(clippy::unwrap_used, reason = "benchmarks")]
mod benchmarks {
    use std::path::{Path, PathBuf};

    /// Compile and run a full pipeline from a snek file.
    fn run_pipeline(snek_path: &Path, cache_path: &Path, standalone_cache: bool) {
        let graph = crate::snek::compile_graph(snek_path, "DEFAULT").unwrap();
        let cli = crate::Run {
            pipeline: snek_path.to_path_buf(),
            ci: true,
            cache: Some(cache_path.to_path_buf()),
            standalone_cache,
            clean_old: false,
            entry_point: "DEFAULT".into(),
            jobs: 2,
        };

        super::run(graph, crate::events::Reporter::none(), &cli).unwrap();
    }

    /// Benchmark a cold pipeline run (no cache).
    #[divan::bench(threads = false, sample_count = 5, args = ["bench/small.snek", "bench/large.snek"])]
    fn live_cold(bencher: divan::Bencher, snek: &str) {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../test_cases")
            .join(snek);
        bencher
            .with_inputs(|| tempfile::TempDir::new().unwrap())
            .bench_values(|cache| run_pipeline(&path, cache.path(), false));
    }

    /// Benchmark a warm pipeline run (with primed cache).
    #[divan::bench(threads = false, sample_count = 20, args = ["bench/small.snek", "bench/large.snek"])]
    fn live_warm(bencher: divan::Bencher, snek: &str) {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../test_cases")
            .join(snek);
        let cache = tempfile::TempDir::new().unwrap();
        run_pipeline(&path, cache.path(), false);

        bencher.bench(|| run_pipeline(&path, cache.path(), false));
    }

    /// Benchmark a warm pipeline run with standalone cache.
    #[divan::bench(threads = false, sample_count = 5, args = ["bench/small.snek", "bench/large.snek"])]
    fn live_warm_standalone(bencher: divan::Bencher, snek: &str) {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../test_cases")
            .join(snek);
        let cache = tempfile::TempDir::new().unwrap();
        run_pipeline(&path, cache.path(), true);

        bencher.bench(|| run_pipeline(&path, cache.path(), true));
    }
}
