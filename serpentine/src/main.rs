#![doc = include_str!(concat!("../../", std::env!("CARGO_PKG_README")))]
#![cfg_attr(
    feature = "_bench",
    expect(unreachable_code, reason = "bench feature replaces main body")
)]

use std::borrow::Cow;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use clap::{CommandFactory, Parser};
use miette::Diagnostic;
use thiserror::Error;
use typed_path::{PlatformPath, PlatformPathBuf};

use crate::engine::RuntimeError;
use crate::events::Reporter;
use crate::snek::span::VirtualFile;

mod engine;
mod events;
mod github_reporter;
mod plain;
mod snek;
#[cfg(test)]
mod test_support;
mod tui;

/// Serpentine is a workflow runner driven by its own DSL, snek.
#[derive(clap::Parser)]
struct Cli {
    /// Action to take
    #[command(subcommand)]
    command: Command,
}

/// Return the path to the cache directory to use
fn get_default_cache_dir() -> PlatformPathBuf {
    if let Some(project_dirs) = directories::ProjectDirs::from("org", "serpent-tools", "serpentine")
    {
        let cache_dir = project_dirs.cache_dir();
        PlatformPathBuf::from(cache_dir.as_os_str().as_encoded_bytes())
    } else {
        log::warn!("Failed to determine default cache location.");
        PlatformPathBuf::from("./serpentine_cache/")
    }
}

/// Subcommands for serpentine
#[derive(clap::Subcommand)]
enum Command {
    /// Run a serpentine pipeline
    Run(Run),
    /// Clear out serpentine's cache.
    ///
    /// This will delete the specified cache directory, as well as deleting serpentines docker volume.
    ///
    /// NOTE: Dropping the docker volume takes the container layers with it, so this invalidates
    /// every cache on the system and not just the one named here. Standalone caches are the
    /// exception, as they carry the layer diffs themselves and can re-import them.
    Clean {
        /// The cache directory to clean
        cache: Option<PathBuf>,
    },
}

/// Arguments for the run command
#[derive(clap::Args)]
struct Run {
    /// The pipeline to use
    #[arg(short, long, default_value = "./main.snek")]
    pipeline: PathBuf,
    /// The entry point to use for the pipeline
    #[arg(short, long, default_value = "DEFAULT")]
    entry_point: String,
    /// How to render progress.
    #[arg(long, value_enum, default_value = "auto")]
    output: OutputKind,
    /// Location of the cache directory
    ///
    /// This can be useful to set in CI, or per project if you are running multiple serpentine
    /// instances at once as some caching features are racing (this will only ever lead to cache
    /// misses, never corruption).
    #[arg(short, long)]
    cache: Option<PathBuf>,
    /// Also export container layers, and any other external data referenced by the cache to the
    /// cache directory.
    ///
    /// This is intended for use with CI, or generally when the cache needs to be transferred
    /// between systems.
    #[arg(long)]
    standalone_cache: bool,
    /// The containerd namespace to use.
    ///
    /// This is mostly only useful for temporarily disabling the snapshot caches (unless
    /// --standalone-cache has been used).
    ///
    /// NOTE: Serpentine already ensures that multiple instance cooperate in regards to containerd.
    #[arg(long, default_value = "serpentine")]
    containerd_namespace: String,
    /// Delete old cache entries (also cleans out stale container layers).
    #[arg(long)]
    clean_old: bool,
    /// Limit of the number of parallel exec jobs allowed to run
    ///
    /// Due to most build systems already using all available cores it usually smart to set this to
    /// a smaller value, at least when first priming caches.
    #[arg(short, long, default_value_t = 2)]
    jobs: usize,
}

