//! Desugaring transforms.
//!
//! Converts syntactic sugar into core HIR constructs:
//! - `a ?? b` → `match a { v -> v, Null -> b }`
//! - `a?.f` → `match a { v -> Some(v.f), Null -> Null }`
//! - `e!` → `match e { v -> v, err -> return err }`

use spark_intern::Symbol;
use spark_resolve::DefinitionTable;
use spark_span::Span;
use spark_types::TypeId;

use crate::hir::{HirExpr, HirExprKind, HirLiteral, HirMatchArm, HirPattern};

/// Desugars `lhs ?? rhs` into a match expression.
///
/// ```text
/// match lhs {
///     v -> v,     // non-null arm (binding pattern)
///     _ -> rhs,   // null arm (wildcard, returns default)
/// }
/// ```
#[must_use]
pub fn desugar_coalesce(
    lhs: HirExpr,
    rhs: HirExpr,
    span: Span,
    ty: TypeId,
    definitions: &mut DefinitionTable,
) -> HirExpr {
    // The non-null arm binds the value and returns it.
    let fresh_id = definitions.next_synthetic_id(span);
    let non_null_arm = HirMatchArm {
        pattern: HirPattern::Binding { def_id: fresh_id },
        guard: None,
        body: HirExpr::new(HirExprKind::Local(fresh_id), ty, span),
        span,
    };

    let null_arm = HirMatchArm {
        pattern: HirPattern::Literal(HirLiteral::Null),
        guard: None,
        body: rhs,
        span,
    };

    // Order: null arm first (specific), then wildcard (catch-all)
    HirExpr::new(
        HirExprKind::Match {
            scrutinee: Box::new(lhs),
            arms: vec![null_arm, non_null_arm],
        },
        ty,
        span,
    )
}

/// Desugars `base?.field` into a match on the optional base.
///
/// ```text
/// match base {
///     v -> Some(v.field),
///     Null -> Null,
/// }
/// ```
///
/// Simplified: since we can't synthesize fresh `DefId`s, we produce a Match
/// that captures the intent. The field access type is the optional result type.
#[must_use]
pub fn desugar_optional_field(
    base: HirExpr,
    field: Symbol,
    span: Span,
    ty: TypeId,
    inner_ty: TypeId,
    definitions: &mut DefinitionTable,
) -> HirExpr {
    let fresh_id = definitions.next_synthetic_id(span);
    let field_arm = HirMatchArm {
        pattern: HirPattern::Binding { def_id: fresh_id },
        guard: None,
        body: HirExpr::new(
            HirExprKind::FieldAccess {
                base: Box::new(HirExpr::new(HirExprKind::Local(fresh_id), inner_ty, span)),
                field,
            },
            ty,
            span,
        ),
        span,
    };

    let null_arm = HirMatchArm {
        pattern: HirPattern::Literal(HirLiteral::Null),
        guard: None,
        body: HirExpr::new(HirExprKind::Literal(HirLiteral::Null), ty, span),
        span,
    };

    HirExpr::new(
        HirExprKind::Match {
            scrutinee: Box::new(base),
            arms: vec![null_arm, field_arm],
        },
        ty,
        span,
    )
}

/// Desugars `expr!` (propagate) into a match with early return.
///
/// ```text
/// match expr {
///     v -> v,              // success arm
///     err -> return err,   // error propagation arm
/// }
/// ```
///
/// Simplified: since we can't distinguish success/error variants without
/// deeper union type analysis, we produce a match that captures the intent.
/// Desugars `expr!` for the simple optional case (`T?`).
///
/// ```text
/// match expr {
///     v -> v,              // non-null arm (binding)
///     Null -> return Null,  // null propagation arm
/// }
/// ```
#[must_use]
pub fn desugar_propagate_optional(
    inner: HirExpr,
    span: Span,
    success_ty: TypeId,
    null_ty: TypeId,
    definitions: &mut DefinitionTable,
) -> HirExpr {
    let fresh_id = definitions.next_synthetic_id(span);
    let success_arm = HirMatchArm {
        pattern: HirPattern::Binding { def_id: fresh_id },
        guard: None,
        body: HirExpr::new(HirExprKind::Local(fresh_id), success_ty, span),
        span,
    };

    let null_arm = HirMatchArm {
        pattern: HirPattern::Literal(HirLiteral::Null),
        guard: None,
        body: HirExpr::new(
            HirExprKind::Return(Some(Box::new(HirExpr::new(
                HirExprKind::Literal(HirLiteral::Null),
                null_ty,
                span,
            )))),
            TypeId::ERROR, // return is Never
            span,
        ),
        span,
    };

    HirExpr::new(
        HirExprKind::Match {
            scrutinee: Box::new(inner),
            arms: vec![null_arm, success_arm],
        },
        success_ty,
        span,
    )
}

