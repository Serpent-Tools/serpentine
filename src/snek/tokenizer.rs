//! Tokenize the input string.

use crate::snek::CompileError;
use crate::snek::span::{FileId, Span, Spanned};

/// a token is a small unit of the input stream.
#[derive(Debug, PartialEq, Eq, Clone)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub enum Token {
    /// A identifier
    Ident(Box<str>),
    /// A string,
    String(Box<str>),
    /// A number
    Numeric(i128),
    /// `(`
    OpenParen,
    /// `)`
    ClosingParen,
    /// `{`
    OpenBracket,
    /// `}`
    ClosingBracket,
    /// `;`
    SemiColon,
    /// `>`
    Pipe,
    /// `,`
    Comma,
    /// `=`
    Eq,
    /// `!`
    Wait,
    /// `::`
    Path,
    /// `return`
    Return,
    /// `def`
    Def,
    /// `import`
    Import,
    /// `export`
    Export,
    /// `as`
    As,
    /// End of file
    Eof,
}

impl Token {
    /// Return a human friendly description of the token
    pub fn describe(&self) -> String {
        match self {
            Self::Ident(value) => format!("identifier ({value:?})"),
            Self::String(value) => format!("{value:?}"),
            Self::Numeric(value) => format!("{value:?}"),
            Self::OpenParen => "(".to_owned(),
            Self::ClosingParen => ")".to_owned(),
            Self::OpenBracket => "{".to_owned(),
            Self::ClosingBracket => "}".to_owned(),
            Self::SemiColon => ";".to_owned(),
            Self::Pipe => ">".to_owned(),
            Self::Comma => ",".to_owned(),
            Self::Eq => "=".to_owned(),
            Self::Wait => "!".to_owned(),
            Self::Path => "::".to_owned(),
            Self::Return => "return".to_owned(),
            Self::Def => "def".to_owned(),
            Self::Import => "import".to_owned(),
            Self::Export => "export".to_owned(),
            Self::As => "as".to_owned(),
            Self::Eof => "end of file".to_owned(),
        }
    }
}

/// the tokenizer handles turning a input stream into tokens
pub struct Tokenizer<'src> {
    /// File id for the file
    file_id: FileId,
    /// The code to parse into tokens
    code: &'src str,
    /// Current byte we are on
    byte: usize,
}

impl<'src> Tokenizer<'src> {
    /// tokenize the given string and return the spanned tokens
    pub fn tokenize(
        file_id: FileId,
        code: &'src str,
    ) -> Result<Box<[Spanned<Token>]>, CompileError> {
        let mut tokenizer = Self {
            file_id,
            code,
            byte: 0,
        };

        let mut tokens = Vec::new();
        loop {
            match tokenizer.read_next_token()? {
                None => break,
                Some(token) => tokens.push(token),
            }
        }
        tokens.push(tokenizer.span(1).with(Token::Eof));

        Ok(tokens.into_boxed_slice())
    }

    /// read the next token from the input string.
    fn read_next_token(&mut self) -> Result<Option<Spanned<Token>>, CompileError> {
        self.advance_while(char::is_whitespace)?;

        let Some(character) = self.advance()? else {
            return Ok(None);
        };

        Ok(Some(match character {
            '(' => self.span(1).with(Token::OpenParen),
            ')' => self.span(1).with(Token::ClosingParen),
            '{' => self.span(1).with(Token::OpenBracket),
            '}' => self.span(1).with(Token::ClosingBracket),
            ';' => self.span(1).with(Token::SemiColon),
            '>' => self.span(1).with(Token::Pipe),
            ',' => self.span(1).with(Token::Comma),
            '=' => self.span(1).with(Token::Eq),
            '!' => self.span(1).with(Token::Wait),
            ':' if self.peek()? == Some(':') => {
                self.advance()?;
                self.span(2).with(Token::Path)
            }
            '"' => {
                let consumed = self.advance_while(|next_char| next_char != '"')?;
                let content_span = self.span(consumed);
                if self.advance()?.is_none() {
                    return Err(CompileError::UnterminatedString {
                        location: content_span,
                    });
                }
                let string_span = self.span(consumed.saturating_add(2));

                let content = content_span.index_str(self.code)?;
                string_span.with(Token::String(content))
            }
            character if character.is_ascii_digit() => {
                let consumed = self.advance_while(|digit: char| digit.is_ascii_digit())?;
                let span = self.span(consumed.saturating_add(character.len_utf8()));
                let number = span.index_str(self.code)?;
                let number = number
                    .parse::<i128>()
                    .map_err(|err| CompileError::internal(err.to_string()))?;
                span.with(Token::Numeric(number))
            }
            character if character.is_alphabetic() => {
                let consumed = self.advance_while(|characther| {
                    characther.is_alphanumeric() || characther == '-' || characther == '_'
                })?;

                let span = self.span(consumed.saturating_add(character.len_utf8()));
                let text = span.index_str(self.code)?;
                let token = match &*text {
                    "return" => Token::Return,
                    "def" => Token::Def,
                    "import" => Token::Import,
                    "export" => Token::Export,
                    "as" => Token::As,
                    _ => Token::Ident(text),
                };
                span.with(token)
            }
            character => {
                return Err(super::CompileError::UnknownCharacter {
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
    ) -> Result<usize, CompileError> {
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
    fn peek(&self) -> Result<Option<char>, CompileError> {
        match self.code.get(self.byte..) {
            Some(slice) => Ok(slice.chars().next()),
            None => Err(CompileError::internal(
                "tokenizer byte offset didnt land on character boundary",
            )),
        }
    }

    /// Return the next character in the string and output `self.byte`
    fn advance(&mut self) -> Result<Option<char>, CompileError> {
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
        Span::new(self.file_id, start, end)
    }
}

#[cfg(test)]
mod tests {
    use proptest::property_test;
    use rstest::rstest;

    use super::*;
    use crate::snek::span::FileId;

    #[property_test]
    fn doesnt_panic(code: String) {
        let _ = Tokenizer::tokenize(FileId(0), &code);
    }

    #[rstest]
    #[case::simple_numeber("123")]
    #[case::string(r#""hello""#)]
    fn tokenize(#[case] code: String) {
        let res = Tokenizer::tokenize(FileId(0), &code);
        assert!(res.is_ok(), "Failed to tokenize {code:?}: {res:?}");
    }

    #[rstest]
    #[case::unicode_digit("²")]
    #[case::unterminated_string(r#""hello"#)]
    #[case::single_colon(":")]
    #[case::double_colon_with_whitespace(": :")]
    fn edge_case_fails(#[case] code: String) {
        let res = Tokenizer::tokenize(FileId(0), &code);
        assert!(res.is_err(), "Should fail to tokenize {code:?}: {res:?}");
    }
}