/// How serpentine renders a run's progress.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OutputKind {
    /// Pick from the environment: `github` under GitHub Actions at `--jobs 1`, `plain` under any
    /// other CI or when stdout is not a terminal, and `tui` otherwise.
    Auto,
    /// The live progress view.
    Tui,
    /// Log lines and captured command output on stdout.
    Plain,
    /// Stdout, with each command folded into a GitHub Actions log group.
    ///
    /// Requires `--jobs 1`.
    Github,
    /// Render nothing; the run still writes its log file.
    None,
}

/// How serpentine produces output.
///
/// Every mode but [`OutputKind::None`] is a consumer draining the run's events on its own thread;
/// they differ only in how they render them.
struct OutputMode {
    /// A handle to the reporter
    reporter: Reporter,
    /// Handle to the consumer thread
    handle: Option<std::thread::JoinHandle<()>>,
}

impl OutputMode {
    /// Spawn `consumer` on its own thread to drain this run's events.
    fn spawn(consumer: fn(Receiver<events::SerpentineEvent>)) -> Self {
        let (reporter, receiver) = events::Reporter::channel();
        let handle = std::thread::spawn(move || consumer(receiver));

        Self {
            reporter,
            handle: Some(handle),
        }
    }

    /// Drop every event at the point it is emitted.
    fn none() -> Self {
        Self {
            reporter: Reporter::none(),
            handle: None,
        }
    }

    /// Get the reporter to use
    fn get_reporter(&self) -> Reporter {
        self.reporter.clone()
    }

    /// Shutdown this output mode
    fn shutdown(self) {
        if let Some(handle) = self.handle {
            self.reporter.lifecycle(events::Lifecycle::Stop);
            let _ = handle.join();
        }
    }
}

impl Run {
    /// Reject the flag combinations clap cannot express on its own.
    fn validate(&self) -> Result<(), clap::Error> {
        if self.output == OutputKind::Github && self.jobs != 1 {
            return Err(Cli::command().error(
                clap::error::ErrorKind::ArgumentConflict,
                "--output github renders one command at a time, so it requires --jobs 1",
            ));
        }
        Ok(())
    }

    /// The consumer suggested by the environment serpentine is running under.
    fn detect_output(&self) -> fn(Receiver<events::SerpentineEvent>) {
        let ci = ci_info::get();
        if ci.vendor == Some(ci_info::types::Vendor::GitHubActions) && self.jobs == 1 {
            github_reporter::start
        } else if ci.ci || !std::io::stdout().is_terminal() {
            plain::start
        } else {
            tui::start_tui
        }
    }

    /// Get the output mode to use
    fn get_output_mode(&self) -> OutputMode {
        match self.output {
            OutputKind::Auto => OutputMode::spawn(self.detect_output()),
            OutputKind::Tui => OutputMode::spawn(tui::start_tui),
            OutputKind::Plain => OutputMode::spawn(plain::start),
            OutputKind::Github => OutputMode::spawn(github_reporter::start),
            OutputKind::None => OutputMode::none(),
        }
    }

    /// Get the cache to use
    fn get_cache(&self) -> Cow<'_, PlatformPath> {
        if let Some(cache) = &self.cache {
            Cow::Borrowed(PlatformPath::new(cache.as_os_str().as_encoded_bytes()))
        } else {
            Cow::Owned(get_default_cache_dir())
        }
    }
}
/// An error produced by serpentine
#[derive(Debug, Error, Diagnostic)]
enum SerpentineError {
    /// We failed to compile the file.
    #[error("Compile Error")]
    Compile {
        /// The source code that produced the compile error
        #[source_code]
        source_code: snek::span::ReadOnlyVirtualFile,
        /// The compile Error
        #[related]
        error: Vec<snek::CompileError>,
    },

    /// Something failed at runtime.
    #[error("Runtime Error")]
    Runtime {
        /// The source code that produced the runtime error
        #[source_code]
        source_code: snek::span::ReadOnlyVirtualFile,
        /// The error that occurred at runtime
        #[related]
        error: Vec<engine::RuntimeError>,
    },
}

