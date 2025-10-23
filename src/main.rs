#![doc = include_str!(concat!("../", std::env!("CARGO_PKG_README")))]

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use clap::Parser;
use miette::Diagnostic;
use thiserror::Error;

mod docker;
mod engine;
mod snek;
mod tui;

/// Serpentine is a build system and programming language.
#[derive(clap::Parser)]
struct Cli {
    /// The pipeline to use, defaults to `./main.snek`
    #[arg(short, long)]
    pipeline: Option<PathBuf>,
}

impl Cli {
    /// Get the pipeline, or fallback to defaults.
    fn pipeline(&self) -> Cow<'_, Path> {
        match self.pipeline.as_ref() {
            Some(pipeline) => Cow::Borrowed(pipeline),
            None => Cow::Owned(PathBuf::from("./main.snek")),
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
    Runtime(engine::RuntimeError),
}

fn setup_logging() -> miette::Result<()> {
    let project_dirs = directories::ProjectDirs::from("org", "serpent-tools", "serpentine")
        .ok_or_else(|| miette::miette!("Failed to determine log directory"))?;

    let log_dir = project_dirs.cache_dir().join("logs");

    let current_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|timestamp| timestamp.as_secs())
        .unwrap_or(0);

    let log_file = log_dir.join(format!("{current_timestamp}.log"));

    std::fs::create_dir_all(&log_dir).map_err(|error| {
        miette::miette!(
            "Failed to create log directory {}: {}",
            log_dir.display(),
            error
        )
    })?;

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
        .filter(|metadata| {
            // Filter out noisy docker logs
            if metadata.target().starts_with("serpentine") {
                true
            } else {
                metadata.level() <= log::Level::Info
            }
        })
        .chain(
            fern::log_file(log_file)
                .map_err(|error| miette::miette!("Failed to open log file: {}", error))?,
        )
        .apply()
        .map_err(|error| miette::miette!("Failed to initialize logging: {}", error))?;
    Ok(())
}

fn main() -> miette::Result<()> {
    let command = Cli::parse();

    let res = setup_logging();
    if let Err(error) = res {
        eprintln!("Failed to initialize logging: {error}");
    }

    log::info!("Compiling pipeline: {}", command.pipeline().display());
    let result = snek::compile_graph(&command.pipeline())?;
    log::info!("Executing pipeline");
    engine::run(result)?;

    Ok(())
}
