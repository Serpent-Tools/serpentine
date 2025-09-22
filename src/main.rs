#![doc = include_str!(concat!("../", std::env!("CARGO_PKG_README")))]

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use clap::Parser;
use miette::{Diagnostic, NamedSource};
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
    /// We failed to parse the file.
    #[error("Parsing error")]
    Parsing {
        /// The source code that produced the parsing error
        #[source_code]
        source_code: NamedSource<String>,
        /// The inner Error
        #[related]
        error: Vec<snek::ParsingError>,
    },

    /// We failed to compile the file.
    #[error("Compile Error")]
    Compile {
        /// The source code that produced the compile error
        #[source_code]
        source_code: NamedSource<String>,
        /// The compile Error
        #[related]
        error: Vec<snek::CompileError>,
    },

    /// Something failed at runtime.
    #[error(transparent)]
    Runtime(engine::RuntimeError),

    /// Couldnt read a file needed to compile the code
    #[error("Could not read file {file}")]
    #[diagnostic(code(file_error))]
    FileReading {
        /// The file that couldnt be read
        file: PathBuf,
        /// The io error that caused it
        #[source]
        inner: std::io::Error,
    },
}

fn main() -> miette::Result<()> {
    let command = Cli::parse();

    let result = snek::process_file(&command.pipeline())?;
    engine::run(result)?;

    Ok(())
}