/// Setup logging using `fern`.
fn setup_logging(reporter: events::Reporter) -> miette::Result<()> {
    let project_dirs = directories::ProjectDirs::from("org", "serpent-tools", "serpentine")
        .ok_or_else(|| miette::miette!("Failed to determine log directory"))?;

    let log_dir = project_dirs.data_local_dir().join("logs");

    let current_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |timestamp| timestamp.as_secs());

    let log_file = log_dir.join(format!("{current_timestamp}.log"));
    println!("Saving logs in {}", log_file.display());

    std::fs::create_dir_all(&log_dir).map_err(|error| {
        miette::miette!(
            "Failed to create log directory {}: {}",
            log_dir.display(),
            error
        )
    })?;

    let log_dispatch = fern::Dispatch::new()
        .filter(|metadata| {
            // Filter out noisy docker logs
            if metadata.target().starts_with("serpentine") {
                true
            } else {
                metadata.level() <= log::Level::Info
            }
        })
        .chain(
            fern::Dispatch::new()
                .format(|out, message, record| {
                    out.finish(format_args!(
                        "[{}][{}] {}",
                        record.level(),
                        record.target(),
                        message
                    ));
                })
                .level(log::LevelFilter::Trace)
                .chain(
                    fern::log_file(log_file)
                        .map_err(|error| miette::miette!("Failed to open log file: {}", error))?,
                ),
        )
        .chain(
            fern::Dispatch::new()
                .level(log::LevelFilter::Debug)
                .chain(fern::Output::call(move |record| {
                    reporter.log(
                        record.level(),
                        record.target(),
                        record.args().to_string().into_boxed_str(),
                    );
                })),
        );

    log_dispatch
        .apply()
        .map_err(|error| miette::miette!("Failed to initialize logging: {}", error))?;
    Ok(())
}

fn main() -> miette::Result<()> {
    #[cfg(feature = "_bench")]
    {
        let mut criterion = criterion::Criterion::default().configure_from_args();
        snek::benchmarks::register(&mut criterion);
        #[cfg(feature = "_test_docker")]
        engine::benchmarks::register(&mut criterion);
        criterion.final_summary();
        return Ok(());
    }

    let command = Cli::parse();

    match command.command {
        Command::Run(run) => {
            if let Err(error) = run.validate() {
                error.exit();
            }
            handle_run(&run)
        }
        Command::Clean { cache } => clean_caches(&cache.map_or(get_default_cache_dir(), |path| {
            PlatformPathBuf::from(path.as_os_str().as_encoded_bytes())
        }))
        .map_err(Into::into),
    }
}

/// Clean out serpentine caches and docker state
fn clean_caches(cache: &PlatformPath) -> Result<(), RuntimeError> {
    let output_mode = OutputMode::spawn(plain::start);
    if let Err(err) = setup_logging(output_mode.get_reporter()) {
        eprintln!("Failed to setup logging: {err}");
    }

    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(RuntimeError::from)
        .and_then(|runtime| {
            runtime.block_on(async {
                log::info!("Deleting {}", cache.display());
                let res = tokio::fs::remove_dir_all(
                    serpentine_internal::platform_to_std(cache)
                        .map_err(|err| RuntimeError::internal(err.to_string()))?,
                )
                .await;

                if let Err(err) = res {
                    log::error!("Failed to delete folder: {err}");
                }

                engine::docker::delete_container_and_volume().await?;

                Ok::<_, RuntimeError>(())
            })
        });

    output_mode.shutdown();
    result
}

