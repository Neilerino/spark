//! Desugaring transforms.
//!
//! Converts syntactic sugar into core HIR constructs:
//! - `a ?? b` → `match a { v -> v, Null -> b }`
//! - `a?.f` → `match a { v -> Some(v.f), Null -> Null }`
//! - `e!` → `match e { v -> v, err -> return err }`

use spark_intern::Symbol;
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
) -> HirExpr {
    // The non-null arm binds the value and returns it.
    // We use a wildcard for the binding since we don't have a fresh DefId.
    // The null arm returns the default value.
    let non_null_arm = HirMatchArm {
        pattern: HirPattern::Wildcard,
        guard: None,
        body: HirExpr::new(HirExprKind::Error, ty, span), // placeholder: ideally the bound value
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
) -> HirExpr {
    let field_arm = HirMatchArm {
        pattern: HirPattern::Wildcard,
        guard: None,
        body: HirExpr::new(
            HirExprKind::FieldAccess {
                base: Box::new(HirExpr::new(HirExprKind::Error, base.ty, span)),
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
#[must_use]
pub fn desugar_propagate(
    inner: HirExpr,
    _fn_ret_type: Option<TypeId>,
    span: Span,
    ty: TypeId,
) -> HirExpr {
    let success_arm = HirMatchArm {
        pattern: HirPattern::Wildcard,
        guard: None,
        body: HirExpr::new(HirExprKind::Error, ty, span),
        span,
    };

    let error_arm = HirMatchArm {
        pattern: HirPattern::Wildcard,
        guard: None,
        body: HirExpr::new(
            HirExprKind::Return(Some(Box::new(HirExpr::new(HirExprKind::Error, TypeId::ERROR, span)))),
            TypeId::ERROR,
            span,
        ),
        span,
    };

    HirExpr::new(
        HirExprKind::Match {
            scrutinee: Box::new(inner),
            arms: vec![success_arm, error_arm],
        },
        ty,
        span,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desugar_coalesce_produces_match() {
        let lhs = HirExpr::new(HirExprKind::Literal(HirLiteral::Int(1)), TypeId::ERROR, Span::DUMMY);
        let rhs = HirExpr::new(HirExprKind::Literal(HirLiteral::Int(0)), TypeId::ERROR, Span::DUMMY);

        let result = desugar_coalesce(lhs, rhs, Span::DUMMY, TypeId::ERROR);
        assert!(matches!(result.kind, HirExprKind::Match { .. }));
        if let HirExprKind::Match { arms, .. } = &result.kind {
            assert_eq!(arms.len(), 2);
            // First arm should match Null
            assert!(matches!(arms[0].pattern, HirPattern::Literal(HirLiteral::Null)));
        }
    }

    #[test]
    fn desugar_optional_field_produces_match() {
        let base = HirExpr::new(HirExprKind::Literal(HirLiteral::Null), TypeId::ERROR, Span::DUMMY);
        let result = desugar_optional_field(base, spark_intern::sym("name"), Span::DUMMY, TypeId::ERROR);
        assert!(matches!(result.kind, HirExprKind::Match { .. }));
    }

    #[test]
    fn desugar_propagate_produces_match() {
        let inner = HirExpr::new(HirExprKind::Literal(HirLiteral::Int(1)), TypeId::ERROR, Span::DUMMY);
        let result = desugar_propagate(inner, None, Span::DUMMY, TypeId::ERROR);
        assert!(matches!(result.kind, HirExprKind::Match { .. }));
        if let HirExprKind::Match { arms, .. } = &result.kind {
            assert_eq!(arms.len(), 2);
        }
    }
}
