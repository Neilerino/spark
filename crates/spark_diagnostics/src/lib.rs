//! # `spark_diagnostics`
//!
//! Error reporting and diagnostics for the Spark compiler.
//!
//! This crate provides structured diagnostics with source locations,
//! suggestions, and beautiful terminal rendering.
//!
//! # Quick Start
//!
//! ```
//! use spark_diagnostics::{Diagnostic, Label};
//! use spark_span::{Span, FileId};
//!
//! let span = Span::new(FileId::new(0), 10, 15);
//!
//! let diag = Diagnostic::error("undefined variable `foo`")
//!     .with_code("E0425")
//!     .with_label(Label::primary(span, "not found in this scope"));
//! ```
//!
//! See the crate README for detailed documentation.

#![forbid(unsafe_code)]
#![allow(clippy::cast_possible_truncation)]

use std::fmt;
use std::io::{self, Write};

use ariadne::{Color, Config, IndexType, LabelAttach, Report, ReportKind};
use spark_span::{FileId, SourceMap, Span};

// ============================================================================
// Level
// ============================================================================

/// The severity level of a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Level {
    /// An error that prevents compilation.
    Error,
    /// A warning about suspicious code.
    Warning,
    /// Additional context for another diagnostic.
    Note,
    /// A suggestion for fixing an issue.
    Help,
}

impl Level {
    /// Returns the ariadne report kind for this level.
    #[allow(clippy::missing_const_for_fn)] // ReportKind is not Copy
    fn to_report_kind(self) -> ReportKind<'static> {
        match self {
            Self::Error => ReportKind::Error,
            Self::Warning => ReportKind::Warning,
            Self::Note | Self::Help => ReportKind::Advice,
        }
    }

    /// Returns the color for this level.
    #[allow(clippy::missing_const_for_fn)] // Color is not Copy
    fn color(self) -> Color {
        match self {
            Self::Error => Color::Red,
            Self::Warning => Color::Yellow,
            Self::Note | Self::Help => Color::Cyan,
        }
    }

    /// Returns true if this is an error level.
    #[must_use]
    pub const fn is_error(self) -> bool {
        matches!(self, Self::Error)
    }

    /// Returns true if this is a warning level.
    #[must_use]
    pub const fn is_warning(self) -> bool {
        matches!(self, Self::Warning)
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error => write!(f, "error"),
            Self::Warning => write!(f, "warning"),
            Self::Note => write!(f, "note"),
            Self::Help => write!(f, "help"),
        }
    }
}

// ============================================================================
// DiagnosticCode
// ============================================================================

/// An error code for documentation lookup (e.g., E0308).
///
/// Error codes help users find documentation about specific errors
/// and make errors recognizable across compiler versions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DiagnosticCode(String);

impl DiagnosticCode {
    /// Creates a new diagnostic code.
    #[must_use]
    pub fn new(code: impl Into<String>) -> Self {
        Self(code.into())
    }

    /// Returns the code as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DiagnosticCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<&str> for DiagnosticCode {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

// ============================================================================
// Label
// ============================================================================

/// A labeled source span within a diagnostic.
///
/// Labels point to specific locations in source code and provide
/// context about what's wrong or relevant at that location.
#[derive(Debug, Clone)]
pub struct Label {
    /// The source span this label points to.
    pub span: Span,
    /// The message to display with this label.
    pub message: String,
    /// Whether this is the primary label (the main error location).
    pub is_primary: bool,
}

impl Label {
    /// Creates a primary label (the main error location).
    ///
    /// Primary labels are highlighted more prominently and typically
    /// point to where the error occurred.
    #[must_use]
    pub fn primary(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
            is_primary: true,
        }
    }

    /// Creates a secondary label (additional context).
    ///
    /// Secondary labels provide context about why an error occurred,
    /// such as "expected type defined here" or "previous definition".
    #[must_use]
    pub fn secondary(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
            is_primary: false,
        }
    }

    /// Returns the color for this label.
    fn color(&self, level: Level) -> Color {
        if self.is_primary {
            level.color()
        } else {
            Color::Blue
        }
    }
}

