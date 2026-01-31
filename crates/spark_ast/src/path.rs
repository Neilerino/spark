//! Identifier and path types.

use spark_intern::Symbol;
use spark_span::Span;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// An identifier with its source location.
///
/// Identifiers are interned using `Symbol` for efficient comparison
/// and reduced memory usage.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Ident {
    /// The interned name.
    pub name: Symbol,
    /// The source span.
    pub span: Span,
}

impl Ident {
    /// Creates a new identifier.
    #[must_use]
    pub const fn new(name: Symbol, span: Span) -> Self {
        Self { name, span }
    }

    /// Creates a dummy identifier for testing or error recovery.
    #[must_use]
    pub fn dummy(name: Symbol) -> Self {
        Self {
            name,
            span: Span::DUMMY,
        }
    }
}

/// A path to a module, type, or item.
///
/// Paths consist of one or more segments separated by dots.
///
/// # Examples
///
/// - Simple: `user`
/// - Qualified: `user.auth.Token`
/// - Module path: `std.collections`
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Path {
    /// The segments of the path.
    pub segments: Vec<Ident>,
    /// The full span covering all segments.
    pub span: Span,
}

impl Path {
    /// Creates a new path from segments.
    #[must_use]
    pub fn new(segments: Vec<Ident>, span: Span) -> Self {
        Self { segments, span }
    }

    /// Creates a path with a single segment.
    #[must_use]
    pub fn from_ident(ident: Ident) -> Self {
        let span = ident.span;
        Self {
            segments: vec![ident],
            span,
        }
    }

    /// Returns true if this is a simple single-segment path.
    #[must_use]
    pub fn is_simple(&self) -> bool {
        self.segments.len() == 1
    }

    /// Returns the first segment of the path.
    #[must_use]
    pub fn first(&self) -> Option<&Ident> {
        self.segments.first()
    }

    /// Returns the last segment of the path.
    #[must_use]
    pub fn last(&self) -> Option<&Ident> {
        self.segments.last()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_intern::sym;

    #[test]
    fn ident_creation() {
        let name = sym("foo");
        let span = Span::new(spark_span::FileId::new(0), 0, 3);
        let ident = Ident::new(name, span);

        assert_eq!(ident.name, name);
        assert_eq!(ident.span, span);
    }

    #[test]
    fn ident_dummy() {
        let name = sym("test");
        let ident = Ident::dummy(name);

        assert_eq!(ident.name, name);
        assert!(ident.span.is_dummy());
    }

    #[test]
    fn path_from_ident() {
        let ident = Ident::dummy(sym("module"));
        let path = Path::from_ident(ident.clone());

        assert!(path.is_simple());
        assert_eq!(path.segments.len(), 1);
        assert_eq!(path.first().unwrap().name, ident.name);
    }

    #[test]
    fn path_multi_segment() {
        let segments = vec![
            Ident::dummy(sym("std")),
            Ident::dummy(sym("collections")),
            Ident::dummy(sym("List")),
        ];
        let path = Path::new(segments, Span::DUMMY);

        assert!(!path.is_simple());
        assert_eq!(path.segments.len(), 3);
        assert_eq!(path.first().unwrap().name, sym("std"));
        assert_eq!(path.last().unwrap().name, sym("List"));
    }
}