/// Desugars `expr!` for union types (`Success | Err1 | Err2`).
///
/// ```text
/// match expr {
///     v: Success -> v,
///     e: Err1 -> return e,
///     e: Err2 -> return e,
/// }
/// ```
#[must_use]
pub fn desugar_propagate_union(
    inner: HirExpr,
    span: Span,
    success_ty: TypeId,
    error_types: &[TypeId],
    definitions: &mut DefinitionTable,
) -> HirExpr {
    let mut arms = Vec::with_capacity(error_types.len() + 1);

    // Error arms first (specific type narrowing)
    for &err_ty in error_types {
        let fresh_id = definitions.next_synthetic_id(span);
        arms.push(HirMatchArm {
            pattern: HirPattern::TypeNarrow { def_id: fresh_id, ty: err_ty },
            guard: None,
            body: HirExpr::new(
                HirExprKind::Return(Some(Box::new(HirExpr::new(
                    HirExprKind::Local(fresh_id),
                    err_ty,
                    span,
                )))),
                TypeId::ERROR, // return is Never
                span,
            ),
            span,
        });
    }

    // Success arm last (catch-all binding)
    let success_id = definitions.next_synthetic_id(span);
    arms.push(HirMatchArm {
        pattern: HirPattern::Binding { def_id: success_id },
        guard: None,
        body: HirExpr::new(HirExprKind::Local(success_id), success_ty, span),
        span,
    });

    HirExpr::new(
        HirExprKind::Match {
            scrutinee: Box::new(inner),
            arms,
        },
        success_ty,
        span,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_definitions() -> DefinitionTable {
        DefinitionTable::new()
    }

    #[test]
    fn desugar_coalesce_produces_match() {
        let mut defs = test_definitions();
        let lhs = HirExpr::new(HirExprKind::Literal(HirLiteral::Int(1)), TypeId::ERROR, Span::DUMMY);
        let rhs = HirExpr::new(HirExprKind::Literal(HirLiteral::Int(0)), TypeId::ERROR, Span::DUMMY);

        let result = desugar_coalesce(lhs, rhs, Span::DUMMY, TypeId::ERROR, &mut defs);
        assert!(matches!(result.kind, HirExprKind::Match { .. }));
        if let HirExprKind::Match { arms, .. } = &result.kind {
            assert_eq!(arms.len(), 2);
            // First arm should match Null
            assert!(matches!(arms[0].pattern, HirPattern::Literal(HirLiteral::Null)));
            // Second arm should be a Binding (not Wildcard)
            assert!(matches!(arms[1].pattern, HirPattern::Binding { .. }));
            // Second arm body should use Local (not Error)
            assert!(matches!(arms[1].body.kind, HirExprKind::Local(_)));
        }
    }

    #[test]
    fn desugar_optional_field_produces_match() {
        let mut defs = test_definitions();
        let base = HirExpr::new(HirExprKind::Literal(HirLiteral::Null), TypeId::ERROR, Span::DUMMY);
        let result = desugar_optional_field(base, spark_intern::sym("name"), Span::DUMMY, TypeId::ERROR, TypeId::ERROR, &mut defs);
        assert!(matches!(result.kind, HirExprKind::Match { .. }));
        if let HirExprKind::Match { arms, .. } = &result.kind {
            assert_eq!(arms.len(), 2);
            // First arm matches Null
            assert!(matches!(arms[0].pattern, HirPattern::Literal(HirLiteral::Null)));
            // Second arm is a Binding with field access in the body
            assert!(matches!(arms[1].pattern, HirPattern::Binding { .. }));
            assert!(matches!(arms[1].body.kind, HirExprKind::FieldAccess { .. }));
            // The field access base should use Local (not Error)
            if let HirExprKind::FieldAccess { base, .. } = &arms[1].body.kind {
                assert!(matches!(base.kind, HirExprKind::Local(_)));
            }
        }
    }

    #[test]
    fn desugar_propagate_optional_produces_match() {
        let mut defs = test_definitions();
        let inner = HirExpr::new(HirExprKind::Literal(HirLiteral::Int(1)), TypeId::ERROR, Span::DUMMY);
        let result = desugar_propagate_optional(inner, Span::DUMMY, TypeId::ERROR, TypeId::ERROR, &mut defs);
        assert!(matches!(result.kind, HirExprKind::Match { .. }));
        if let HirExprKind::Match { arms, .. } = &result.kind {
            assert_eq!(arms.len(), 2);
            // First arm matches Null
            assert!(matches!(arms[0].pattern, HirPattern::Literal(HirLiteral::Null)));
            // First arm returns Null
            assert!(matches!(arms[0].body.kind, HirExprKind::Return(_)));
            // Second arm is a Binding
            assert!(matches!(arms[1].pattern, HirPattern::Binding { .. }));
            // Second arm body uses Local
            assert!(matches!(arms[1].body.kind, HirExprKind::Local(_)));
        }
    }

    #[test]
    fn desugar_propagate_union_produces_typed_arms() {
        let mut defs = test_definitions();
        let inner = HirExpr::new(HirExprKind::Literal(HirLiteral::Int(1)), TypeId::ERROR, Span::DUMMY);
        let success_ty = TypeId::ERROR;
        let err1 = TypeId::ERROR;
        let result = desugar_propagate_union(inner, Span::DUMMY, success_ty, &[err1], &mut defs);
        assert!(matches!(result.kind, HirExprKind::Match { .. }));
        if let HirExprKind::Match { arms, .. } = &result.kind {
            assert_eq!(arms.len(), 2);
            // First arm is TypeNarrow for the error type
            assert!(matches!(arms[0].pattern, HirPattern::TypeNarrow { .. }));
            // First arm returns the error
            assert!(matches!(arms[0].body.kind, HirExprKind::Return(_)));
            // Last arm is Binding for success
            assert!(matches!(arms[1].pattern, HirPattern::Binding { .. }));
        }
    }

    #[test]
    fn synthetic_ids_are_unique() {
        let mut defs = test_definitions();
        let id1 = defs.next_synthetic_id(Span::DUMMY);
        let id2 = defs.next_synthetic_id(Span::DUMMY);
        assert_ne!(id1, id2);
    }
}