// ============================================================================
// Suggestion
// ============================================================================

/// A suggested code fix.
///
/// Suggestions can be applied automatically by tools to fix issues.
#[derive(Debug, Clone)]
pub struct Suggestion {
    /// A description of what this suggestion does.
    pub message: String,
    /// The span to replace.
    pub span: Span,
    /// The replacement text.
    pub replacement: String,
}

impl Suggestion {
    /// Creates a new suggestion.
    #[must_use]
    pub fn new(message: impl Into<String>, span: Span, replacement: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            span,
            replacement: replacement.into(),
        }
    }

    /// Creates a suggestion to insert text at a position.
    #[must_use]
    pub fn insert(
        message: impl Into<String>,
        file: FileId,
        pos: u32,
        text: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            span: Span::point(file, pos),
            replacement: text.into(),
        }
    }

    /// Creates a suggestion to delete a span.
    #[must_use]
    pub fn delete(message: impl Into<String>, span: Span) -> Self {
        Self {
            message: message.into(),
            span,
            replacement: String::new(),
        }
    }
}

// ============================================================================
// Diagnostic
// ============================================================================

/// A compiler diagnostic (error, warning, or note).
///
/// Diagnostics are the primary way the compiler communicates issues
/// to the user. They include:
/// - A severity level (error, warning, note, help)
/// - A message describing the issue
/// - Optional source locations (labels)
/// - Optional suggestions for fixes
/// - Optional notes with additional context
///
/// # Example
///
/// ```
/// use spark_diagnostics::{Diagnostic, Label};
/// use spark_span::{Span, FileId};
///
/// let span = Span::new(FileId::new(0), 0, 5);
///
/// let diag = Diagnostic::error("type mismatch")
///     .with_code("E0308")
///     .with_label(Label::primary(span, "expected `Int`, found `String`"))
///     .with_note("all function arguments must match their declared types");
/// ```
#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// The severity level.
    pub level: Level,
    /// The main error message.
    pub message: String,
    /// Optional error code for documentation.
    pub code: Option<DiagnosticCode>,
    /// Source labels pointing to relevant code.
    pub labels: Vec<Label>,
    /// Additional notes.
    pub notes: Vec<String>,
    /// Suggested fixes.
    pub suggestions: Vec<Suggestion>,
}

impl Diagnostic {
    /// Creates a new diagnostic with the given level and message.
    #[must_use]
    pub fn new(level: Level, message: impl Into<String>) -> Self {
        Self {
            level,
            message: message.into(),
            code: None,
            labels: Vec::new(),
            notes: Vec::new(),
            suggestions: Vec::new(),
        }
    }

