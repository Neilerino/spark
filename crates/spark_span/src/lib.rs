//! # `spark_span`
//!
//! Source location tracking for the Spark compiler.
//!
//! This crate provides types for tracking where things are in source code:
//! - [`FileId`] — identifier for a source file
//! - [`Span`] — a range in source code
//! - [`Spanned`] — wrapper attaching a span to any value
//! - [`SourceMap`] — registry of source files
//!
//! See the crate README for detailed documentation.

#![forbid(unsafe_code)]
// We intentionally use u32 for byte offsets and file IDs to keep spans compact.
// Files larger than 4GB are not supported.
#![allow(clippy::cast_possible_truncation)]

use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

// ============================================================================
// FileId
// ============================================================================

/// A unique identifier for a source file in the [`SourceMap`].
///
/// File IDs are handed out sequentially starting from 0 when files are added
/// to the source map. They are cheap to copy and compare.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct FileId(u32);

impl FileId {
    /// Creates a new `FileId` from a raw index.
    ///
    /// This is primarily for internal use. Prefer using [`SourceMap::add_file`].
    #[must_use]
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    /// Returns the raw index of this file ID.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }

    /// A dummy file ID for use in tests or synthetic spans.
    pub const DUMMY: Self = Self(u32::MAX);
}

impl fmt::Debug for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if *self == Self::DUMMY {
            write!(f, "FileId(DUMMY)")
        } else {
            write!(f, "FileId({})", self.0)
        }
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ============================================================================
// Span
// ============================================================================

/// A contiguous region in source code.
///
/// A span represents a range of bytes in a specific source file. It is used
/// throughout the compiler to track the origin of tokens, AST nodes, and
/// diagnostics.
///
/// # Layout
///
/// Spans are designed to be small (12 bytes) so they can be embedded cheaply
/// in AST nodes. They use byte offsets rather than line/column because:
/// - Byte offsets are what the lexer naturally produces
/// - They're cheaper to store and compare
/// - Line/column can be computed on-demand from the [`SourceMap`]
///
/// # Combining Spans
///
/// Use [`Span::merge`] to combine two spans into one that covers both:
///
/// ```
/// # use spark_span::{FileId, Span};
/// let file = FileId::new(0);
/// let left = Span::new(file, 0, 5);
/// let right = Span::new(file, 10, 15);
/// let combined = left.merge(right);
/// assert_eq!(combined.start, 0);
/// assert_eq!(combined.end, 15);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Span {
    /// The source file this span belongs to.
    pub file: FileId,
    /// The start byte offset (inclusive).
    pub start: u32,
    /// The end byte offset (exclusive).
    pub end: u32,
}

impl Span {
    /// Creates a new span.
    ///
    /// # Panics
    ///
    /// Panics in debug mode if `start > end`.
    #[must_use]
    pub const fn new(file: FileId, start: u32, end: u32) -> Self {
        debug_assert!(start <= end, "span start must be <= end");
        Self { file, start, end }
    }

    /// Creates a zero-width span at the given position.
    ///
    /// Useful for representing positions (like cursor locations) rather than
    /// ranges.
    #[must_use]
    pub const fn point(file: FileId, pos: u32) -> Self {
        Self {
            file,
            start: pos,
            end: pos,
        }
    }

    /// A dummy span for use in tests or compiler-generated code.
    pub const DUMMY: Self = Self {
        file: FileId::DUMMY,
        start: 0,
        end: 0,
    };

    /// Returns the length of this span in bytes.
    #[must_use]
    pub const fn len(self) -> u32 {
        self.end - self.start
    }

    /// Returns true if this span has zero length.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// Returns true if this is a dummy span.
    #[must_use]
    pub const fn is_dummy(self) -> bool {
        self.file.0 == FileId::DUMMY.0
    }

    /// Returns this span as a byte range.
    #[must_use]
    pub const fn range(self) -> Range<usize> {
        self.start as usize..self.end as usize
    }

