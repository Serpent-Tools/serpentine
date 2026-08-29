//! Implementation of the snek language.
//! Snek is pretty simplistic.
//! You define nodes, and their connections.
//!
//! ```snek
//! base_image = Image("...") > Exec("cargo install cargo-nextest");
//!
//! tests = base_image > Exec("cargo nextest run");
//! clippy = base_image > Exec("cargo clippy");
//!
//! export DEFAULT = base_image
//!     > !(tests, clippy) Exec("cargo build")
//!     > Export("/target/release/app")
//!     > ToHost("./bin/app");
//! ```

mod ast;
mod compiler;
mod ir;
mod parser;
mod resolver;
pub mod span;
mod tokenizer;

use std::borrow::Cow;
use std::path::{Path, PathBuf};

pub use compiler::CompileResult;
use miette::Diagnostic;
use span::Span;
use thiserror::Error;

use crate::snek::span::VirtualFile;

/// An error occurred while building the graph.
#[derive(Debug, Error, Diagnostic)]
pub enum CompileError {
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

    /// The tokenizer encountered a value it didnt know what to do with
    #[error("unknown character {char:?} encountered in source code")]
    #[diagnostic(code(parsing::unknown_char))]
    UnknownCharacter {
        /// The location of the character
        #[label("This character was not understood by the lexer")]
        location: Span,
        /// The character
        char: char,
    },

    /// A unterminated string literal was found
    #[error("Unterminated string literal")]
    #[diagnostic(code(parsing::unterminated_string))]
    UnterminatedString {
        /// The location of the string literal
        #[label("String literal not terminated")]
        location: Span,
    },

    /// A literal overflowing the maximum size of an i128 was found
    #[error("Integer literal overflow")]
    #[diagnostic(help("Integer literals must be below 2^127 - 1"))]
    #[diagnostic(code(parsing::integer_overflow))]
    IntegerOverflow {
        /// The location of the literal
        #[label("Integer literal too large")]
        location: Span,
        /// The inner error that caused the overflow
        #[source]
        inner: std::num::ParseIntError,
    },

    /// The parser encountered something different from what it expected.
    #[error("Expected `{expected}`")]
    #[diagnostic(code(parsing::unexpected_token))]
    UnexpectedToken {
        /// The token that was expected
        expected: Cow<'static, str>,
        /// The token that was encountered instead
        got: Cow<'static, str>,
        /// The location of the offending token
        #[label("Got `{got}`")]
        location: Span,
    },

    /// Mismatched type Error
    #[error("Expected `{expected}` got `{got}`")]
    #[diagnostic(code(compiler::type_mismatch))]
    TypeMismatch {
        /// The expected type
        expected: &'static str,
        /// The type we got
        got: &'static str,
        /// The location of the offending type in the source code
        #[label("Has type `{got}`")]
        location: Span,
        /// The node it was arguments for
        #[label("In call to this node")]
        node: Span,
    },

