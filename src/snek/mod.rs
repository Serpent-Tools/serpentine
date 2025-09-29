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
mod parser;
pub mod span;
mod tokenizer;

use std::path::PathBuf;

pub use compiler::{CompileResult, compile_graph};
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
        /// The node it was arugments for
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
    #[diagnostic(help("Maybe you meant to use a `{maybe}` statement?"))]
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
        #[label("Second return after exsisting return.")]
        location: Span,
    },

    /// A error occurred while inling function
    #[error("Inlining function failed")]
    InlineError {
        /// The error in the function
        #[diagnostic_source]
        error: Box<dyn Diagnostic + Send + Sync>,
        /// Where the function call was
        #[label("In this call")]
        call: Span,
    },

    /// A scope item was overwritten
    #[error("Item '{ident}' already defined in this scope")]
    #[diagnostic(code(compiler::item_already_defined))]
    #[diagnostic(help(
        "Snek sementically speaking does not have a specified exeuction order, so shadowing is not allowed."
    ))]
    ShadowedName {
        /// The name that was shadowed
        ident: String,
        /// The location of the name
        #[label("This name is already defined in this scope")]
        location: Span,
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
