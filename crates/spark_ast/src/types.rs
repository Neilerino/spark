//! Type expression nodes.

use spark_span::Span;

use crate::{Ident, Path};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// A type expression in source code.
///
/// Type expressions appear in variable declarations, function signatures,
/// struct fields, and other contexts where types are specified.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct TypeExpr {
    /// The kind of type expression.
    pub kind: TypeExprKind,
    /// The source span.
    pub span: Span,
}

impl TypeExpr {
    /// Creates a new type expression.
    #[must_use]
    pub fn new(kind: TypeExprKind, span: Span) -> Self {
        Self { kind, span }
    }

    /// Creates an error type for recovery.
    #[must_use]
    pub fn error(span: Span) -> Self {
        Self::new(TypeExprKind::Error, span)
    }

    /// Creates an inferred type placeholder.
    #[must_use]
    pub fn infer(span: Span) -> Self {
        Self::new(TypeExprKind::Infer, span)
    }
}

/// The different kinds of type expressions.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum TypeExprKind {
    /// A named type, possibly with generic arguments.
    ///
    /// Examples: `Int`, `User`, `List[Int]`, `Map[String, User]`
    Named {
        /// The type path (e.g., `std.collections.List`).
        path: Path,
        /// Generic type arguments (e.g., `[Int, String]`).
        type_args: Vec<TypeExpr>,
    },

    /// A tuple type: `Tup[A, B, C]`
    Tuple(Vec<TypeExpr>),

    /// A function type: `(Int, String): Bool`
    Function {
        /// Parameter types.
        params: Vec<TypeExpr>,
        /// Return type.
        ret: Box<TypeExpr>,
    },

    /// An optional type: `Type?` (sugar for `Type | Null`)
    Optional(Box<TypeExpr>),

    /// A union type: `A | B | C`
    Union(Vec<TypeExpr>),

    /// A mutable type: `Type!`
    Mutable(Box<TypeExpr>),

    /// A reference type: `Ref[Type, Lifetime]` or `Ref[Type, Lifetime]!`
    Ref {
        /// The referenced type.
        inner: Box<TypeExpr>,
        /// The lifetime identifier.
        lifetime: Ident,
        /// Whether this is a mutable reference.
        mutable: bool,
    },

    /// A structural type constraint: `{ field: Type, ... }`
    Structural(Vec<StructuralField>),

    /// An inferred type (for error recovery or internal use).
    Infer,

    /// An error type (for error recovery).
    Error,
}

/// A field in a structural type constraint.
///
/// Used in generic constraints like `[T: { name: String, age: Int }]`.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct StructuralField {
    /// The field name.
    pub name: Ident,
    /// The field type.
    pub ty: TypeExpr,
    /// The span of the field.
    pub span: Span,
}

impl StructuralField {
    /// Creates a new structural field.
    #[must_use]
    pub fn new(name: Ident, ty: TypeExpr, span: Span) -> Self {
        Self { name, ty, span }
    }
}

/// A generic parameter declaration.
///
/// Examples: `T`, `T: Printable`, `T: { name: String }`
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct GenericParam {
    /// The parameter name.
    pub name: Ident,
    /// Optional constraint.
    pub constraint: Option<GenericConstraint>,
    /// The span.
    pub span: Span,
}

impl GenericParam {
    /// Creates a new unconstrained generic parameter.
    #[must_use]
    pub fn new(name: Ident, span: Span) -> Self {
        Self {
            name,
            constraint: None,
            span,
        }
    }

    /// Creates a generic parameter with a constraint.
    #[must_use]
    pub fn with_constraint(name: Ident, constraint: GenericConstraint, span: Span) -> Self {
        Self {
            name,
            constraint: Some(constraint),
            span,
        }
    }
}

/// A constraint on a generic parameter.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum GenericConstraint {
    /// A named constraint (interface or type): `T: Printable`
    Named(Path),
    /// A structural constraint: `T: { name: String }`
    Structural(Vec<StructuralField>),
}

