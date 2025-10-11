//! Parses tokens into a ast
#![expect(
    clippy::needless_pass_by_value,
    reason = "While yes Token is technically not Copy and expensive to Clone in some variants, all the tokens we pass by value are cheap to clone"
)]

use std::iter::Peekable;

use super::ast;
use super::span::Spanned;
use super::tokenizer::Token;
use crate::snek::CompileError;
use crate::snek::span::Span;

/// A parser for keeping track of the parsing state.
pub struct Parser {
    /// A peekable iterator over the tokens to parse
    tokens: Peekable<std::vec::IntoIter<Spanned<Token>>>,
}

impl Parser {
    /// Parse the given tokens as a complete snek file
    pub fn parse_file(
        tokens: std::boxed::Box<[Spanned<Token>]>,
    ) -> Result<ast::File, CompileError> {
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
    fn parse_statement(&mut self) -> Result<ast::Statement, CompileError> {
        let token = self.next()?;
        let token_span = token.span();
        match token.take() {
            Token::Return => {
                let expression = self.parse_expression()?;
                self.expect(Token::SemiColon)?;
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
                        expected: "def/ident".to_owned(),
                        got: next_token.describe(),
                        location: next_token_span,
                    }),
                }
            }
            token => Err(CompileError::UnexpectedToken {
                expected: "import/export/return/def/ident".to_owned(),
                got: token.describe(),
                location: token_span,
            }),
        }
    }

    /// Parse a label statement
    fn parse_label(
        &mut self,
        export: Option<Span>,
        token_span: Span,
        ident: Box<str>,
    ) -> Result<ast::Statement, CompileError> {
        let label = ast::Ident(token_span.with(ident));
        self.expect(Token::Eq)?;
        let expression = self.parse_expression()?;
        self.expect(Token::SemiColon)?;
        Ok(ast::Statement::Label {
            export,
            expression,
            label,
        })
    }

    /// Parse a function definition
    fn parse_function_def(&mut self, export: Option<Span>) -> Result<ast::Statement, CompileError> {
        let name = self.expect_ident()?;

        self.expect(Token::OpenParen)?;
        let paramaters =
            self.parse_list(Token::ClosingParen, Some(Token::Comma), Self::expect_ident)?;

        self.expect(Token::OpenBracket)?;
        let statements = self.parse_list(Token::ClosingBracket, None, Self::parse_statement)?;

        Ok(ast::Statement::Function {
            export,
            name,
            paramters: paramaters.into_boxed_slice(),
            statements: statements.into_boxed_slice(),
        })
    }

    /// Parse an import statement
    fn parse_import(&mut self, export: Option<Span>) -> Result<ast::Statement, CompileError> {
        let path = self.expect_str()?;
        self.expect(Token::As)?;
        let name = self.expect_ident()?;
        self.expect(Token::SemiColon)?;
        Ok(ast::Statement::Import { export, path, name })
    }

    /// Parse a expression
    fn parse_expression(&mut self) -> Result<ast::Expression, CompileError> {
        let value = self.parse_atom()?;

        if self.peek()? == Token::Pipe {
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
    fn parse_atom(&mut self) -> Result<ast::Expression, CompileError> {
        Ok(match self.peek()? {
            Token::Numeric(value) => {
                let span = self.next()?.span();
                ast::Expression::Number(span.with(value))
            }
            Token::String(value) => {
                let span = self.next()?.span();
                ast::Expression::String(span.with(value))
            }
            Token::Ident(_) => {
                let item = self.parse_item_path()?;
                let item_span = item.span();
                if self.peek()? == Token::OpenParen {
                    ast::Expression::Node(self.parse_node(Some(item))?)
                } else {
                    ast::Expression::Label(item_span.with(item))
                }
            }
            _ => ast::Expression::Node(self.parse_node(None)?),
        })
    }

    /// Parse a chain of nodes, `expression > Node > Node`
    fn parse_chain(&mut self, start: ast::Expression) -> Result<ast::Chain, CompileError> {
        let mut nodes = Vec::new();
        while self.peek()? == Token::Pipe {
            self.next()?;
            nodes.push(self.parse_node(None)?);
        }

        Ok(ast::Chain {
            start: Box::new(start),
            nodes: nodes.into_boxed_slice(),
        })
    }

    /// Parse a node
    ///
    /// If `name` is passed its assumed a previous parser attempted to grab a ident, but then
    /// realized it was a node.
    fn parse_node(
        &mut self,
        pre_parsed_name: Option<ast::ItemPath>,
    ) -> Result<Spanned<ast::Node>, CompileError> {
        let name;
        let mut phantom_inputs = Vec::new();
        let start_span;

        if let Some(pre_parsed_name) = pre_parsed_name {
            start_span = pre_parsed_name.span();
            name = start_span.with(pre_parsed_name);
        } else {
            // REFACTOR: Create a `.next_if` method
            if self.peek()? == Token::Wait {
                start_span = self.next()?.span();

                if self.peek()? == Token::OpenParen {
                    self.next()?;
                    phantom_inputs = self
                        .parse_list(
                            Token::ClosingParen,
                            Some(Token::Comma),
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

        self.expect(Token::OpenParen)?;
        let arguments = self.parse_list(
            Token::ClosingParen,
            Some(Token::Comma),
            Self::parse_expression,
        )?;
        let end_span = self.tokens.peek().map_or(start_span, Spanned::span);

        let node = ast::Node {
            name,
            arguments: arguments.into_boxed_slice(),
            phantom_inputs: phantom_inputs.into_boxed_slice(),
        };

        Ok(start_span.join(end_span).with(node))
    }

    /// Parse a list of items from the token stream.
    /// Will stop once hits `closing`.
    /// If provided will consume `seperator` between calls to `parser`.
    fn parse_list<T>(
        &mut self,
        closing: Token,
        seperator: Option<Token>,
        parser: impl Fn(&mut Self) -> Result<T, CompileError>,
    ) -> Result<Vec<T>, CompileError> {
        let seperator = seperator.as_ref();
        let mut result = Vec::new();
        while self.peek()? != closing {
            result.push(parser(self)?);

            if let Some(seperator) = seperator
                && self.peek()? == *seperator
            {
                self.next()?;
            }
        }
        self.next()?;
        Ok(result)
    }

    /// Parse a item path
    fn parse_item_path(&mut self) -> Result<ast::ItemPath, CompileError> {
        let base = self.expect_ident()?;
        let mut rest = Vec::new();
        while self.peek()? == Token::Path {
            self.next()?;
            rest.push(self.expect_ident()?);
        }

        Ok(ast::ItemPath {
            base,
            rest: rest.into_boxed_slice(),
        })
    }

    /// If the next token is the given token return its span, otherwise return a error
    fn expect(&mut self, expected_token: Token) -> Result<Span, CompileError> {
        let token = self.next()?;
        if *token == expected_token {
            Ok(token.span())
        } else {
            Err(CompileError::UnexpectedToken {
                expected: expected_token.describe(),
                got: token.describe(),
                location: token.span(),
            })
        }
    }

    /// If the next token is a identifier return it, otherwiser return a error.
    fn expect_ident(&mut self) -> Result<ast::Ident, CompileError> {
        let token = self.next()?;
        if let Token::Ident(ident) = (*token).clone() {
            Ok(ast::Ident(token.span().with(ident)))
        } else {
            Err(CompileError::UnexpectedToken {
                expected: "identifier".to_owned(),
                got: token.describe(),
                location: token.span(),
            })
        }
    }

    /// If the next token is a string return it, otherwiser return a error.
    fn expect_str(&mut self) -> Result<Spanned<Box<str>>, CompileError> {
        let token = self.next()?;
        if let Token::String(value) = (*token).clone() {
            Ok(token.span().with(value))
        } else {
            Err(CompileError::UnexpectedToken {
                expected: "string".to_owned(),
                got: token.describe(),
                location: token.span(),
            })
        }
    }

    /// Return the next value in the token stream
    fn peek(&mut self) -> Result<Token, CompileError> {
        match self.tokens.peek() {
            Some(token) => Ok((**token).clone()),
            None => Err(CompileError::internal(
                "Hit end of token list before hitting EOF token.",
            )),
        }
    }

    /// Return the next value in the token stream and advance it.
    fn next(&mut self) -> Result<Spanned<Token>, CompileError> {
        match self.tokens.next() {
            Some(token) => Ok(token),
            None => Err(CompileError::internal(
                "Hit end of token list before hitting EOF token.",
            )),
        }
    }
}