    /// Merges two spans into one that covers both.
    ///
    /// The resulting span starts at the minimum start position and ends at
    /// the maximum end position.
    ///
    /// # Panics
    ///
    /// Panics if the spans are from different files.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        assert_eq!(
            self.file, other.file,
            "cannot merge spans from different files"
        );
        Self {
            file: self.file,
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// Returns a new span that starts at this span's start and ends at the
    /// other span's end.
    ///
    /// Useful for creating a span that covers "from here to there".
    ///
    /// # Panics
    ///
    /// Panics if the spans are from different files.
    #[must_use]
    pub fn to(self, other: Self) -> Self {
        assert_eq!(
            self.file, other.file,
            "cannot create span across different files"
        );
        Self {
            file: self.file,
            start: self.start,
            end: other.end,
        }
    }

    /// Returns true if this span contains the given byte offset.
    #[must_use]
    pub const fn contains(self, pos: u32) -> bool {
        pos >= self.start && pos < self.end
    }

    /// Returns true if this span fully contains another span.
    #[must_use]
    pub const fn contains_span(self, other: Self) -> bool {
        self.file.0 == other.file.0 && self.start <= other.start && other.end <= self.end
    }

    /// Shrinks the span by the given amount from both ends.
    ///
    /// Returns `None` if the shrink amount is larger than the span.
    #[must_use]
    pub const fn shrink(self, amount: u32) -> Option<Self> {
        if self.len() >= amount * 2 {
            Some(Self {
                file: self.file,
                start: self.start + amount,
                end: self.end - amount,
            })
        } else {
            None
        }
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_dummy() {
            write!(f, "Span(DUMMY)")
        } else {
            write!(f, "Span({}:{}..{})", self.file, self.start, self.end)
        }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

// ============================================================================
// Spanned
// ============================================================================

/// A value with an associated source span.
///
/// This is a convenience wrapper for attaching location information to values.
/// It's commonly used in the AST and for tokens.
///
/// # Example
///
/// ```
/// # use spark_span::{Span, Spanned, FileId};
/// let span = Span::new(FileId::new(0), 0, 5);
/// let spanned_value = Spanned::new(42, span);
///
/// assert_eq!(*spanned_value, 42);
/// assert_eq!(spanned_value.span, span);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Spanned<T> {
    /// The wrapped value.
    pub value: T,
    /// The source span for this value.
    pub span: Span,
}

impl<T> Spanned<T> {
    /// Creates a new spanned value.
    #[must_use]
    pub const fn new(value: T, span: Span) -> Self {
        Self { value, span }
    }

    /// Creates a spanned value with a dummy span.
    ///
    /// Useful for tests or compiler-generated values.
    #[must_use]
    pub const fn dummy(value: T) -> Self {
        Self {
            value,
            span: Span::DUMMY,
        }
    }

    /// Maps the inner value while preserving the span.
    #[must_use]
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> Spanned<U> {
        Spanned {
            value: f(self.value),
            span: self.span,
        }
    }

    /// Returns a reference to the inner value.
    #[must_use]
    pub const fn inner(&self) -> &T {
        &self.value
    }

    /// Consumes the wrapper and returns the inner value.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.value
    }
}

impl<T> std::ops::Deref for Spanned<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> std::ops::DerefMut for Spanned<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

impl<T: fmt::Debug> fmt::Debug for Spanned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} @ {:?}", self.value, self.span)
    }
}

impl<T: fmt::Display> fmt::Display for Spanned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.value)
    }
}

// ============================================================================
// SourceFile
// ============================================================================

/// Information about a single source file.
#[derive(Debug, Clone)]
pub struct SourceFile {
    /// The file's unique ID.
    id: FileId,
    /// The file path (may be synthetic like "<stdin>").
    path: PathBuf,
    /// The source code contents.
    source: String,
    /// Byte offsets of each line start, computed lazily.
    line_starts: Vec<u32>,
}

impl SourceFile {
    /// Creates a new source file.
    fn new(id: FileId, path: PathBuf, source: String) -> Self {
        // Pre-compute line starts for efficient line/column lookup
        let line_starts = std::iter::once(0)
            .chain(source.match_indices('\n').map(|(i, _)| (i + 1) as u32))
            .collect();

        Self {
            id,
            path,
            source,
            line_starts,
        }
    }

    /// Returns the file's unique ID.
    #[must_use]
    pub const fn id(&self) -> FileId {
        self.id
    }

    /// Returns the file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the file name (last component of path).
    #[must_use]
    pub fn name(&self) -> &str {
        self.path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("<unknown>")
    }

    /// Returns the full source code.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Returns the source code for a byte range.
    ///
    /// Returns `None` if the range is out of bounds.
    #[must_use]
    pub fn source_slice(&self, range: Range<usize>) -> Option<&str> {
        self.source.get(range)
    }

