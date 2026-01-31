//! Pattern nodes for match expressions.

use spark_span::Span;

use crate::{Expr, Ident, Literal, Path, TypeExpr};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// A pattern for matching values.
///
/// Patterns appear in match expressions, let bindings with destructuring,
/// and function parameters.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Pattern {
    /// The kind of pattern.
    pub kind: PatternKind,
    /// The source span.
    pub span: Span,
}

impl Pattern {
    /// Creates a new pattern.
    #[must_use]
    pub fn new(kind: PatternKind, span: Span) -> Self {
        Self { kind, span }
    }

    /// Creates a wildcard pattern.
    #[must_use]
    pub fn wildcard(span: Span) -> Self {
        Self::new(PatternKind::Wildcard, span)
    }

    /// Creates an identifier pattern.
    #[must_use]
    pub fn ident(name: Ident) -> Self {
        let span = name.span;
        Self::new(
            PatternKind::Ident {
                name,
                ty: None,
            },
            span,
        )
    }

    /// Creates an identifier pattern with type annotation.
    #[must_use]
    pub fn ident_typed(name: Ident, ty: TypeExpr, span: Span) -> Self {
        Self::new(
            PatternKind::Ident {
                name,
                ty: Some(ty),
            },
            span,
        )
    }
}

/// The different kinds of patterns.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum PatternKind {
    /// Wildcard pattern: `_`
    Wildcard,

    /// Identifier pattern (binds the matched value): `x`, `user`
    Ident {
        /// The binding name.
        name: Ident,
        /// Optional type annotation: `x: Int`
        ty: Option<TypeExpr>,
    },

    /// Literal pattern: `42`, `"hello"`, `true`
    Literal(Literal),

    /// Enum variant pattern: `Status.Active`, `Result.Success(value)`
    Variant {
        /// The path to the variant.
        path: Path,
        /// Destructured fields (positional).
        fields: Vec<Pattern>,
    },

    /// Struct destructuring pattern: `User { name, email }`
    Struct {
        /// The struct type path.
        path: Path,
        /// Field patterns.
        fields: Vec<FieldPattern>,
        /// Whether `..` is present to ignore remaining fields.
        rest: bool,
    },

    /// Tuple destructuring pattern: `(a, b, c)`
    Tuple(Vec<Pattern>),

    /// List pattern: `[first, second, ...rest]`
    List {
        /// Element patterns.
        elements: Vec<Pattern>,
        /// Optional rest pattern for remaining elements.
        rest: Option<Box<Pattern>>,
    },

    /// Type narrowing pattern: `user: User`
    ///
    /// Used in match on union types to narrow to a specific type.
    TypeNarrowing {
        /// The binding name.
        name: Ident,
        /// The type to narrow to.
        ty: TypeExpr,
    },

    /// Or pattern: `A | B | C`
    Or(Vec<Pattern>),

    /// Pattern with guard: `n if n > 0`
    Guard {
        /// The pattern to match.
        pattern: Box<Pattern>,
        /// The guard condition.
        guard: Box<Expr>,
    },

    /// Error pattern (for error recovery).
    Error,
}

/// A field pattern in struct destructuring.
///
/// Examples:
/// - `name` (shorthand, binds to local `name`)
/// - `name: n` (binds field `name` to local `n`)
/// - `name: _` (matches but ignores)
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct FieldPattern {
    /// The field name.
    pub name: Ident,
    /// The pattern to match against (None for shorthand syntax).
    pub pattern: Option<Pattern>,
    /// The span.
    pub span: Span,
}

impl FieldPattern {
    /// Creates a new field pattern.
    #[must_use]
    pub fn new(name: Ident, pattern: Option<Pattern>, span: Span) -> Self {
        Self { name, pattern, span }
    }

    /// Creates a shorthand field pattern (binds field to same-named local).
    #[must_use]
    pub fn shorthand(name: Ident) -> Self {
        let span = name.span;
        Self {
            name,
            pattern: None,
            span,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_intern::sym;

    fn dummy_ident(s: &str) -> Ident {
        Ident::dummy(sym(s))
    }

    #[test]
    fn pattern_wildcard() {
        let pat = Pattern::wildcard(Span::DUMMY);
        assert!(matches!(pat.kind, PatternKind::Wildcard));
    }

    #[test]
    fn pattern_ident() {
        let pat = Pattern::ident(dummy_ident("x"));
        if let PatternKind::Ident { name, ty } = pat.kind {
            assert_eq!(name.name, sym("x"));
            assert!(ty.is_none());
        } else {
            panic!("Expected Ident pattern");
        }
    }

    #[test]
    fn pattern_tuple() {
        let patterns = vec![
            Pattern::ident(dummy_ident("a")),
            Pattern::ident(dummy_ident("b")),
        ];
        let pat = Pattern::new(PatternKind::Tuple(patterns), Span::DUMMY);

        if let PatternKind::Tuple(elems) = pat.kind {
            assert_eq!(elems.len(), 2);
        } else {
            panic!("Expected Tuple pattern");
        }
    }

    #[test]
    fn pattern_or() {
        let patterns = vec![
            Pattern::new(PatternKind::Literal(Literal::int(1, Span::DUMMY)), Span::DUMMY),
            Pattern::new(PatternKind::Literal(Literal::int(2, Span::DUMMY)), Span::DUMMY),
        ];
        let pat = Pattern::new(PatternKind::Or(patterns), Span::DUMMY);

        if let PatternKind::Or(alts) = pat.kind {
            assert_eq!(alts.len(), 2);
        } else {
            panic!("Expected Or pattern");
        }
    }

    #[test]
    fn field_pattern_shorthand() {
        let fp = FieldPattern::shorthand(dummy_ident("name"));
        assert!(fp.pattern.is_none());
    }
}
