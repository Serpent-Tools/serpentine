#![doc = include_str!(concat!("../", std::env!("CARGO_PKG_README")))]

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use clap::Parser;
use miette::{Diagnostic, NamedSource};
use thiserror::Error;

mod snek;

/// Serpentine is a build system and programming language.
#[derive(clap::Parser)]
struct Commands {
    /// The pipeline to use, defaults to `./main.snek`
    #[arg(short, long)]
    pipeline: Option<PathBuf>,
}

impl Commands {
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
    ParsingError {
        /// The source code that produced the parsing error
        #[source_code]
        source_code: NamedSource<String>,
        /// The inner Error
        #[related]
        error: Vec<snek::ParsingError>,
    },

    /// Couldnt read a file needed to compile the code
    #[error("Could not read file {file}")]
    #[diagnostic(code(file_error))]
    FileReadingError {
        /// The file that couldnt be read
        file: PathBuf,
        /// The io error that caused it
        #[source]
        inner: std::io::Error,
    },
}

fn main() -> miette::Result<()> {
    let command = Commands::parse();

    snek::process_file(&command.pipeline())?;

    Ok(())
}