    /// Returns the number of lines in the file.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // Vec::len() is not const
    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Converts a byte offset to a line and column (both 1-indexed).
    ///
    /// Returns `None` if the offset is out of bounds.
    #[must_use]
    pub fn line_col(&self, offset: u32) -> Option<(u32, u32)> {
        if offset as usize > self.source.len() {
            return None;
        }

        // Binary search for the line
        let line_index = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };

        let line_start = self.line_starts[line_index];
        let column = offset - line_start;

        // Convert to 1-indexed
        Some((line_index as u32 + 1, column + 1))
    }

    /// Returns the byte offset of the start of a line (1-indexed).
    ///
    /// Returns `None` if the line number is out of bounds.
    #[must_use]
    pub fn line_start(&self, line: u32) -> Option<u32> {
        if line == 0 {
            return None;
        }
        self.line_starts.get(line as usize - 1).copied()
    }

    /// Returns the source text of a specific line (1-indexed), without the
    /// trailing newline.
    ///
    /// Returns `None` if the line number is out of bounds.
    #[must_use]
    pub fn line_source(&self, line: u32) -> Option<&str> {
        if line == 0 {
            return None;
        }

        let line_index = line as usize - 1;
        let start = *self.line_starts.get(line_index)? as usize;

        let end = self
            .line_starts
            .get(line_index + 1)
            .map_or(self.source.len(), |&e| (e as usize).saturating_sub(1));

        self.source.get(start..end)
    }
}

// ============================================================================
// SourceMap
// ============================================================================

