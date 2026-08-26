//! Tokenize the input string.

use std::borrow::Cow;

use crate::snek::CompileError;
use crate::snek::span::{FileId, Span, Spanned};

/// a token is a small unit of the input stream.
#[derive(Debug, PartialEq, Eq, Clone)]
#[cfg_attr(
    test,
    derive(strum::EnumDiscriminants),
    strum_discriminants(derive(strum::EnumIter))
)]
pub enum Token<'file> {
    /// A identifier
    Ident(&'file str),
    /// A string,
    // PERF: Look into using `Cow`, the tokenizing code just gets more tricky, but not hard.
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

impl Token<'_> {
    /// Return a human friendly description of the token
    pub fn describe(&self) -> Cow<'static, str> {
        match self {
            Self::Ident(value) => Cow::Owned(format!("identifier ({value:?})")),
            Self::String(value) => Cow::Owned(format!("{value:?}")),
            Self::Numeric(value) => Cow::Owned(format!("{value:?}")),
            Self::OpenParen => Cow::Borrowed("("),
            Self::ClosingParen => Cow::Borrowed(")"),
            Self::OpenBracket => Cow::Borrowed("{"),
            Self::ClosingBracket => Cow::Borrowed("}"),
            Self::SemiColon => Cow::Borrowed(";"),
            Self::Pipe => Cow::Borrowed(">"),
            Self::Comma => Cow::Borrowed(","),
            Self::Eq => Cow::Borrowed("="),
            Self::Wait => Cow::Borrowed("!"),
            Self::Path => Cow::Borrowed("::"),
            Self::Return => Cow::Borrowed("return"),
            Self::Def => Cow::Borrowed("def"),
            Self::Import => Cow::Borrowed("import"),
            Self::Export => Cow::Borrowed("export"),
            Self::As => Cow::Borrowed("as"),
            Self::Eof => Cow::Borrowed("end of file"),
        }
    }
}

/// the tokenizer handles turning a input stream into tokens
pub struct Tokenizer<'file> {
    /// File id for the file
    file_id: FileId,
    /// The code to parse into tokens
    code: &'file str,
    /// Current byte we are on
    byte: usize,
}

impl<'file> Tokenizer<'file> {
    /// tokenize the given string and return the spanned tokens
    pub fn tokenize(
        file_id: FileId,
        code: &'file str,
    ) -> Result<Box<[Spanned<Token<'file>>]>, CompileError> {
        let mut tokenizer = Self {
            file_id,
            code,
            byte: 0,
        };

        let mut tokens = Vec::new();
        while let Some(token) = tokenizer.read_next_token()? {
            tokens.push(token);
        }
        tokens.push(tokenizer.span(1).with(Token::Eof));

        Ok(tokens.into_boxed_slice())
    }

