//! Parses tokens into a ast

use std::iter::Peekable;

use super::ast;
use super::span::Spanned;
use super::tokenizer::Token;
use crate::snek::ParsingError;
use crate::snek::span::Span;

/// A parser for keeping track of the parsing state.
pub struct Parser<'src> {
    /// A peekable iterator over the tokens to parse
    tokens: Peekable<std::vec::IntoIter<Spanned<Token<'src>>>>,
}

impl<'src> Parser<'src> {
    /// Parse the given tokens as a complete snek file
    pub fn parse_file(
        tokens: Box<[Spanned<Token<'src>>]>,
    ) -> Result<ast::File<'src>, ParsingError> {
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
    fn parse_statement(&mut self) -> Result<ast::Statement<'src>, ParsingError> {
        match self.peek()? {
            Token::Return => {
                self.next()?;
                let expression = self.parse_expression()?;
                self.expect(Token::SemiColon)?;
                Ok(ast::Statement::Return(expression))
            }
            Token::Def => {
                self.next()?;
                let name = self.expect_ident()?;

                self.expect(Token::OpenParen)?;
                let paramaters =
                    self.parse_list(Token::ClosingParen, Some(Token::Comma), Self::expect_ident)?;

                self.expect(Token::OpenBracket)?;
                let statements =
                    self.parse_list(Token::ClosingBracket, None, Self::parse_statement)?;

                Ok(ast::Statement::Function {
                    name,
                    paramters: paramaters.into_boxed_slice(),
                    statements: statements.into_boxed_slice(),
                })
            }
            _ => self.parse_expression_statement(),
        }
    }

    /// Parse a expression statement from the current token stream.
    fn parse_expression_statement(&mut self) -> Result<ast::Statement<'src>, ParsingError> {
        let expression = self.parse_expression()?;

        let label = if self.peek()? == Token::Eq {
            self.next()?;
            Some(self.expect_ident()?)
        } else {
            None
        };

        self.expect(Token::SemiColon)?;
        Ok(ast::Statement::Expression { expression, label })
    }

    /// Parse a expression
    fn parse_expression(&mut self) -> Result<ast::Expression<'src>, ParsingError> {
        let value = self.parse_atom()?;

        if self.peek()? == Token::Pipe {
            Ok(ast::Expression::Chain(self.parse_chain(value)?))
        } else {
            Ok(value)
        }
    }

    /// Parse a simple expression (literal, var, node)
    fn parse_atom(&mut self) -> Result<ast::Expression<'src>, ParsingError> {
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
                let ident = self.expect_ident()?;
                if self.peek()? == Token::OpenParen {
                    ast::Expression::Node(self.parse_node(Some(ident))?)
                } else {
                    ast::Expression::Label(ident)
                }
            }
            _ => ast::Expression::Node(self.parse_node(None)?),
        })
    }

    /// Parse a chain of nodes, `expression > Node > Node`
    fn parse_chain(
        &mut self,
        start: ast::Expression<'src>,
    ) -> Result<ast::Chain<'src>, ParsingError> {
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
        pre_parsed_name: Option<ast::Ident<'src>>,
    ) -> Result<ast::Node<'src>, ParsingError> {
        let name;
        let mut phantom_inputs = Vec::new();

        if let Some(pre_parsed_name) = pre_parsed_name {
            name = pre_parsed_name;
        } else {
            if self.peek()? == Token::Wait {
                self.next()?;

                if self.peek()? == Token::OpenParen {
                    self.next()?;
                    phantom_inputs = self.parse_list(
                        Token::ClosingParen,
                        Some(Token::Comma),
                        Self::expect_ident,
                    )?;
                } else {
                    phantom_inputs = vec![self.expect_ident()?];
                }
            }

            name = self.expect_ident()?;
        }

        self.expect(Token::OpenParen)?;
        let arguments = self.parse_list(
            Token::ClosingParen,
            Some(Token::Comma),
            Self::parse_expression,
        )?;

        Ok(ast::Node {
            name,
            arguments: arguments.into_boxed_slice(),
            phantom_inputs: phantom_inputs.into_boxed_slice(),
        })
    }

    /// Parse a list of items from the token stream.
    /// Will stop once hits `closing`.
    /// If provided will consume `seperator` between calls to `parser`.
    fn parse_list<T>(
        &mut self,
        closing: Token,
        seperator: Option<Token>,
        parser: impl Fn(&mut Self) -> Result<T, ParsingError>,
    ) -> Result<Vec<T>, ParsingError> {
        let mut result = Vec::new();
        while self.peek()? != closing {
            result.push(parser(self)?);

            if let Some(seperator) = seperator
                && self.peek()? == seperator
            {
                self.next()?;
            }
        }
        self.next()?;
        Ok(result)
    }

    /// If the next token is the given token return its span, otherwise return a error
    fn expect(&mut self, expected_token: Token) -> Result<Span, ParsingError> {
        let token = self.next()?;
        if *token == expected_token {
            Ok(token.span())
        } else {
            Err(ParsingError::UnexpectedToken {
                expected: expected_token.describe(),
                got: token.describe(),
                location: token.span(),
            })
        }
    }

    /// If the next token is a identifier return it, otherwiser return a error.
    fn expect_ident(&mut self) -> Result<ast::Ident<'src>, ParsingError> {
        let token = self.next()?;
        if let Token::Ident(ident) = *token {
            Ok(ast::Ident(token.span().with(ident)))
        } else {
            Err(ParsingError::UnexpectedToken {
                expected: "identifier".to_owned(),
                got: token.describe(),
                location: token.span(),
            })
        }
    }

    /// Return the next value in the token stream
    fn peek(&mut self) -> Result<Token<'src>, ParsingError> {
        match self.tokens.peek() {
            Some(token) => Ok(**token),
            None => Err(ParsingError::internal(
                "Hit end of token list before hitting EOF token.",
            )),
        }
    }

    /// Return the next value in the token stream and advance it.
    fn next(&mut self) -> Result<Spanned<Token<'src>>, ParsingError> {
        match self.tokens.next() {
            Some(token) => Ok(token),
            None => Err(ParsingError::internal(
                "Hit end of token list before hitting EOF token.",
            )),
        }
    }
}
