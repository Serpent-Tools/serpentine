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
    ) -> Result<ast::File<'src>, Vec<ParsingError>> {
        let mut parser = Self {
            tokens: tokens.into_iter().peekable(),
        };

        let mut statements = Vec::new();
        let mut errors = Vec::new();

        while parser
            .tokens
            .peek()
            .is_some_and(|token| !matches!(**token, Token::Eof))
        {
            match parser.parse_statement() {
                Ok(stmt) => statements.push(stmt),
                Err(parsing_error) => {
                    errors.push(parsing_error);
                    if let Err(err) = parser.error_recovery() {
                        errors.push(err);
                    }
                }
            }
        }

        if errors.is_empty() {
            Ok(ast::File(statements.into_boxed_slice()))
        } else {
            Err(errors)
        }
    }

    /// Eat tokens until we consume a Eof or Semicolon.
    fn error_recovery(&mut self) -> Result<(), ParsingError> {
        if self.tokens.peek().is_none() {
            return Ok(());
        }

        loop {
            match self.peek()? {
                Token::Eof => break,
                Token::SemiColon => {
                    self.next()?;
                    break;
                }
                _ => {
                    self.next()?;
                }
            }
        }
        Ok(())
    }

    /// Parse a statement from the current token stream.
    fn parse_statement(&mut self) -> Result<ast::Statement<'src>, ParsingError> {
        let expression = self.parse_expression()?;
        self.expect(Token::SemiColon)?;
        Ok(ast::Statement::Expression(expression))
    }

    /// Parse a expression
    fn parse_expression(&mut self) -> Result<ast::Expression<'src>, ParsingError> {
        let node = ast::Expression::Node(self.parse_node()?);

        if self.peek()? == Token::Pipe {
            Ok(ast::Expression::Chain(self.parse_chain(node)?))
        } else {
            Ok(node)
        }
    }

    /// Parse a chain of nodes, `expression > Node > Node = ident`
    fn parse_chain(
        &mut self,
        start: ast::Expression<'src>,
    ) -> Result<ast::Chain<'src>, ParsingError> {
        let mut nodes = Vec::new();
        while self.peek()? == Token::Pipe {
            self.next()?;
            nodes.push(self.parse_node()?);
        }

        Ok(ast::Chain {
            start: Box::new(start),
            nodes: nodes.into_boxed_slice(),
        })
    }

    /// Parse a node, `Ident()`
    fn parse_node(&mut self) -> Result<ast::Node<'src>, ParsingError> {
        let name = self.expect_ident()?;
        self.expect(Token::OpenParen)?;
        self.expect(Token::ClosingParen)?;
        Ok(ast::Node { name })
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
