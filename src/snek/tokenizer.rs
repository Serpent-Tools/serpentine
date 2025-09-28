//! Tokenize the input string.

use crate::snek::ParsingError;
use crate::snek::span::{Span, Spanned};

/// a token is a small unit of the input stream.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Token<'src> {
    /// A identifier
    Ident(&'src str),
    /// A string,
    String(&'src str),
    /// A number
    Numeric(i128),
    /// `(`
    OpenParen,
    /// `)`
    ClosingParen,
    /// `;`
    SemiColon,
    /// `>`
    Pipe,
    /// `,`
    Comma,
    /// End of file
    Eof,
}

impl Token<'_> {
    /// Return a human friendly description of the token
    pub fn describe(&self) -> String {
        match self {
            Self::Ident(value) => format!("identifier ({value:?})"),
            Self::String(value) => format!("{value:?}"),
            Self::Numeric(value) => format!("{value:?}"),
            Self::OpenParen => "(".to_owned(),
            Self::ClosingParen => ")".to_owned(),
            Self::SemiColon => ";".to_owned(),
            Self::Pipe => ">".to_owned(),
            Self::Comma => ",".to_owned(),
            Self::Eof => "end of file".to_owned(),
        }
    }
}

/// the tokenizer handles turning a input stream into tokens
pub struct Tokenizer<'src> {
    /// The code to parse into tokens
    code: &'src str,
    /// Current byte we are on
    byte: usize,
}

impl<'src> Tokenizer<'src> {
    /// tokenize the given string and return the spanned tokens
    pub fn tokenize(code: &'src str) -> Result<Box<[Spanned<Token<'src>>]>, Vec<ParsingError>> {
        let mut tokenizer = Self { code, byte: 0 };

        let mut tokens = Vec::new();
        let mut errors = Vec::new();
        loop {
            match tokenizer.read_next_token() {
                Ok(None) => break,
                Ok(Some(token)) => tokens.push(token),
                Err(error) => errors.push(error),
            }
        }
        tokens.push(tokenizer.span(1).with(Token::Eof));

        if errors.is_empty() {
            Ok(tokens.into_boxed_slice())
        } else {
            Err(errors)
        }
    }

    /// read the next token from the input string.
    fn read_next_token(&mut self) -> Result<Option<Spanned<Token<'src>>>, super::ParsingError> {
        self.advance_while(char::is_whitespace)?;

        let Some(character) = self.advance()? else {
            return Ok(None);
        };

        Ok(Some(match character {
            '(' => self.span(1).with(Token::OpenParen),
            ')' => self.span(1).with(Token::ClosingParen),
            ';' => self.span(1).with(Token::SemiColon),
            '>' => self.span(1).with(Token::Pipe),
            ',' => self.span(1).with(Token::Comma),
            '"' => {
                let consumed = self.advance_while(|next_char| next_char != '"')?;
                let content_span = self.span(consumed);
                self.advance()?;
                let string_span = self.span(consumed.saturating_add(2));

                let content = content_span.index_str(self.code)?;
                string_span.with(Token::String(content))
            }
            character if character == '-' || character.is_numeric() => {
                let consumed = self.advance_while(char::is_numeric)?;
                let span = self.span(consumed.saturating_add(character.len_utf8()));
                let number = span.index_str(self.code)?;
                let number = number
                    .parse::<i128>()
                    .map_err(|err| ParsingError::internal(err.to_string()))?;
                span.with(Token::Numeric(number))
            }
            character if character.is_alphabetic() => {
                let consumed = self.advance_while(char::is_alphanumeric)?;

                let span = self.span(consumed.saturating_add(character.len_utf8()));
                let token = Token::Ident(span.index_str(self.code)?);
                span.with(token)
            }
            character => {
                return Err(super::ParsingError::UnknownCharacter {
                    location: self.span(1),
                    char: character,
                });
            }
        }))
    }

    /// Consume characthers that satifisy the predicate, returning the number of bytes consumed
    pub fn advance_while(
        &mut self,
        predicate: impl Fn(char) -> bool,
    ) -> Result<usize, super::ParsingError> {
        let mut consumed: usize = 0;
        while let Some(next_character) = self.peek()?
            && predicate(next_character)
        {
            self.advance()?;
            consumed = consumed.saturating_add(next_character.len_utf8());
        }

        Ok(consumed)
    }

    /// Peek at the next character in the code,
    /// returns None at eof
    fn peek(&self) -> Result<Option<char>, super::ParsingError> {
        match self.code.get(self.byte..) {
            Some(slice) => Ok(slice.chars().next()),
            None => Err(super::ParsingError::internal(
                "tokenizer byte offset didnt land on character boundary",
            )),
        }
    }

    /// Return the next character in the string and output `self.byte`
    fn advance(&mut self) -> Result<Option<char>, super::ParsingError> {
        let result = self.peek()?;
        if let Some(next_character) = result {
            self.byte = self.byte.saturating_add(next_character.len_utf8());
        }
        Ok(result)
    }

    /// Return a span ending at the current byte position, with the given length.
    fn span(&self, length: usize) -> Span {
        let end = self.byte;
        let start = end.saturating_sub(length);
        Span::new(start, end)
    }
}
