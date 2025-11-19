//! Spans represent a range in the code

use std::fmt::Debug;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use crate::snek::CompileError;

/// A virtual file that can hold multiple files for error reporting
/// But look like one big file to miette.
#[derive(Debug)]
pub struct VirtualFile {
    /// The actual files, with their content and paths.
    files: Vec<(PathBuf, String)>,
}

/// A unique identifier for a file in the virtual file system
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId(pub usize);

impl VirtualFile {
    /// Create a new empty virtual file
    pub fn new() -> Self {
        Self { files: Vec::new() }
    }

    /// Add a new file to this virtual file, returning its `FileId`.
    ///
    /// This will clone the `code` into the virtual file
    pub fn push(&mut self, path: PathBuf, code: &str) -> FileId {
        let mut current_end = 0;
        for (name, file) in &self.files {
            if *name == path {
                return FileId(current_end);
            }

            current_end = current_end.saturating_add(file.len());
        }

        self.files.push((path, code.to_owned()));

        FileId(current_end)
    }
}

impl miette::SourceCode for VirtualFile {
    fn read_span<'this>(
        &'this self,
        span: &miette::SourceSpan,
        context_lines_before: usize,
        context_lines_after: usize,
    ) -> Result<Box<dyn miette::SpanContents<'this> + 'this>, miette::MietteError> {
        let mut current_start = 0_usize;

        for (name, file) in &self.files {
            if span.offset() < current_start.saturating_add(file.len()) {
                let local_span = miette::SourceSpan::from((
                    span.offset().saturating_sub(current_start),
                    span.len(),
                ));

                let local_content =
                    file.read_span(&local_span, context_lines_before, context_lines_after)?;
                let local_content_span = local_content.span();
                let content_span = miette::SourceSpan::from((
                    local_content_span.offset().saturating_add(current_start),
                    local_content_span.len(),
                ));

                return Ok(Box::new(miette::MietteSpanContents::new_named(
                    name.display().to_string(),
                    local_content.data(),
                    content_span,
                    local_content.line(),
                    local_content.column(),
                    local_content.line_count(),
                )));
            }

            current_start = current_start.saturating_add(file.len());
        }

        Err(miette::MietteError::OutOfBounds)
    }
}

/// A span between `start` and `end`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// The id of the file this span is in
    pub file_id: FileId,
    /// Start of the span (inclusive)
    pub start: usize,
    /// End of the span (exclusive)
    pub end: usize,
}

impl Span {
    /// Create a new span
    pub fn new(file_id: FileId, start: usize, end: usize) -> Self {
        debug_assert!(start <= end, "start ({start}) bigger than end ({end})");
        Self {
            file_id,
            start,
            end,
        }
    }

    /// Return a dummy span for calling apis in the compiler with compiler generated values.
    /// That also take a span for error reporting.
    pub fn dummy() -> Self {
        Self {
            file_id: FileId(0),
            start: 0,
            end: 0,
        }
    }

    /// Join two spans
    pub fn join(self, other: Self) -> Self {
        debug_assert!(
            self.file_id == other.file_id,
            "Joined spans from different files"
        );

        Self {
            file_id: self.file_id,
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// Return a `Spanned` of the value and this span
    pub fn with<T>(self, value: T) -> Spanned<T> {
        Spanned(value, self)
    }

    /// return a clone of the string this span represents from the input string.
    pub fn index_str(self, value: &str) -> Result<Box<str>, CompileError> {
        value
            .get(self.start..self.end)
            .ok_or_else(|| {
                CompileError::internal(format!(
                    "Failed to index string {value:?} with {}..{}",
                    self.start, self.end
                ))
            })
            .map(Into::into)
    }
}

impl From<Span> for miette::SourceSpan {
    fn from(val: Span) -> Self {
        miette::SourceSpan::new(
            val.start.saturating_add(val.file_id.0).into(),
            val.end.saturating_sub(val.start),
        )
    }
}

/// Store `T` and a `Span`
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Spanned<T>(pub T, pub Span);

impl<T: Debug> Debug for Spanned<T> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_tuple("Spanned")
            .field(&self.0)
            .finish_non_exhaustive()
    }
}

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
