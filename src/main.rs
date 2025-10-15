#![doc = include_str!(concat!("../", std::env!("CARGO_PKG_README")))]

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use clap::Parser;
use miette::Diagnostic;
use thiserror::Error;

mod engine;
mod snek;

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

fn main() -> miette::Result<()> {
    let command = Cli::parse();
    env_logger::init();

    log::info!("Compiling pipeline: {}", command.pipeline().display());
    let result = snek::compile_graph(&command.pipeline())?;
    log::info!("Executing pipeline");
    engine::run(result)?;

    Ok(())
}
