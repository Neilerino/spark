//! Literal value representations.

use spark_intern::Symbol;
use spark_span::Span;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// A literal value in source code.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Literal {
    /// The kind of literal.
    pub kind: LiteralKind,
    /// The source span.
    pub span: Span,
}

impl Literal {
    /// Creates a new literal.
    #[must_use]
    pub const fn new(kind: LiteralKind, span: Span) -> Self {
        Self { kind, span }
    }

    /// Creates an integer literal.
    #[must_use]
    pub const fn int(value: i64, span: Span) -> Self {
        Self::new(LiteralKind::Int(value), span)
    }

    /// Creates a float literal.
    #[must_use]
    pub const fn float(value: f64, span: Span) -> Self {
        Self::new(LiteralKind::Float(value), span)
    }

    /// Creates a boolean literal.
    #[must_use]
    pub const fn bool(value: bool, span: Span) -> Self {
        Self::new(LiteralKind::Bool(value), span)
    }

    /// Creates a null literal.
    #[must_use]
    pub const fn null(span: Span) -> Self {
        Self::new(LiteralKind::Null, span)
    }

    /// Creates a character literal.
    #[must_use]
    pub const fn char(value: char, span: Span) -> Self {
        Self::new(LiteralKind::Char(value), span)
    }
}

/// The different kinds of literals.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum LiteralKind {
    /// Integer literal: `42`, `0xFF`, `0b1010`
    Int(i64),

    /// Float literal: `3.14`, `1.5e10`
    Float(f64),

    /// String literal: `"hello"`, including interpolated parts.
    String(StringLiteral),

    /// Character literal: `'a'`
    Char(char),

    /// Boolean literal: `true` or `false`
    Bool(bool),

    /// Null literal: `Null`
    Null,
}

/// A string literal that may contain interpolations.
///
/// # Examples
///
/// - Simple: `"hello"`
/// - Interpolated: `"Hello, {name}!"`
/// - Raw: `r"C:\Users\Alice"`
/// - Multiline: `"""..."""`
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct StringLiteral {
    /// The parts of the string.
    pub parts: Vec<StringPart>,
    /// Whether this is a raw string (r"...").
    pub is_raw: bool,
    /// Whether this is a multiline string (""").
    pub is_multiline: bool,
}

impl StringLiteral {
    /// Creates a simple string with no interpolation.
    #[must_use]
    pub fn simple(text: Symbol) -> Self {
        Self {
            parts: vec![StringPart::Text(text)],
            is_raw: false,
            is_multiline: false,
        }
    }

    /// Creates a raw string.
    #[must_use]
    pub fn raw(text: Symbol) -> Self {
        Self {
            parts: vec![StringPart::Text(text)],
            is_raw: true,
            is_multiline: false,
        }
    }

    /// Returns true if this string has no interpolations.
    #[must_use]
    pub fn is_simple(&self) -> bool {
        self.parts.len() == 1 && matches!(self.parts.first(), Some(StringPart::Text(_)))
    }
}

/// A part of a string literal.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum StringPart {
    /// A literal text segment.
    Text(Symbol),
    /// An interpolated expression: `{expr}`.
    Interpolation(Box<crate::Expr>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_intern::sym;

    #[test]
    fn literal_int() {
        let lit = Literal::int(42, Span::DUMMY);
        assert!(matches!(lit.kind, LiteralKind::Int(42)));
    }

    #[test]
    fn literal_float() {
        let lit = Literal::float(3.14, Span::DUMMY);
        if let LiteralKind::Float(f) = lit.kind {
            assert!((f - 3.14).abs() < f64::EPSILON);
        } else {
            panic!("Expected float literal");
        }
    }

    #[test]
    fn literal_bool() {
        let lit_true = Literal::bool(true, Span::DUMMY);
        let lit_false = Literal::bool(false, Span::DUMMY);

        assert!(matches!(lit_true.kind, LiteralKind::Bool(true)));
        assert!(matches!(lit_false.kind, LiteralKind::Bool(false)));
    }

    #[test]
    fn literal_null() {
        let lit = Literal::null(Span::DUMMY);
        assert!(matches!(lit.kind, LiteralKind::Null));
    }

    #[test]
    fn literal_char() {
        let lit = Literal::char('A', Span::DUMMY);
        assert!(matches!(lit.kind, LiteralKind::Char('A')));
    }

    #[test]
    fn string_literal_simple() {
        let s = StringLiteral::simple(sym("hello"));
        assert!(s.is_simple());
        assert!(!s.is_raw);
        assert!(!s.is_multiline);
    }

    #[test]
    fn string_literal_raw() {
        let s = StringLiteral::raw(sym(r"C:\Users"));
        assert!(s.is_simple());
        assert!(s.is_raw);
    }
}
