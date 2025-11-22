//! Implementation of the snek language.
//!
//! Snek is pretty simplistic.
//! You define nodes, and their connections.
//!
//! ```snek
//! Image("...") .> Exec("cargo install nextest") .= base_image;
//!
//! base_image > Exec("cargo nextest run") 'tests;
//! base_image > Exec("cargo clippy") 'clippy;
//!
//! base_image > !'tests !'clippy Exec("cargo build") .> File("/target/...") .> Export("./bin/app") 'export;
//!
//! !'export RESULT;
//! ```

mod ast;
mod compiler;
mod ir;
mod parser;
mod resolver;
pub mod span;
mod tokenizer;

use std::path::{Path, PathBuf};

pub use compiler::CompileResult;
use miette::Diagnostic;
use span::Span;
use thiserror::Error;

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

    /// The parser encountered something different from what it expected.
    #[error("Expected `{expected}`")]
    #[diagnostic(code(parsing::unexpected_token))]
    UnexpectedToken {
        /// The token that was expected
        expected: String,
        /// The token that was encountered instead
        got: String,
        /// The location of the offending token
        #[label("Got `{got}`")]
        location: Span,
    },

    /// Mismatched type Error
    #[error("Expected `{expected}` got `{got}`")]
    #[diagnostic(code(compiler::type_mismatch))]
    TypeMismatch {
        /// The expected type
        expected: String,
        /// The type we got
        got: String,
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
pub fn compile_graph(file: &Path) -> Result<CompileResult, crate::SerpentineError> {
    let resolved = resolver::resolve(file)?;
    let compiled = compiler::compile(resolved)?;
    Ok(compiled)
}

#[cfg(test)]
#[expect(clippy::panic, reason = "tests")]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[test_log::test]
    fn compile_positive(#[files("test_cases/positive/**/*.snek")] path: PathBuf) {
        let res = compile_graph(&path);
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
    fn compile_negative(#[files("test_cases/negative/**/*.snek")] path: PathBuf) {
        let res = compile_graph(&path);
        match res {
            Ok(_) => panic!("Unexpectedly compiled {path:?} successfully"),
            Err(err) => {
                let _ = miette::set_hook(Box::new(|_| {
                    let config = miette::GraphicalReportHandler::default();
                    let config = config
                        .with_width(usize::MAX)
                        .with_theme(miette::GraphicalTheme::none());
                    Box::new(config)
                }));

                let err = miette::Report::new(err);
                let err = format!("{err:?}");

                insta::with_settings! { {
                    filters => vec![
                        // Redact file paths
                        (r#"(/[^/\s:"'\]]+)+"#, "<redacted-path>"),
                        // Redact OS error messages, they can be different on different systems
                        (r"(?i)os error \d+: [^\n]+", "OS error <redacted>"),
                    ],
                }, {
                insta::assert_snapshot!(
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    err
                );
                }};
            }
        }
    }
}
