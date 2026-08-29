//! Contains the node engine, as well as node type definitions.

mod cache;
mod containerd;
pub mod data_model;
pub mod docker;
mod filesystem;
pub mod nodes;
mod scheduler;
mod sidecar_client;
mod userdb;

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use miette::{Diagnostic, IntoDiagnostic, Report, WrapErr};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::engine::cache::CacheBackend;
use crate::events::{Lifecycle, Reporter};
use crate::snek::CompileResult;

/// Ctrl-C was pressed
#[derive(Debug, Error, Diagnostic)]
#[error("Execution interrupted by user (Ctrl-C)")]
#[diagnostic(code(execution_interrupted))]
pub struct Interrupted;

/// A violated invariant, i.e. a bug in serpentine rather than a fault in the pipeline being run.
#[derive(Debug, Error)]
#[error("INTERNAL ERROR - this is a bug, please report it.\n{msg}")]
struct InternalError {
    /// The invariant that was violated
    msg: String,
    /// The failure that exposed it
    inner: Option<Box<dyn Diagnostic + Send + Sync>>,
}

impl Diagnostic for InternalError {
    fn code(&self) -> Option<Box<dyn std::fmt::Display + '_>> {
        Some(Box::new("internal_error"))
    }

    fn diagnostic_source(&self) -> Option<&dyn Diagnostic> {
        self.inner.as_ref().map(|inner| &**inner as &dyn Diagnostic)
    }
}

/// Report a violated internal invariant, panicking in debug builds.
#[track_caller]
pub fn internal(msg: impl std::fmt::Display) -> Report {
    let msg = msg.to_string();
    debug_assert!(false, "{msg}");
    InternalError { msg, inner: None }.into()
}

/// Blame a failure on a violated internal invariant.
pub trait WrapInternal<T> {
    /// Attribute this failure to a serpentine bug, keeping any original error as the cause.
    ///
    /// Panics in debug builds, like [`internal`]. A failure the pipeline can legitimately provoke
    /// wants [`WrapErr::wrap_err`] instead.
    fn wrap_internal(self, msg: impl std::fmt::Display) -> miette::Result<T>;
}

impl<T> WrapInternal<T> for Option<T> {
    #[track_caller]
    fn wrap_internal(self, msg: impl std::fmt::Display) -> miette::Result<T> {
        self.ok_or_else(|| internal(msg))
    }
}