    /// Argument mismatch
    #[error("Expected {expected} arguments got {got}")]
    #[diagnostic(code(compiler::argument_count))]
    #[diagnostic(help(
        "Remember that chaining counts as a argument, `... > Foo(1, 2)` has 3 arguments passed for example."
    ))]
    ArgumentCountMismatch {
        /// The expected number of arguments
        expected: usize,
        /// The number of arguments we got
        got: usize,
        /// The location of the node in the source code
        #[label("This node expects {expected} arguments")]
        location: Span,
    },

    /// A name wasn't found in scope
    #[error("'{ident}' not found in scope")]
    #[diagnostic(help(
        "Serpentine cannot reference items (including functions) before they are defined."
    ))]
    #[diagnostic(code(compiler::item_not_found))]
    ItemNotFound {
        /// The name that wasn't found
        ident: String,
        /// The location of the name
        #[label("Item with this name not found")]
        location: Span,
    },

    /// A name was found in scope but wasnt the expected kind
    /// (e.g. a node was found when a label was expected)
    #[error("Expected a {expected}")]
    #[diagnostic(code(compiler::wrong_item_kind))]
    WrongItemKind {
        /// The expected kind
        expected: &'static str,
        /// The actual kind
        got: &'static str,
        /// The location of the name
        #[label("This is a '{got}'")]
        location: Span,
    },

    /// Statement found in unexpected context.
    #[error("{stmt} not allowed in {context}")]
    #[diagnostic(code(compiler::invalid_statement))]
    #[diagnostic(help("Maybe you meant to use `{maybe}` instead?"))]
    InvalidStatement {
        /// The statement that was invalid
        stmt: &'static str,
        /// The context it was found in
        context: &'static str,
        /// A possible alternative statement
        maybe: &'static str,
        /// The location of the statement
        #[label("This statement is not allowed here")]
        location: Span,
    },

    /// A return was missing.
    #[error("No return found.")]
    #[diagnostic(code(compiler::no_return))]
    ReturnNotFound {
        /// The function that was missing a return
        #[label("In this function.")]
        location: Span,
    },

    /// Two returns were found.
    #[error("Multiple returns")]
    #[diagnostic(code(compiler::double_return))]
    DoubleReturn {
        /// Where was the second return found.
        #[label("Second return after existing return.")]
        location: Span,
    },

    /// A error occurred while importing a module
    #[error("Importing module '{module}' failed")]
    ImportError {
        /// The module that failed to import
        module: String,
        /// The error that caused the import to fail
        #[diagnostic_source]
        error: Box<dyn Diagnostic + Send + Sync>,
        /// Where the import was
        #[label("In this import statement")]
        location: Span,
    },

    /// A circular import was encountered.
    #[error("Circular import. Module {file} attempted to be imported while resolving.")]
    #[diagnostic(code(compiler::circular_import))]
    CircularImport {
        /// The file that was circular imported
        file: PathBuf,
    },

    /// Unhandled internal error.
    #[error("INTERNAL ERROR - this is a bug, please report it.\n{0}")]
    #[diagnostic(code(internal_error))]
    InternalError(String),
}

impl CompileError {
    /// Create a `CompileError::InternalError`, but panic in debug mode instead
    pub fn internal(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        debug_assert!(false, "{msg}");
        Self::InternalError(msg)
    }
}

/// Compile the given file into a compile result
pub fn compile_graph(
    virtual_file: &VirtualFile,
    file: &Path,
    entry_point: &str,
) -> Result<CompileResult, CompileError> {
    let resolved = resolver::resolve(virtual_file, file, entry_point)?;
    let compiled = compiler::compile(resolved)?;
    Ok(compiled)
}

/// Benchmarks for the snek compiler.
#[cfg(feature = "_bench")]
#[expect(clippy::unwrap_used, reason = "benchmarks")]
pub(crate) mod benchmarks {
    use std::path::PathBuf;

    /// Every pipeline under `test_cases/positive`, in a stable order.
    fn cases() -> Vec<PathBuf> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../test_cases/positive");
        let mut cases: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "snek")
            })
            .collect();
        cases.sort();
        cases
    }

    /// Register the snek compiler benchmarks.
    pub(crate) fn register(criterion: &mut criterion::Criterion) {
        let mut group = criterion.benchmark_group("snek");

        for path in cases() {
            let id = path.file_stem().unwrap().to_string_lossy().into_owned();
            group.bench_function(id, |bencher| {
                bencher.iter(|| {
                    let virtual_file = super::VirtualFile::new();
                    let _ = super::compile_graph(&virtual_file, &path, "DEFAULT");
                });
            });
        }

        group.finish();
    }
}

#[cfg(test)]
#[expect(clippy::panic, reason = "tests")]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[test_log::test]
    fn compile_positive(#[files("../test_cases/positive/**/*.snek")] path: PathBuf) {
        let res = compile_graph(&VirtualFile::new(), &path, "DEFAULT");
        match res {
            Ok(_) => {}
            Err(err) => {
                let err = miette::Report::new(err);
                let err = format!("{err:?}");
                panic!("Failed to compile {path:?}:\n{err}");
            }
        }
    }

    #[rstest]
    #[test_log::test]
    fn compile_negative(#[files("../test_cases/negative/**/*.snek")] path: PathBuf) {
        let virtual_file = VirtualFile::new();
        let res = compile_graph(&virtual_file, &path, "DEFAULT");

        match res {
            Ok(_) => panic!("Unexpectedly compiled {path:?} successfully"),
            Err(err) => {
                let error = miette::Report::new(err).with_source_code(virtual_file.into_readonly());

                crate::test_support::assert_error_snapshot!(
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    error
                );
            }
        }
    }
}