/// A lifetime parameter for reference types.
///
/// In Spark, lifetime parameters are single uppercase letters like `A`, `B`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct LifetimeParam {
    /// The lifetime identifier (e.g., `A`, `B`).
    pub name: Ident,
    /// The span.
    pub span: Span,
}

impl LifetimeParam {
    /// Creates a new lifetime parameter.
    #[must_use]
    pub fn new(name: Ident, span: Span) -> Self {
        Self { name, span }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ident;
    use spark_intern::sym;

    fn dummy_ident(s: &str) -> Ident {
        Ident::dummy(sym(s))
    }

    fn dummy_path(s: &str) -> Path {
        Path::from_ident(dummy_ident(s))
    }

    #[test]
    fn type_expr_named_simple() {
        let ty = TypeExpr::new(
            TypeExprKind::Named {
                path: dummy_path("Int"),
                type_args: vec![],
            },
            Span::DUMMY,
        );

        if let TypeExprKind::Named { path, type_args } = ty.kind {
            assert!(path.is_simple());
            assert!(type_args.is_empty());
        } else {
            panic!("Expected Named type");
        }
    }

    #[test]
    fn type_expr_named_generic() {
        let inner_ty = TypeExpr::new(
            TypeExprKind::Named {
                path: dummy_path("String"),
                type_args: vec![],
            },
            Span::DUMMY,
        );

        let ty = TypeExpr::new(
            TypeExprKind::Named {
                path: dummy_path("List"),
                type_args: vec![inner_ty],
            },
            Span::DUMMY,
        );

        if let TypeExprKind::Named { type_args, .. } = ty.kind {
            assert_eq!(type_args.len(), 1);
        } else {
            panic!("Expected Named type");
        }
    }

    #[test]
    fn type_expr_optional() {
        let inner = Box::new(TypeExpr::new(
            TypeExprKind::Named {
                path: dummy_path("User"),
                type_args: vec![],
            },
            Span::DUMMY,
        ));

        let ty = TypeExpr::new(TypeExprKind::Optional(inner), Span::DUMMY);

        assert!(matches!(ty.kind, TypeExprKind::Optional(_)));
    }

    #[test]
    fn type_expr_union() {
        let types = vec![
            TypeExpr::new(
                TypeExprKind::Named {
                    path: dummy_path("User"),
                    type_args: vec![],
                },
                Span::DUMMY,
            ),
            TypeExpr::new(
                TypeExprKind::Named {
                    path: dummy_path("NotFound"),
                    type_args: vec![],
                },
                Span::DUMMY,
            ),
        ];

        let ty = TypeExpr::new(TypeExprKind::Union(types), Span::DUMMY);

        if let TypeExprKind::Union(types) = ty.kind {
            assert_eq!(types.len(), 2);
        } else {
            panic!("Expected Union type");
        }
    }

    #[test]
    fn type_expr_function() {
        let params = vec![TypeExpr::new(
            TypeExprKind::Named {
                path: dummy_path("Int"),
                type_args: vec![],
            },
            Span::DUMMY,
        )];

        let ret = Box::new(TypeExpr::new(
            TypeExprKind::Named {
                path: dummy_path("String"),
                type_args: vec![],
            },
            Span::DUMMY,
        ));

        let ty = TypeExpr::new(TypeExprKind::Function { params, ret }, Span::DUMMY);

        assert!(matches!(ty.kind, TypeExprKind::Function { .. }));
    }

    #[test]
    fn generic_param_unconstrained() {
        let param = GenericParam::new(dummy_ident("T"), Span::DUMMY);
        assert!(param.constraint.is_none());
    }

    #[test]
    fn generic_param_with_constraint() {
        let constraint = GenericConstraint::Named(dummy_path("Printable"));
        let param = GenericParam::with_constraint(dummy_ident("T"), constraint, Span::DUMMY);
        assert!(param.constraint.is_some());
    }
}