/// A registry of all source files in a compilation.
///
/// The `SourceMap` stores source files and provides efficient lookup of:
/// - Source text for a span
/// - Line and column numbers for error reporting
/// - File paths for diagnostics
///
/// # Example
///
/// ```
/// use spark_span::SourceMap;
///
/// let mut sm = SourceMap::new();
/// let file_id = sm.add_file("test.spark", "fn main(): Void { }");
///
/// // Get line/column for offset 3 ("main")
/// let (line, col) = sm.line_col(file_id, 3).unwrap();
/// assert_eq!((line, col), (1, 4));
/// ```
#[derive(Debug, Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    /// Creates an empty source map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a source file to the map.
    ///
    /// Returns the file's unique ID.
    pub fn add_file(&mut self, path: impl AsRef<Path>, source: impl Into<String>) -> FileId {
        let id = FileId::new(self.files.len() as u32);
        let file = SourceFile::new(id, path.as_ref().to_path_buf(), source.into());
        self.files.push(file);
        id
    }

    /// Returns the source file for a given ID.
    ///
    /// Returns `None` if the ID is invalid.
    #[must_use]
    pub fn get_file(&self, id: FileId) -> Option<&SourceFile> {
        if id == FileId::DUMMY {
            return None;
        }
        self.files.get(id.index() as usize)
    }

    /// Returns the source text for a span.
    ///
    /// Returns `None` if the span refers to an invalid file or is out of
    /// bounds.
    #[must_use]
    pub fn source(&self, span: Span) -> Option<&str> {
        self.get_file(span.file)?.source_slice(span.range())
    }

    /// Returns the source text for a span, or `"<unavailable>"` if not found.
    #[must_use]
    pub fn source_or_unavailable(&self, span: Span) -> &str {
        self.source(span).unwrap_or("<unavailable>")
    }

    /// Converts a byte offset to line and column (both 1-indexed).
    ///
    /// Returns `None` if the file ID is invalid or offset is out of bounds.
    #[must_use]
    pub fn line_col(&self, file: FileId, offset: u32) -> Option<(u32, u32)> {
        self.get_file(file)?.line_col(offset)
    }

    /// Returns the path of a file.
    ///
    /// Returns `None` if the file ID is invalid.
    #[must_use]
    pub fn file_path(&self, id: FileId) -> Option<&Path> {
        self.get_file(id).map(SourceFile::path)
    }

    /// Returns the name (filename component) of a file.
    ///
    /// Returns `"<unknown>"` if the file ID is invalid.
    #[must_use]
    pub fn file_name(&self, id: FileId) -> &str {
        self.get_file(id)
            .map_or("<unknown>", SourceFile::name)
    }

    /// Returns an iterator over all source files.
    pub fn files(&self) -> impl Iterator<Item = &SourceFile> {
        self.files.iter()
    }

    /// Returns the number of files in the source map.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // Vec::len() is not const
    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_id_basics() {
        let id = FileId::new(42);
        assert_eq!(id.index(), 42);
        assert_ne!(id, FileId::DUMMY);
    }

    #[test]
    fn span_basics() {
        let file = FileId::new(0);
        let span = Span::new(file, 10, 20);

        assert_eq!(span.len(), 10);
        assert!(!span.is_empty());
        assert!(!span.is_dummy());
        assert!(span.contains(15));
        assert!(!span.contains(5));
        assert!(!span.contains(20)); // end is exclusive
    }

    #[test]
    fn span_merge() {
        let file = FileId::new(0);
        let a = Span::new(file, 5, 10);
        let b = Span::new(file, 15, 20);

        let merged = a.merge(b);
        assert_eq!(merged.start, 5);
        assert_eq!(merged.end, 20);
    }

    #[test]
    fn span_to() {
        let file = FileId::new(0);
        let a = Span::new(file, 5, 10);
        let b = Span::new(file, 15, 20);

        let result = a.to(b);
        assert_eq!(result.start, 5);
        assert_eq!(result.end, 20);
    }

    #[test]
    fn spanned_basics() {
        let span = Span::new(FileId::new(0), 0, 5);
        let spanned = Spanned::new(42, span);

        assert_eq!(*spanned, 42);
        assert_eq!(spanned.span, span);
        assert_eq!(spanned.into_inner(), 42);
    }

    #[test]
    fn spanned_map() {
        let span = Span::new(FileId::new(0), 0, 5);
        let spanned = Spanned::new(42, span);
        let doubled = spanned.map(|x| x * 2);

        assert_eq!(*doubled, 84);
        assert_eq!(doubled.span, span);
    }

    #[test]
    fn source_map_add_file() {
        let mut sm = SourceMap::new();
        let id = sm.add_file("test.spark", "hello world");

        assert_eq!(id.index(), 0);
        assert_eq!(sm.file_count(), 1);
        assert_eq!(sm.file_name(id), "test.spark");
    }

    #[test]
    fn source_map_source_lookup() {
        let mut sm = SourceMap::new();
        let id = sm.add_file("test.spark", "hello world");
        let span = Span::new(id, 0, 5);

        assert_eq!(sm.source(span), Some("hello"));
    }

    #[test]
    fn source_map_line_col() {
        let mut sm = SourceMap::new();
        let source = "line one\nline two\nline three";
        let id = sm.add_file("test.spark", source);

        // First line
        assert_eq!(sm.line_col(id, 0), Some((1, 1)));
        assert_eq!(sm.line_col(id, 4), Some((1, 5)));

        // Second line (after first newline at byte 8)
        assert_eq!(sm.line_col(id, 9), Some((2, 1)));
        assert_eq!(sm.line_col(id, 13), Some((2, 5)));

        // Third line
        assert_eq!(sm.line_col(id, 18), Some((3, 1)));
    }

    #[test]
    fn source_file_line_source() {
        let mut sm = SourceMap::new();
        let source = "line one\nline two\nline three";
        let id = sm.add_file("test.spark", source);

        let file = sm.get_file(id).unwrap();
        assert_eq!(file.line_source(1), Some("line one"));
        assert_eq!(file.line_source(2), Some("line two"));
        assert_eq!(file.line_source(3), Some("line three"));
        assert_eq!(file.line_source(4), None);
        assert_eq!(file.line_source(0), None);
    }

    #[test]
    fn source_file_line_count() {
        let mut sm = SourceMap::new();
        let id = sm.add_file("test.spark", "a\nb\nc");
        let file = sm.get_file(id).unwrap();

        assert_eq!(file.line_count(), 3);
    }

    #[test]
    fn dummy_span() {
        assert!(Span::DUMMY.is_dummy());
        assert_eq!(Span::DUMMY.file, FileId::DUMMY);
    }

    #[test]
    fn span_shrink() {
        let file = FileId::new(0);
        let span = Span::new(file, 0, 10);

        let shrunk = span.shrink(2).unwrap();
        assert_eq!(shrunk.start, 2);
        assert_eq!(shrunk.end, 8);

        // Can't shrink more than half the span
        assert!(span.shrink(6).is_none());
    }
}
