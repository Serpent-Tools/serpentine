//! Parses tokens into a ast

use std::iter::Peekable;

use super::ast;
use super::span::Spanned;
use super::tokenizer::Token;
use crate::snek::CompileError;
use crate::snek::span::Span;

/// A parser for keeping track of the parsing state.
pub struct Parser<'arena> {
    /// A peekable iterator over the tokens to parse
    tokens: Peekable<std::vec::IntoIter<Spanned<Token<'arena>>>>,
}

impl<'arena> Parser<'arena> {
    /// Parse the given tokens as a complete snek file
    pub fn parse_file(
        tokens: std::boxed::Box<[Spanned<Token<'arena>>]>,
    ) -> Result<ast::File<'arena>, CompileError> {
        let mut parser = Self {
            tokens: tokens.into_iter().peekable(),
        };

        let mut statements = Vec::new();

        while parser
            .tokens
            .peek()
            .is_some_and(|token| !matches!(**token, Token::Eof))
        {
            statements.push(parser.parse_statement()?);
        }

        Ok(ast::File(statements.into_boxed_slice()))
    }

    /// Parse a statement from the current token stream.
    ///
    /// This expects the next token to be the start of a statement.
    fn parse_statement(&mut self) -> Result<ast::Statement<'arena>, CompileError> {
        let token = self.next()?;
        let token_span = token.span();
        match token.take() {
            Token::Return => {
                let expression = self.parse_expression()?;
                self.expect(&Token::SemiColon)?;
                Ok(ast::Statement::Return {
                    return_kw: token_span,
                    expression,
                })
            }
            Token::Def => self.parse_function_def(None),
            Token::Ident(ident) => self.parse_label(None, token_span, ident),
            Token::Import => self.parse_import(None),
            Token::Export => {
                let next_token = self.next()?;
                let next_token_span = next_token.span();
                match next_token.take() {
                    Token::Def => self.parse_function_def(Some(token_span)),
                    Token::Ident(ident) => {
                        self.parse_label(Some(token_span), next_token_span, ident)
                    }
                    Token::Import => self.parse_import(Some(token_span)),
                    next_token => Err(CompileError::UnexpectedToken {
                        expected: "import, def or ident".into(),
                        got: next_token.describe(),
                        location: next_token_span,
                    }),
                }
            }
            token => Err(CompileError::UnexpectedToken {
                expected: "import, export, return, def or ident".into(),
                got: token.describe(),
                location: token_span,
            }),
        }
    }

    /// Parse a label statement
    ///
    /// This expects the next token to be the `=` of the label statement.
    /// As such it takes in the identifier as a argument.
    fn parse_label(
        &mut self,
        export: Option<Span>,
        token_span: Span,
        ident: &'arena str,
    ) -> Result<ast::Statement<'arena>, CompileError> {
        let label = ast::Ident(token_span.with(ident));
        self.expect(&Token::Eq)?;
        let expression = self.parse_expression()?;
        self.expect(&Token::SemiColon)?;
        Ok(ast::Statement::Label {
            export,
            expression,
            label,
        })
    }

    /// Parse a function definition
    ///
    /// This expects the next token to be the function name.
    /// I.e it expects `export` and `def` to have already been consumed,
    /// and if export was present its span should be passed as a argument.
    fn parse_function_def(
        &mut self,
        export: Option<Span>,
    ) -> Result<ast::Statement<'arena>, CompileError> {
        let name = self.expect_ident()?;

        self.expect(&Token::OpenParen)?;
        let parameters = self.parse_list(
            &Token::ClosingParen,
            Some(&Token::Comma),
            Self::expect_ident,
        )?;

        self.expect(&Token::OpenBracket)?;
        let statements = self.parse_list(&Token::ClosingBracket, None, Self::parse_statement)?;

        Ok(ast::Statement::Function {
            export,
            name,
            parameters: parameters.into_boxed_slice(),
            statements: statements.into_boxed_slice(),
        })
    }

    /// Parse an import statement
    ///
    /// This expects the next token to be the import path.
    /// I.e it expects `export` and `import` to have already been consumed,
    /// and if export was present its span should be passed as a argument.
    fn parse_import(
        &mut self,
        export: Option<Span>,
    ) -> Result<ast::Statement<'arena>, CompileError> {
        let path = self.expect_str()?;
        self.expect(&Token::As)?;
        let name = self.expect_ident()?;
        self.expect(&Token::SemiColon)?;
        Ok(ast::Statement::Import { export, path, name })
    }

    /// Parse a expression
    ///
    /// This expects the next token to be the start of the expression.
    fn parse_expression(&mut self) -> Result<ast::Expression<'arena>, CompileError> {
        let value = self.parse_atom()?;

        if *self.peek()? == Token::Pipe {
            let chain = self.parse_chain(value)?;
            let span = if let Some(last) = chain.nodes.last() {
                chain.start.span().join(last.span())
            } else {
                chain.start.span()
            };

            Ok(ast::Expression::Chain(span.with(chain)))
        } else {
            Ok(value)
        }
    }

    /// Parse a simple expression (literal, var, node)
    ///
    /// This expects the next token to be the start of the expression.
    fn parse_atom(&mut self) -> Result<ast::Expression<'arena>, CompileError> {
        Ok(match self.peek()? {
            &Token::Numeric(value) => {
                let span = self.next()?.span();
                ast::Expression::Number(span.with(value))
            }
            Token::String(value) => {
                // PERF: This is annoying, either we need to do next and check that its still a
                // string (:P), or we change this to a .next() above but then self.parse_node needs
                // to take a even more generic "previous token" argument
                let value = value.clone();
                let span = self.next()?.span();
                ast::Expression::String(span.with(value))
            }
            Token::Ident(_) => {
                let item = self.parse_item_path()?;
                let item_span = item.span();
                if *self.peek()? == Token::OpenParen {
                    ast::Expression::Node(self.parse_node(Some(item))?)
                } else {
                    ast::Expression::Label(item_span.with(item))
                }
            }
            _ => ast::Expression::Node(self.parse_node(None)?),
        })
    }

    /// Parse a chain of nodes, `expression > Node > Node`
    ///
    /// This expects the next token to be the first `>`.
    /// I.e it the start expression is already consumed, and passed as a argument.
    fn parse_chain(
        &mut self,
        start: ast::Expression<'arena>,
    ) -> Result<ast::Chain<'arena>, CompileError> {
        let mut nodes = Vec::new();
        while self.next_if(&Token::Pipe)?.is_some() {
            nodes.push(self.parse_node(None)?);
        }

        Ok(ast::Chain {
            start: Box::new(start),
            nodes: nodes.into_boxed_slice(),
        })
    }

    /// Parse a node
    ///
    /// This expects the next token to be the start of the node call (phantom inputs or function
    /// name)
    /// If `name` is passed its assumed a previous parser attempted to grab a ident, but then
    /// realized it was a node, and hence expects the next token to be the `(` of the call part.
    fn parse_node(
        &mut self,
        pre_parsed_name: Option<ast::ItemPath<'arena>>,
    ) -> Result<Spanned<ast::Node<'arena>>, CompileError> {
        let name;
        let mut phantom_inputs = Vec::new();
        let start_span;

        if let Some(pre_parsed_name) = pre_parsed_name {
            start_span = pre_parsed_name.span();
            name = start_span.with(pre_parsed_name);
        } else {
            if let Some(next_span) = self.next_if(&Token::Wait)? {
                start_span = next_span;

                if self.next_if(&Token::OpenParen)?.is_some() {
                    phantom_inputs = self
                        .parse_list(
                            &Token::ClosingParen,
                            Some(&Token::Comma),
                            Self::parse_item_path,
                        )?
                        .into_iter()
                        .map(|path| {
                            let span = path.span();
                            span.with(path)
                        })
                        .collect();
                } else {
                    let path = self.parse_item_path()?;
                    let span = path.span();
                    phantom_inputs = vec![span.with(path)];
                }
            } else {
                start_span = self
                    .tokens
                    .peek()
                    .ok_or_else(|| CompileError::internal("Hit end of token list"))?
                    .span();
            }

            let path = self.parse_item_path()?;
            let span = path.span();
            name = span.with(path);
        }

        self.expect(&Token::OpenParen)?;
        let arguments = self.parse_list(
            &Token::ClosingParen,
            Some(&Token::Comma),
            Self::parse_expression,
        )?;
        let end_span = arguments
            .last()
            .map_or(start_span, super::ast::Expression::span);

        let node = ast::Node {
            name,
            arguments: arguments.into_boxed_slice(),
            phantom_inputs: phantom_inputs.into_boxed_slice(),
        };

        Ok(start_span.join(end_span).with(node))
    }

    /// Parse a list of items from the token stream.
    /// Will stop once hits `closing`.
    /// If provided will consume `separator` between calls to `parser`.
    fn parse_list<T>(
        &mut self,
        closing: &Token<'static>,
        separator: Option<&Token<'static>>,
        parser: impl Fn(&mut Self) -> Result<T, CompileError>,
    ) -> Result<Vec<T>, CompileError> {
        let mut result = Vec::new();

        let mut first = true;

        while self.peek()? != closing {
            if !first && let Some(separator) = separator {
                self.expect(separator)?;
            }

            result.push(parser(self)?);
            first = false;
        }
        self.next()?;
        Ok(result)
    }

    /// Parse a item path
    ///
    /// This expects the first token to be the first identifier of the path.
    fn parse_item_path(&mut self) -> Result<ast::ItemPath<'arena>, CompileError> {
        let base = self.expect_ident()?;
        let mut rest = Vec::new();
        while self.next_if(&Token::Path)?.is_some() {
            rest.push(self.expect_ident()?);
        }

        Ok(ast::ItemPath {
            base,
            rest: rest.into_boxed_slice(),
        })
    }

    /// If the next token is the given token return its span, otherwise return a error
    fn expect(&mut self, expected_token: &Token<'static>) -> Result<Span, CompileError> {
        let token = self.next()?;
        if *token == *expected_token {
            Ok(token.span())
        } else {
            Err(CompileError::UnexpectedToken {
                expected: expected_token.describe(),
                got: token.describe(),
                location: token.span(),
            })
        }
    }

    /// If the next token is a identifier return it, otherwise return a error.
    fn expect_ident(&mut self) -> Result<ast::Ident<'arena>, CompileError> {
        let token = self.next()?;
        if let Token::Ident(ident) = *token {
            Ok(ast::Ident(token.span().with(ident)))
        } else {
            Err(CompileError::UnexpectedToken {
                expected: "identifier".into(),
                got: token.describe(),
                location: token.span(),
            })
        }
    }

    /// If the next token is a string return it, otherwise return a error.
    fn expect_str(&mut self) -> Result<Spanned<Box<str>>, CompileError> {
        let token = self.next()?;
        let span = token.span();
        let token = token.take();
        if let Token::String(value) = token {
            Ok(span.with(value))
        } else {
            Err(CompileError::UnexpectedToken {
                expected: "string".into(),
                got: token.describe(),
                location: span,
            })
        }
    }

    /// If the next token is `expected_token` consume it and return its span,
    /// If not returns None
    ///
    /// This is effectively a version of `expect` that returns a `None` instead and ensures the
    /// parser state is allowed to continue without failure.
    fn next_if(&mut self, expected_token: &Token<'static>) -> Result<Option<Span>, CompileError> {
        if *self.peek()? == *expected_token {
            Ok(Some(self.next()?.span()))
        } else {
            Ok(None)
        }
    }

    /// Return the next value in the token stream (without consuming it)
    fn peek(&mut self) -> Result<&Token<'arena>, CompileError> {
        match self.tokens.peek() {
            Some(token) => Ok(&**token),
            None => Err(CompileError::internal(
                "Hit end of token list before hitting EOF token.",
            )),
        }
    }

    /// Return the next value in the token stream and advance it.
    fn next(&mut self) -> Result<Spanned<Token<'arena>>, CompileError> {
        match self.tokens.next() {
            Some(token) => Ok(token),
            None => Err(CompileError::internal(
                "Hit end of token list before hitting EOF token.",
            )),
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests")]
mod tests {
    use strum::IntoEnumIterator;

    use super::*;
    use crate::snek::span::{FileId, VirtualFile};
    use crate::snek::tokenizer::{TokenDiscriminants, Tokenizer};

    /// Source containing every payload-free token, tokenized to build the fuzz alphabet.
    const UNIT_TOKEN_SOURCE: &str = "( ) { } ; > , = ! :: return def import export as";

    /// Tokenize [`UNIT_TOKEN_SOURCE`] into the payload-free token alphabet (plus EOF).
    fn unit_tokens() -> Vec<Token<'static>> {
        Tokenizer::tokenize(FileId(0), UNIT_TOKEN_SOURCE)
            .expect("the alphabet source must tokenize")
            .into_iter()
            .map(|token| token.0)
            .collect()
    }

    /// Generator-friendly [`Token`], carrying payloads only where [`Token`] does.
    #[derive(Clone, Debug, bolero::TypeGenerator)]
    enum FuzzToken {
        /// [`Token::Ident`]
        Ident(Box<str>),
        /// [`Token::String`]
        String(Box<str>),
        /// [`Token::Numeric`]
        Numeric(i128),
        /// A payload-free [`Token`], as an index into the alphabet.
        Unit(u8),
    }

    impl FuzzToken {
        /// Convert into a [`Token`], allocating string payloads in the given virtual file.
        fn into_token<'file>(
            self,
            file: &'file VirtualFile,
            alphabet: &[Token<'file>],
        ) -> Token<'file> {
            match self {
                Self::Ident(value) => Token::Ident(file.push("".into(), value).1),
                Self::String(value) => Token::String(value),
                Self::Numeric(value) => Token::Numeric(value),
                Self::Unit(index) => alphabet
                    .iter()
                    .cycle()
                    .nth(usize::from(index))
                    .cloned()
                    .unwrap_or(Token::Eof),
            }
        }
    }

    #[test]
    fn fuzz_alphabet_is_complete() {
        let alphabet = unit_tokens();
        for variant in TokenDiscriminants::iter() {
            let generated = matches!(
                variant,
                TokenDiscriminants::Ident
                    | TokenDiscriminants::String
                    | TokenDiscriminants::Numeric
            ) || alphabet
                .iter()
                .any(|token| TokenDiscriminants::from(token) == variant);
            assert!(generated, "the fuzzer cannot produce {variant:?}");
        }
    }

    #[test]
    fn doesnt_panic() {
        let alphabet = unit_tokens();
        bolero::check!()
            .with_type()
            .for_each(|stream: &Vec<FuzzToken>| {
                let file = VirtualFile::new();
                let tokens = stream
                    .clone()
                    .into_iter()
                    .map(|token| token.into_token(&file, &alphabet))
                    .chain(std::iter::once(Token::Eof))
                    .map(|token| Span::dummy().with(token))
                    .collect();
                let _ = Parser::parse_file(tokens);
            });
    }
}
