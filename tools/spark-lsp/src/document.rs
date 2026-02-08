//! Document state management and span-to-LSP conversion utilities.
//!
//! Each open document has a [`DocumentState`] that holds its source text
//! (as a [`Rope`]) and provides helpers for converting between the
//! compiler's byte-offset [`Span`]s and LSP line/character positions.

use ropey::Rope;
use spark_diagnostics::Diagnostic;
use spark_span::Span;
use tower_lsp::lsp_types;

/// State for a single open document.
pub struct DocumentState {
    /// The full source text, stored as a rope for efficient edits.
    pub rope: Rope,
}

impl DocumentState {
    /// Creates a new document state from source text.
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self {
            rope: Rope::from_str(text),
        }
    }

    /// Replaces the document contents with new text.
    pub fn update(&mut self, text: &str) {
        self.rope = Rope::from_str(text);
    }
}

/// Converts a byte offset to an LSP `Position` (line, character).
///
/// Returns `None` if the offset is out of bounds.
#[must_use]
pub fn offset_to_position(rope: &Rope, byte_offset: u32) -> Option<lsp_types::Position> {
    let offset = byte_offset as usize;
    if offset > rope.len_bytes() {
        return None;
    }
    let char_idx = rope.try_byte_to_char(offset).ok()?;
    let line = rope.char_to_line(char_idx);
    let line_start_char = rope.line_to_char(line);
    let character = char_idx - line_start_char;
    Some(lsp_types::Position {
        line: line as u32,
        character: character as u32,
    })
}

/// Converts a compiler [`Span`] to an LSP `Range`.
///
/// Returns `None` if either endpoint is out of bounds.
#[must_use]
pub fn span_to_range(rope: &Rope, span: Span) -> Option<lsp_types::Range> {
    let start = offset_to_position(rope, span.start)?;
    let end = offset_to_position(rope, span.end)?;
    Some(lsp_types::Range { start, end })
}

/// Converts a [`spark_diagnostics::Diagnostic`] to an LSP `Diagnostic`.
///
/// The primary label's span becomes the diagnostic range. Secondary labels
/// are included as related information.
#[must_use]
pub fn diagnostic_to_lsp(
    diag: &Diagnostic,
    rope: &Rope,
    uri: &lsp_types::Url,
) -> lsp_types::Diagnostic {
    // Find the primary label for the diagnostic range
    let primary_label = diag.labels.iter().find(|l| l.is_primary);
    let range = primary_label
        .and_then(|l| span_to_range(rope, l.span))
        .unwrap_or(lsp_types::Range {
            start: lsp_types::Position {
                line: 0,
                character: 0,
            },
            end: lsp_types::Position {
                line: 0,
                character: 0,
            },
        });

    let severity = if diag.is_error() {
        Some(lsp_types::DiagnosticSeverity::ERROR)
    } else if diag.is_warning() {
        Some(lsp_types::DiagnosticSeverity::WARNING)
    } else {
        Some(lsp_types::DiagnosticSeverity::INFORMATION)
    };

    // Convert secondary labels to related information
    let related_information: Vec<lsp_types::DiagnosticRelatedInformation> = diag
        .labels
        .iter()
        .filter(|l| !l.is_primary)
        .filter_map(|l| {
            let label_range = span_to_range(rope, l.span)?;
            Some(lsp_types::DiagnosticRelatedInformation {
                location: lsp_types::Location {
                    uri: uri.clone(),
                    range: label_range,
                },
                message: l.message.clone(),
            })
        })
        .collect();

    lsp_types::Diagnostic {
        range,
        severity,
        code: diag.code.as_ref().map(|c| {
            lsp_types::NumberOrString::String(c.as_str().to_string())
        }),
        source: Some("spark".to_string()),
        message: diag.message.clone(),
        related_information: if related_information.is_empty() {
            None
        } else {
            Some(related_information)
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_span::FileId;

    #[test]
    fn offset_to_position_first_line() {
        let rope = Rope::from_str("hello world");
        let pos = offset_to_position(&rope, 5).unwrap();
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character, 5);
    }

    #[test]
    fn offset_to_position_second_line() {
        let rope = Rope::from_str("hello\nworld");
        let pos = offset_to_position(&rope, 6).unwrap();
        assert_eq!(pos.line, 1);
        assert_eq!(pos.character, 0);
    }

    #[test]
    fn offset_to_position_out_of_bounds() {
        let rope = Rope::from_str("hi");
        assert!(offset_to_position(&rope, 100).is_none());
    }

    #[test]
    fn span_to_range_basic() {
        let rope = Rope::from_str("fn main(): Void { }");
        let span = Span::new(FileId::new(0), 3, 7);
        let range = span_to_range(&rope, span).unwrap();
        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 3);
        assert_eq!(range.end.line, 0);
        assert_eq!(range.end.character, 7);
    }
}