impl<T, E: std::error::Error + Send + Sync + 'static> WrapInternal<T> for Result<T, E> {
    #[track_caller]
    fn wrap_internal(self, msg: impl std::fmt::Display) -> miette::Result<T> {
        if let Err(err) = &self {
            debug_assert!(false, "{msg}: {err}");
        }

        self.into_diagnostic().map_err(|err| {
            InternalError {
                msg: msg.to_string(),
                inner: Some(err.into()),
            }
            .into()
        })
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
async fn acquire_file_lock(key: &str) -> miette::Result<FileLockGuard> {
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
    .wrap_internal("lock task panicked")?
    .into_diagnostic()
    .with_context(|| format!("acquiring advisory lock {key:?}"))?;
    Ok(FileLockGuard { file })
}

/// The various providers and interfaces used by the runtime
pub struct RuntimeContext {
    /// The containerd client
    containerd: containerd::Client,
    /// The channel run events are reported through
    reporter: Reporter,
    /// Caching of values
    cache: cache::Cache,
    /// Should external state be exported?
    standalone_cache: bool,
}

impl RuntimeContext {
    /// Create a new runtime context
    async fn new(
        reporter: Reporter,
        cli: &crate::Run,
        cache_backend: Arc<dyn CacheBackend + Send + Sync>,
    ) -> miette::Result<Self> {
        log::debug!("Creating runtime context");

        let containerd = containerd::Client::new(
            reporter.clone(),
            Arc::clone(&cache_backend),
            cli.jobs,
            cli.containerd_namespace.clone(),
        )
        .await?;
        let cache = cache::Cache::new(cache_backend).await?;

        Ok(Self {
            containerd,
            reporter,
            cache,
            standalone_cache: cli.standalone_cache,
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
) -> miette::Result<()> {
    let start_node = compile_result.start_node;

    log::debug!("Nodes: {}", compile_result.graph.len());
    log::debug!("Starting execution at node {start_node:?}");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .into_diagnostic()
        .context("starting the tokio runtime")?;

    runtime.block_on(async {
        let cache_path = cli.get_cache().into_owned();
        let backend = cache::LocalCacheBackend::new(cache_path.clone())
            .await
            .into_diagnostic()
            .with_context(|| format!("opening the cache at {}", cache_path.display()))?;

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
                Err(Interrupted.into())
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
}

/// Benchmarks for the engine pipeline.
#[cfg(all(feature = "_bench", feature = "_test_docker"))]
#[expect(clippy::unwrap_used, reason = "benchmarks")]
pub(crate) mod benchmarks {
    use std::path::{Path, PathBuf};

    use criterion::{BatchSize, Criterion};

    use crate::snek::span::VirtualFile;

    /// Pipeline run by the benchmarks, relative to `test_cases`.
    const CASE: &str = "bench/large.snek";

    /// Compile and run a full pipeline from a snek file.
    fn run_pipeline(
        snek_path: &Path,
        cache_path: &Path,
        standalone_cache: bool,
        namespace: String,
    ) {
        let graph = crate::snek::compile_graph(&VirtualFile::new(), snek_path, "DEFAULT").unwrap();
        let cli = crate::Run {
            pipeline: snek_path.to_path_buf(),
            output: crate::OutputKind::None,
            cache: Some(cache_path.to_path_buf()),
            standalone_cache,
            clean_old: false,
            entry_point: "DEFAULT".into(),
            jobs: 2,
            containerd_namespace: namespace,
        };

        super::run(graph, crate::events::Reporter::none(), &cli).unwrap();
    }

    /// Absolute path to the benchmark pipeline.
    fn case_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../test_cases")
            .join(CASE)
    }

    /// Register the engine pipeline benchmarks.
    pub(crate) fn register(criterion: &mut Criterion) {
        let mut group = criterion.benchmark_group("engine");

        group.sample_size(10).noise_threshold(0.10);

        let path = case_path();

        group.bench_function("live_cold", |bencher| {
            bencher.iter_batched(
                || {
                    (
                        tempfile::TempDir::new().unwrap(),
                        uuid::Uuid::new_v4().to_string(),
                    )
                },
                |(cache, namespace)| run_pipeline(&path, cache.path(), false, namespace),
                BatchSize::PerIteration,
            );
        });

        {
            let cache = tempfile::TempDir::new().unwrap();
            let namespace = uuid::Uuid::new_v4().to_string();
            run_pipeline(&path, cache.path(), false, namespace.clone());

            group.bench_function("live_warm", |bencher| {
                bencher.iter_batched(
                    || namespace.clone(),
                    |namespace| run_pipeline(&path, cache.path(), false, namespace),
                    BatchSize::PerIteration,
                );
            });
        }

        group.bench_function("live_cold_standalone", |bencher| {
            bencher.iter_batched(
                || {
                    (
                        tempfile::TempDir::new().unwrap(),
                        uuid::Uuid::new_v4().to_string(),
                    )
                },
                |(cache, namespace)| run_pipeline(&path, cache.path(), true, namespace),
                BatchSize::PerIteration,
            );
        });

        {
            let cache = tempfile::TempDir::new().unwrap();
            run_pipeline(&path, cache.path(), true, uuid::Uuid::new_v4().to_string());

            group.bench_function("live_warm_standalone", |bencher| {
                bencher.iter_batched(
                    || uuid::Uuid::new_v4().to_string(),
                    |namespace| run_pipeline(&path, cache.path(), true, namespace),
                    BatchSize::PerIteration,
                );
            });
        }

        group.finish();
    }
}