    /// Creates an error diagnostic.
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self::new(Level::Error, message)
    }

    /// Creates a warning diagnostic.
    #[must_use]
    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(Level::Warning, message)
    }

    /// Creates a note diagnostic.
    #[must_use]
    pub fn note(message: impl Into<String>) -> Self {
        Self::new(Level::Note, message)
    }

    /// Creates a help diagnostic.
    #[must_use]
    pub fn help(message: impl Into<String>) -> Self {
        Self::new(Level::Help, message)
    }

    /// Adds an error code.
    #[must_use]
    pub fn with_code(mut self, code: impl Into<DiagnosticCode>) -> Self {
        self.code = Some(code.into());
        self
    }

    /// Adds a label.
    #[must_use]
    pub fn with_label(mut self, label: Label) -> Self {
        self.labels.push(label);
        self
    }

    /// Adds multiple labels.
    #[must_use]
    pub fn with_labels(mut self, labels: impl IntoIterator<Item = Label>) -> Self {
        self.labels.extend(labels);
        self
    }

    /// Adds a note.
    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    /// Adds a help message.
    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.notes.push(format!("help: {}", help.into()));
        self
    }

    /// Adds a suggestion.
    #[must_use]
    pub fn with_suggestion(mut self, suggestion: Suggestion) -> Self {
        self.suggestions.push(suggestion);
        self
    }

    /// Returns true if this is an error.
    #[must_use]
    pub const fn is_error(&self) -> bool {
        self.level.is_error()
    }

    /// Returns true if this is a warning.
    #[must_use]
    pub const fn is_warning(&self) -> bool {
        self.level.is_warning()
    }

    /// Returns the primary span, if any.
    #[must_use]
    pub fn primary_span(&self) -> Option<Span> {
        self.labels.iter().find(|l| l.is_primary).map(|l| l.span)
    }

    /// Renders this diagnostic to stderr.
    pub fn emit(&self, source_map: &SourceMap) {
        let _ = self.write(source_map, &mut io::stderr().lock());
    }

    /// Renders this diagnostic to a writer.
    ///
    /// # Errors
    ///
    /// Returns an error if writing to the writer fails.
    pub fn write(&self, source_map: &SourceMap, writer: &mut impl Write) -> io::Result<()> {
        // Find the primary file for this diagnostic
        let primary_file = self
            .labels
            .iter()
            .find(|l| l.is_primary)
            .map(|l| l.span.file)
            .or_else(|| self.labels.first().map(|l| l.span.file));

        let Some(file_id) = primary_file else {
            // No labels, just print the message
            writeln!(writer, "{}: {}", self.level, self.message)?;
            for note in &self.notes {
                writeln!(writer, "  = {note}")?;
            }
            return Ok(());
        };

        // Build the ariadne report
        // Span type is (String, Range<usize>) - file name + byte range
        let file_name: String = source_map.file_name(file_id).to_string();

        let offset = self
            .labels
            .iter()
            .find(|l| l.is_primary)
            .map_or(0, |l| l.span.start as usize);

        let mut report = Report::<(String, std::ops::Range<usize>)>::build(
            self.level.to_report_kind(),
            file_name,
            offset,
        )
        .with_config(
            Config::default()
                .with_index_type(IndexType::Byte)
                .with_label_attach(LabelAttach::Start),
        );

        // Set message with optional code
        let message = self.code.as_ref().map_or_else(
            || self.message.clone(),
            |code| format!("[{code}] {}", self.message),
        );
        report = report.with_message(&message);

        // Add labels
        for label in &self.labels {
            let span_start = label.span.start as usize;
            let span_end = label.span.end as usize;
            let label_file: String = source_map.file_name(label.span.file).to_string();

            let ariadne_label = ariadne::Label::new((label_file, span_start..span_end))
                .with_message(&label.message)
                .with_color(label.color(self.level));

            report = report.with_label(ariadne_label);
        }

        // Add notes
        for note in &self.notes {
            report = report.with_note(note);
        }

        // Add suggestions as notes (ariadne doesn't have native suggestion support)
        for suggestion in &self.suggestions {
            let note = format!(
                "suggestion: {}: replace with `{}`",
                suggestion.message, suggestion.replacement
            );
            report = report.with_note(note);
        }

        // Build and write the report
        let report = report.finish();

        // Create a source cache for ariadne
        // The cache maps source IDs (String) to source content (String)
        let sources: Vec<(String, String)> = source_map
            .files()
            .map(|file| {
                (
                    source_map.file_name(file.id()).to_string(),
                    file.source().to_string(),
                )
            })
            .collect();

        report
            .write(ariadne::sources(sources), writer)
            .map_err(io::Error::other)?;

        Ok(())
    }

    /// Renders this diagnostic to a string.
    #[must_use]
    pub fn to_string_with_source(&self, source_map: &SourceMap) -> String {
        let mut buf = Vec::new();
        let _ = self.write(source_map, &mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    }
}

// ============================================================================
// Diagnostics (collection)
// ============================================================================

/// A collection of diagnostics from a compilation phase.
///
/// This type accumulates diagnostics during compilation and provides
/// methods to check for errors and emit all diagnostics.
#[derive(Debug, Default)]
pub struct Diagnostics {
    items: Vec<Diagnostic>,
}

