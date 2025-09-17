//! Spans represent a range in the code

use std::ops::{Deref, DerefMut};

/// A span between `start` and `end`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Start of the span (inclusive)
    pub start: usize,
    /// End of the span (exclusive)
    pub end: usize,
}

impl Span {
    /// Create a new span
    pub fn new(start: usize, end: usize) -> Self {
        debug_assert!(start <= end, "start ({start}) bigger than end ({end})");
        Self { start, end }
    }

    /// Join two spans
    pub fn join(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// Return a `Spanned` of the value and this span
    pub fn with<T>(self, value: T) -> Spanned<T> {
        Spanned(value, self)
    }

    /// return the string slice this span represents from the input string.
    pub fn index_str(self, value: &str) -> Result<&str, super::ParsingError> {
        value.get(self.start..self.end).ok_or_else(|| {
            super::ParsingError::internal(format!(
                "Failed to index string {value:?} with {}..{}",
                self.start, self.end
            ))
        })
    }
}

impl From<Span> for miette::SourceSpan {
    fn from(val: Span) -> Self {
        miette::SourceSpan::new(val.start.into(), val.end.saturating_sub(val.start))
    }
}

/// Store `T` and a `Span`
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Spanned<T>(pub T, pub Span);

impl<T> Deref for Spanned<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<T> DerefMut for Spanned<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> Spanned<T> {
    /// Return the span of the data
    pub fn span(&self) -> Span {
        self.1
    }

    /// Return the owned data
    pub fn take(self) -> T {
        self.0
    }
}
