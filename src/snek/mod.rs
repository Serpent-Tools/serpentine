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

pub mod ast;
pub mod parser;
pub mod span;
pub mod tokenizer;

use std::path::Path;

use miette::Diagnostic;
use span::Span;
use thiserror::Error;

/// An error encountered while parsing the source code
#[derive(Debug, Error, Diagnostic)]
pub enum ParsingError {
    /// The tokenizer encountered a value it didnt know what to do with
    #[error("unknown characther {char:?} encountered in source code")]
    #[diagnostic(code(parsing::unknown_char))]
    UnknownCharacther {
        /// The location of the characther
        #[label("This characther was not understood by the lexer")]
        location: Span,
        /// The characther
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

    /// Unhandled internal error.
    #[error("INTERNAL ERROR - this is a bug, please report it.\n{0}")]
    #[diagnostic(code(parsing::internal_error))]
    InternalError(String),
}

impl ParsingError {
    /// Create a `ParsingError::InternalError`, but panic in debug mode instead
    fn internal(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        debug_assert!(false, "{msg}");
        Self::InternalError(msg)
    }
}

/// Parse the given string into a ast
pub fn parse(code: &str) -> Result<ast::File<'_>, Vec<ParsingError>> {
    let tokens = tokenizer::Tokenizer::tokenize(code)?;
    parser::Parser::parse_file(tokens)
}

/// Parse and process the given file into a full node graph
pub fn process_file(file: &Path) -> Result<(), crate::SerpentineError> {
    let code = std::fs::read_to_string(file).map_err(|io_error| {
        crate::SerpentineError::FileReadingError {
            file: file.to_owned(),
            inner: io_error,
        }
    })?;

    let ast = match parse(&code) {
        Ok(ast) => ast,
        Err(parse_errors) => {
            return Err(crate::SerpentineError::ParsingError {
                source_code: miette::NamedSource::new(file.to_string_lossy(), code),
                error: parse_errors,
            });
        }
    };

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    proptest::proptest! {
        #[test]
        fn parser_doesnt_crash(code: String) {
            let _ = parse(&code);
        }
    }

    #[test]
    fn can_parse_own_workflow() {
        let our_workflow = PathBuf::from("./ci/main.snek");
        let result = process_file(&our_workflow);

        assert!(result.is_ok(), "{result:?}");
    }
}