impl Diagnostics {
    /// Creates an empty diagnostics collection.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a diagnostic to the collection.
    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.items.push(diagnostic);
    }

    /// Adds an error diagnostic and returns a mutable reference to it.
    ///
    /// This allows chaining builder methods on the returned diagnostic.
    #[allow(clippy::missing_panics_doc)] // unwrap is safe: we just pushed
    pub fn error(&mut self, message: impl Into<String>) -> &mut Diagnostic {
        self.items.push(Diagnostic::error(message));
        self.items.last_mut().unwrap()
    }

    /// Adds a warning diagnostic and returns a mutable reference to it.
    ///
    /// This allows chaining builder methods on the returned diagnostic.
    #[allow(clippy::missing_panics_doc)] // unwrap is safe: we just pushed
    pub fn warning(&mut self, message: impl Into<String>) -> &mut Diagnostic {
        self.items.push(Diagnostic::warning(message));
        self.items.last_mut().unwrap()
    }

    /// Returns true if any errors have been recorded.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.items.iter().any(Diagnostic::is_error)
    }

    /// Returns true if any warnings have been recorded.
    #[must_use]
    pub fn has_warnings(&self) -> bool {
        self.items.iter().any(Diagnostic::is_warning)
    }

    /// Returns the number of errors.
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.items.iter().filter(|d| d.is_error()).count()
    }

    /// Returns the number of warnings.
    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.items.iter().filter(|d| d.is_warning()).count()
    }

    /// Returns true if no diagnostics have been recorded.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // Vec::is_empty is not const
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Returns the number of diagnostics.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // Vec::len is not const
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Returns an iterator over the diagnostics.
    pub fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items.iter()
    }

    /// Emits all diagnostics to stderr.
    pub fn emit_all(&self, source_map: &SourceMap) {
        for diag in &self.items {
            diag.emit(source_map);
        }

        // Print summary
        let errors = self.error_count();
        let warnings = self.warning_count();

        if errors > 0 || warnings > 0 {
            let mut parts = Vec::new();
            if errors > 0 {
                parts.push(format!(
                    "{} error{}",
                    errors,
                    if errors == 1 { "" } else { "s" }
                ));
            }
            if warnings > 0 {
                parts.push(format!(
                    "{} warning{}",
                    warnings,
                    if warnings == 1 { "" } else { "s" }
                ));
            }
            eprintln!("\n{} emitted", parts.join("; "));
        }
    }

    /// Merges another diagnostics collection into this one.
    pub fn extend(&mut self, other: Self) {
        self.items.extend(other.items);
    }

    /// Consumes this collection and returns the diagnostics.
    #[must_use]
    pub fn into_vec(self) -> Vec<Diagnostic> {
        self.items
    }

    /// Sorts diagnostics by file and position.
    pub fn sort_by_location(&mut self) {
        self.items.sort_by(|a, b| {
            let a_span = a.primary_span().unwrap_or(Span::DUMMY);
            let b_span = b.primary_span().unwrap_or(Span::DUMMY);

            a_span
                .file
                .cmp(&b_span.file)
                .then(a_span.start.cmp(&b_span.start))
        });
    }
}

impl IntoIterator for Diagnostics {
    type Item = Diagnostic;
    type IntoIter = std::vec::IntoIter<Diagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

impl<'a> IntoIterator for &'a Diagnostics {
    type Item = &'a Diagnostic;
    type IntoIter = std::slice::Iter<'a, Diagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

impl Extend<Diagnostic> for Diagnostics {
    fn extend<T: IntoIterator<Item = Diagnostic>>(&mut self, iter: T) {
        self.items.extend(iter);
    }
}

// ============================================================================
// Result type alias
// ============================================================================

/// A result type that uses [`Diagnostics`] as the error type.
///
/// This is useful for compiler phases that may produce multiple errors.
pub type DiagnosticResult<T> = Result<T, Diagnostics>;

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_display() {
        assert_eq!(format!("{}", Level::Error), "error");
        assert_eq!(format!("{}", Level::Warning), "warning");
        assert_eq!(format!("{}", Level::Note), "note");
        assert_eq!(format!("{}", Level::Help), "help");
    }

