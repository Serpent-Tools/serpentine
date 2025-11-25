#![doc = include_str!(concat!("../", std::env!("CARGO_PKG_README")))]

use std::borrow::Cow;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::Parser;
use miette::Diagnostic;
use thiserror::Error;

mod engine;
mod snek;
mod tui;

/// Serpentine is a build system and programming language.
#[derive(clap::Parser)]
struct Cli {
    /// The pipeline to use
    #[arg(short, long, default_value = "./main.snek")]
    pipeline: PathBuf,
    /// CI mode, disables TUI and logs directly to stdout.
    #[arg(long)]
    ci: bool,
    /// Location of the cache file
    #[arg(short, long)]
    cache: Option<PathBuf>,
    /// Delete old cache entries (also cleans out stale docker images).
    #[arg(long)]
    clean_old: bool,
}

impl Cli {
    /// Should we use the tui?
    ///
    /// This checks the `--ci` flag and whether we are in an interactive terminal
    fn use_tui(&self) -> bool {
        if self.ci {
            false
        } else {
            std::io::stdout().is_terminal()
        }
    }

    /// Return the path to the cache file to use
    fn cache_file(&self) -> Cow<'_, Path> {
        if let Some(cache) = &self.cache {
            Cow::Borrowed(cache)
        } else if let Some(project_dirs) =
            directories::ProjectDirs::from("org", "serpent-tools", "serpentine")
        {
            Cow::Owned(project_dirs.cache_dir().join("cache.bincode"))
        } else {
            log::warn!("Failed to determine default cache location.");
            Cow::Owned(PathBuf::from("./cache.bincode"))
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
        source_code: snek::span::VirtualFile,
        /// The compile Error
        #[related]
        error: Vec<snek::CompileError>,
    },

    /// Something failed at runtime.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Runtime(engine::RuntimeError),
}

/// Setup logging using `fern`.
///
/// Only logs `Info` or higher levels from non-serpentine sources.
/// Logs to file in `~/.cache/serpentine/logs` (or equivalent on other platforms), at TRACE level.
/// If `non_tui` is false sends logs at DEBUG level to `tui`.
/// If `non_tui` is true sends logs at TRACE level to stdout.
fn setup_logging(tui: tui::TuiSender, non_tui: bool) -> miette::Result<()> {
    let project_dirs = directories::ProjectDirs::from("org", "serpent-tools", "serpentine")
        .ok_or_else(|| miette::miette!("Failed to determine log directory"))?;

    let log_dir = project_dirs.cache_dir().join("logs");

    let current_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|timestamp| timestamp.as_secs())
        .unwrap_or(0);

    let log_file = log_dir.join(format!("{current_timestamp}.log"));
    println!("Saving logs in {}", log_file.display());

    std::fs::create_dir_all(&log_dir).map_err(|error| {
        miette::miette!(
            "Failed to create log directory {}: {}",
            log_dir.display(),
            error
        )
    })?;

    fern::Dispatch::new()
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
        .chain(fern::Dispatch::new().chain(if non_tui {
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
                .chain(std::io::stdout())
        } else {
            fern::Dispatch::new()
                .level(log::LevelFilter::Debug)
                .chain(fern::Output::call(move |record| {
                    let message = record.args().to_string();
                    tui.send(tui::TuiMessage::Log(message.into_boxed_str()));
                }))
        }))
        .apply()
        .map_err(|error| miette::miette!("Failed to initialize logging: {}", error))?;
    Ok(())
}

fn main() -> miette::Result<()> {
    let command = Cli::parse();

    println!("Storing cache in {}", command.cache_file().display());

    if command.use_tui() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let res = setup_logging(tui::TuiSender(Some(sender.clone())), false);
        if let Err(error) = res {
            eprintln!("Failed to initialize logging: {error}");
        }

        log::info!("Compiling pipeline: {}", command.pipeline.display());
        let result = snek::compile_graph(&command.pipeline)?;

        log::info!("Executing pipeline");
        let total_nodes = result.graph.len();
        let tui = std::thread::spawn(move || tui::start_tui(receiver, total_nodes));
        let result = engine::run(result, tui::TuiSender(Some(sender.clone())), &command);

        log::info!("Executor returned, waiting for TUI to exit");
        let _ = sender.send(tui::TuiMessage::Shutdown);
        let _ = tui.join();
        ratatui::restore();

        result.map_err(Into::into)
    } else {
        let res = setup_logging(tui::TuiSender(None), true);
        if let Err(error) = res {
            eprintln!("Failed to initialize logging: {error}");
        }

        log::info!("Compiling pipeline: {}", command.pipeline.display());
        let result = snek::compile_graph(&command.pipeline)?;

        log::info!("Executing pipeline");
        let result = engine::run(result, tui::TuiSender(None), &command);

        result.map_err(Into::into)
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "Tests")]
mod tests {
    use std::path::PathBuf;

    use rstest::rstest;

    #[rstest]
    #[test_log::test]
    #[cfg_attr(not(docker_available), ignore = "Docker host not available")]
    fn live_examples(#[files("test_cases/live/**/*.snek")] path: PathBuf) {
        let graph = crate::snek::compile_graph(&path).expect("Failed to compile pipeline");
        let cli = crate::Cli {
            pipeline: PathBuf::from("."),
            ci: true,
            cache: None,
            clean_old: false,
        };
        crate::engine::run(graph, crate::tui::TuiSender(None), &cli).expect("Failed to execute");
    }
}