/// Handle the `run` subcommand
fn handle_run(command: &Run) -> Result<(), miette::Error> {
    println!("Storing cache in {}", command.get_cache().display());

    let output_mode = command.get_output_mode();
    if let Err(error) = setup_logging(output_mode.get_reporter()) {
        eprintln!("Failed to initialize logging: {error}");
    }

    log::info!("Compiling pipeline: {}", command.pipeline.display());
    let virtual_file = VirtualFile::new();
    let result = match snek::compile_graph(&virtual_file, &command.pipeline, &command.entry_point) {
        Ok(result) => result,
        Err(err) => {
            output_mode.shutdown();
            return Err(SerpentineError::Compile {
                source_code: virtual_file.into_readonly(),
                error: vec![err],
            }
            .into());
        }
    };

    log::info!("Executing pipeline");
    let total_nodes = result.graph.len();
    let pipeline = command.pipeline.display().to_string().into_boxed_str();
    output_mode
        .get_reporter()
        .lifecycle(events::Lifecycle::PipelineParsed {
            total_nodes,
            pipeline,
        });
    let result = engine::run(
        virtual_file.into_readonly(),
        result,
        output_mode.get_reporter(),
        command,
    );

    log::info!("Executor returned, waiting for output mode to exit");
    output_mode.shutdown();

    result.map_err(Into::into)
}

#[cfg(test)]
#[expect(clippy::panic, reason = "Tests")]
#[cfg(feature = "_test_docker")]
mod tests {
    use std::path::PathBuf;

    use rstest::rstest;

    use crate::SerpentineError;
    use crate::snek::span::VirtualFile;

    #[rstest]
    #[test_log::test]
    fn live_examples(#[files("../test_cases/live/**/*.snek")] path: PathBuf) {
        let virtual_file = VirtualFile::new();
        let graph = match crate::snek::compile_graph(&virtual_file, &path, "DEFAULT") {
            Ok(graph) => graph,
            Err(err) => {
                let err = miette::Report::new(err);
                let err = format!("{err:?}");
                panic!("Failed to compile {path:?}\n{err}")
            }
        };

        let random_cache_dir = std::env::temp_dir().join(format!(
            "serpentine_test_cache_{}.serpentine_cache",
            uuid::Uuid::new_v4()
        ));

        let cli = crate::Run {
            pipeline: path.clone(),
            output: crate::OutputKind::None,
            cache: Some(random_cache_dir),
            standalone_cache: false,
            clean_old: false,
            entry_point: "DEFAULT".into(),
            jobs: 1,
            containerd_namespace: "serpentine-test".into(),
        };

        if let Err(err) = crate::engine::run(
            virtual_file.into_readonly(),
            graph,
            crate::events::Reporter::none(),
            &cli,
        ) {
            let err = miette::Report::new(err);
            let err = format!("{err:?}");
            panic!("Failed to run {path:?}\n{err}")
        }
    }

    #[rstest]
    #[test_log::test]
    fn live_fails(#[files("../test_cases/live_negative/**/*.snek")] path: PathBuf) {
        let virtual_file = VirtualFile::new();
        let graph = match crate::snek::compile_graph(&virtual_file, &path, "DEFAULT") {
            Ok(graph) => graph,
            Err(err) => {
                let err = miette::Report::new(SerpentineError::Compile {
                    source_code: virtual_file.into_readonly(),
                    error: vec![err],
                });
                let err = format!("{err:?}");
                panic!("Failed to compile {path:?}\n{err}")
            }
        };

        let random_cache_dir = std::env::temp_dir().join(format!(
            "serpentine_test_cache_{}.serpentine_cache",
            uuid::Uuid::new_v4()
        ));

        let cli = crate::Run {
            pipeline: path.clone(),
            output: crate::OutputKind::None,
            cache: Some(random_cache_dir),
            standalone_cache: false,
            clean_old: false,
            entry_point: "DEFAULT".into(),
            jobs: 1,
            containerd_namespace: "serpentine-test".into(),
        };
        if let Err(err) = crate::engine::run(
            virtual_file.into_readonly(),
            graph,
            crate::events::Reporter::none(),
            &cli,
        ) {
            crate::test_support::assert_error_snapshot!(
                path.file_name().unwrap().to_string_lossy().into_owned(),
                err
            );
        } else {
            panic!("Expected failure when running {path:?}, but it succeeded");
        }
    }
}