    /// read the next token from the input string.
    fn read_next_token(&mut self) -> Result<Option<Spanned<Token<'file>>>, CompileError> {
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
            '"' => self.handle_string()?,
            '/' if self.peek()? == Some('/') => {
                // consume until end of line
                self.advance()?;
                self.advance_while(|next_char| next_char != '\n')?;
                // read the next token
                return self.read_next_token();
            }
            '/' if self.peek()? == Some('*') => {
                // consume until closing */
                self.advance()?;
                loop {
                    let next_char = self.advance()?;
                    match next_char {
                        None => {
                            break;
                        }
                        Some('*') if self.peek()? == Some('/') => {
                            self.advance()?;
                            break;
                        }
                        _ => {}
                    }
                }
                // read the next token
                return self.read_next_token();
            }
            character if character.is_ascii_digit() => {
                let consumed = self.advance_while(|digit: char| digit.is_ascii_digit())?;
                let span = self.span(consumed.saturating_add(character.len_utf8()));
                let number = span.index_str(self.code)?;
                let number =
                    number
                        .parse::<i128>()
                        .map_err(|inner| CompileError::IntegerOverflow {
                            location: span,
                            inner,
                        })?;
                span.with(Token::Numeric(number))
            }
            character if character.is_alphabetic() => {
                let consumed =
                    self.advance_while(|ch| ch.is_alphanumeric() || ch == '-' || ch == '_')?;

                let span = self.span(consumed.saturating_add(character.len_utf8()));
                let text = span.index_str(self.code)?;
                let token = match text {
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
                    location: self.span(character.len_utf8()),
                    char: character,
                });
            }
        }))
    }

    /// Handle the tokenization of a string
    fn handle_string(&mut self) -> Result<Spanned<Token<'file>>, CompileError> {
        enum ParsingState {
            Normal,
            Escape,
        }

        let mut consumed = 1_usize; // initial "
        let mut content = String::new();
        let mut state = ParsingState::Normal;

        loop {
            if let Some(next_char) = self.advance()? {
                consumed = consumed.saturating_add(next_char.len_utf8());

                match state {
                    ParsingState::Normal => match next_char {
                        '"' => break,
                        '\\' => state = ParsingState::Escape,
                        _ => content.push(next_char),
                    },
                    ParsingState::Escape => {
                        let escaped_char = match next_char {
                            '\\' => '\\',
                            '"' => '"',
                            other => {
                                content.push('\\');
                                other
                            }
                        };
                        content.push(escaped_char);
                        state = ParsingState::Normal;
                    }
                }
            } else {
                return Err(CompileError::UnterminatedString {
                    location: self.span(consumed),
                });
            }
        }

        let string_span = self.span(consumed);
        Ok(string_span.with(Token::String(content.into())))
    }

    /// Consume characters that satisfy the predicate, returning the number of bytes consumed
    fn advance_while(&mut self, predicate: impl Fn(char) -> bool) -> Result<usize, CompileError> {
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

    /// Return the next character in the string and update `self.byte`
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
#[expect(clippy::expect_used, reason = "tests")]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::snek::span::FileId;

    #[test]
    fn doesnt_panic() {
        bolero::check!().with_type().for_each(|code: &String| {
            let _ = Tokenizer::tokenize(FileId(0), code);
        });
    }

    #[rstest]
    #[case::simple_number("123")]
    #[case::string(r#""hello""#)]
    fn tokenize(#[case] code: String) {
        let res = Tokenizer::tokenize(FileId(0), &code);
        assert!(res.is_ok(), "Failed to tokenize {code:?}: {res:?}");
    }

    #[rstest]
    #[case::simple(r#""hello""#, "hello")]
    #[case::backslash(r#""hello\\nworld""#, r"hello\nworld")]
    #[case::quote(r#""hello\"world""#, r#"hello"world"#)]
    #[case::quote_start(r#""\"hello""#, r#""hello"#)]
    #[case::quote_end(r#""hello\"""#, r#"hello""#)]
    #[case::unknown_escape(r#""\v""#, r"\v")]
    fn string_parsing(#[case] code: String, #[case] expected: String) {
        let res = Tokenizer::tokenize(FileId(0), &code).expect("Failed to tokenize");

        assert_eq!(res.len(), 2, "Expected 2 tokens, string, EOF");
        let string_token = res
            .into_iter()
            .next()
            .expect("Already checked we got 2 tokens");

        let token_dbg = format!("{string_token:?}");
        assert!(
            matches!(string_token.take(), Token::String(value) if *value == *expected),
            "Expected first token to be a string with value {expected:?}, got {token_dbg}",
        );
    }

    #[test]
    fn empty_comment() {
        let res = Tokenizer::tokenize(FileId(0), "/**/123").expect("Failed to tokenize");
        assert_eq!(res.len(), 2, "Expected 2 tokens, number, EOF");
    }

    #[rstest]
    #[case::unicode_digit("²")]
    #[case::unterminated_string(r#""hello"#)]
    #[case::single_colon(":")]
    #[case::double_colon_with_whitespace(": :")]
    #[case::overflow_digit("222222222222222222222222222222222222222")]
    fn edge_case_fails(#[case] code: String) {
        let res = Tokenizer::tokenize(FileId(0), &code);
        assert!(res.is_err(), "Should fail to tokenize {code:?}: {res:?}");
    }
}