    #[test]
    fn level_predicates() {
        assert!(Level::Error.is_error());
        assert!(!Level::Warning.is_error());
        assert!(Level::Warning.is_warning());
        assert!(!Level::Error.is_warning());
    }

    #[test]
    fn diagnostic_builder() {
        let span = Span::new(FileId::new(0), 0, 10);

        let diag = Diagnostic::error("test error")
            .with_code("E0001")
            .with_label(Label::primary(span, "here"))
            .with_note("this is a note");

        assert!(diag.is_error());
        assert_eq!(diag.message, "test error");
        assert_eq!(diag.code.as_ref().map(|c| c.as_str()), Some("E0001"));
        assert_eq!(diag.labels.len(), 1);
        assert_eq!(diag.notes.len(), 1);
    }

    #[test]
    fn diagnostic_warning() {
        let diag = Diagnostic::warning("unused variable");

        assert!(!diag.is_error());
        assert!(diag.is_warning());
    }

    #[test]
    fn diagnostics_collection() {
        let mut diags = Diagnostics::new();

        diags.push(Diagnostic::error("error 1"));
        diags.push(Diagnostic::warning("warning 1"));
        diags.push(Diagnostic::error("error 2"));

        assert!(diags.has_errors());
        assert!(diags.has_warnings());
        assert_eq!(diags.error_count(), 2);
        assert_eq!(diags.warning_count(), 1);
        assert_eq!(diags.len(), 3);
    }

    #[test]
    fn diagnostics_empty() {
        let diags = Diagnostics::new();

        assert!(diags.is_empty());
        assert!(!diags.has_errors());
        assert!(!diags.has_warnings());
    }

    #[test]
    fn label_creation() {
        let span = Span::new(FileId::new(0), 0, 5);

        let primary = Label::primary(span, "primary message");
        assert!(primary.is_primary);
        assert_eq!(primary.message, "primary message");

        let secondary = Label::secondary(span, "secondary message");
        assert!(!secondary.is_primary);
        assert_eq!(secondary.message, "secondary message");
    }

    #[test]
    fn suggestion_creation() {
        let span = Span::new(FileId::new(0), 0, 5);

        let suggestion = Suggestion::new("replace with bar", span, "bar");
        assert_eq!(suggestion.message, "replace with bar");
        assert_eq!(suggestion.replacement, "bar");
    }

    #[test]
    fn diagnostic_primary_span() {
        let span1 = Span::new(FileId::new(0), 0, 5);
        let span2 = Span::new(FileId::new(0), 10, 15);

        let diag = Diagnostic::error("test")
            .with_label(Label::secondary(span1, "secondary"))
            .with_label(Label::primary(span2, "primary"));

        assert_eq!(diag.primary_span(), Some(span2));
    }

    #[test]
    fn diagnostics_extend() {
        let mut diags1 = Diagnostics::new();
        diags1.push(Diagnostic::error("error 1"));

        let mut diags2 = Diagnostics::new();
        diags2.push(Diagnostic::error("error 2"));

        diags1.extend(diags2);
        assert_eq!(diags1.len(), 2);
    }

    #[test]
    fn diagnostic_to_string() {
        let mut source_map = SourceMap::new();
        let file_id = source_map.add_file("test.spark", "let x = 42");
        let span = Span::new(file_id, 4, 5);

        let diag =
            Diagnostic::error("undefined variable").with_label(Label::primary(span, "not found"));

        let output = diag.to_string_with_source(&source_map);
        assert!(output.contains("undefined variable"));
    }

    #[test]
    fn diagnostics_sort() {
        let file = FileId::new(0);
        let mut diags = Diagnostics::new();

        diags.push(
            Diagnostic::error("second").with_label(Label::primary(Span::new(file, 20, 25), "")),
        );
        diags
            .push(Diagnostic::error("first").with_label(Label::primary(Span::new(file, 0, 5), "")));

        diags.sort_by_location();

        let messages: Vec<_> = diags.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(messages, vec!["first", "second"]);
    }
}
