//! Desugaring transforms.
//!
//! Converts syntactic sugar into core HIR constructs:
//! - `a ?? b` → `match a { v -> v, Null -> b }`
//! - `a?.f` → `match a { v -> Some(v.f), Null -> Null }`
//! - `e!` → `match e { v -> v, err -> return err }`
//! - `list each (x): Void { ... }` → index loop
//! - `list each via (x): U { ... }` → index loop building result list

use spark_ast::BinaryOp;
use spark_intern::Symbol;
use spark_resolve::DefinitionTable;
use spark_span::Span;
use spark_types::TypeId;

use crate::hir::{HirExpr, HirExprKind, HirLiteral, HirMatchArm, HirPattern, HirStmt};

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

/// Desugars `iterable each (param): Void { body }` into an index loop.
///
/// ```text
/// {
///     let __iter = iterable
///     let __idx: Int! = 0
///     let __len: Int = __iter.len()
///     loop {
///         if __idx >= __len { break }
///         let param: T = __iter[__idx]
///         body
///         __idx = __idx + 1
///     }
/// }
/// ```
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn desugar_each(
    iterable: HirExpr,
    body: HirExpr,
    param_def_id: spark_resolve::DefId,
    elem_ty: TypeId,
    int_ty: TypeId,
    bool_ty: TypeId,
    void_ty: TypeId,
    span: Span,
    definitions: &mut DefinitionTable,
) -> HirExpr {
    let (stmts, _) = build_each_loop(
        iterable, body, param_def_id, elem_ty, int_ty, bool_ty, void_ty,
        span, definitions, false,
    );

    HirExpr::new(
        HirExprKind::Block { stmts, expr: None },
        void_ty,
        span,
    )
}

/// Desugars `iterable each via (param): U { body }` into an index loop that
/// builds a result list.
///
/// ```text
/// {
///     let __iter = iterable
///     let __idx: Int! = 0
///     let __len: Int = __iter.len()
///     let __result: List[U]! = []
///     loop {
///         if __idx >= __len { break }
///         let param: T = __iter[__idx]
///         let __elem: U = body
///         __result.append(__elem)
///         __idx = __idx + 1
///     }
///     __result
/// }
/// ```
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn desugar_each_via(
    iterable: HirExpr,
    body: HirExpr,
    param_def_id: spark_resolve::DefId,
    elem_ty: TypeId,
    _result_elem_ty: TypeId,
    result_list_ty: TypeId,
    int_ty: TypeId,
    bool_ty: TypeId,
    void_ty: TypeId,
    span: Span,
    definitions: &mut DefinitionTable,
) -> HirExpr {
    let (stmts, result_id) = build_each_loop(
        iterable, body, param_def_id, elem_ty, int_ty, bool_ty, void_ty,
        span, definitions, true,
    );

    // The result_id is the __result local (always set when via=true)
    // Safety: build_each_loop always returns Some when via=true.
    let Some(result_def_id) = result_id else {
        return HirExpr::new(HirExprKind::Error, result_list_ty, span);
    };

    // Update __result's type: it's actually List[U], not void
    // We need to rebuild stmts with the correct type on __result
    let mut final_stmts = Vec::with_capacity(stmts.len());
    for stmt in stmts {
        match stmt {
            HirStmt::Let { def_id, mutable, value } if def_id == result_def_id => {
                // Replace the empty list literal's type with result_list_ty
                let fixed_value = HirExpr::new(value.kind, result_list_ty, value.span);
                final_stmts.push(HirStmt::Let { def_id, mutable, value: fixed_value });
            }
            other => final_stmts.push(other),
        }
    }

    // Trailing expression: __result
    let trailing = HirExpr::new(
        HirExprKind::Local(result_def_id),
        result_list_ty,
        span,
    );

    HirExpr::new(
        HirExprKind::Block {
            stmts: final_stmts,
            expr: Some(Box::new(trailing)),
        },
        result_list_ty,
        span,
    )
}

/// Builds the shared loop structure for both `each` and `each via`.
///
/// Returns the list of statements and, if `via` is true, the `DefId` of the
/// `__result` local.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn build_each_loop(
    iterable: HirExpr,
    body: HirExpr,
    param_def_id: spark_resolve::DefId,
    elem_ty: TypeId,
    int_ty: TypeId,
    bool_ty: TypeId,
    void_ty: TypeId,
    span: Span,
    definitions: &mut DefinitionTable,
    via: bool,
) -> (Vec<HirStmt>, Option<spark_resolve::DefId>) {
    let iter_id = definitions.next_synthetic_id(span);
    let idx_id = definitions.next_synthetic_id(span);
    let len_id = definitions.next_synthetic_id(span);

    let iter_ty = iterable.ty;

    // let __iter = iterable
    let iter_let = HirStmt::Let {
        def_id: iter_id,
        mutable: false,
        value: iterable,
    };

    // let __idx: Int! = 0
    let idx_let = HirStmt::Let {
        def_id: idx_id,
        mutable: true,
        value: HirExpr::new(HirExprKind::Literal(HirLiteral::Int(0)), int_ty, span),
    };

    // let __len: Int = __iter.len()
    let len_let = HirStmt::Let {
        def_id: len_id,
        mutable: false,
        value: HirExpr::new(
            HirExprKind::MethodCall {
                receiver: Box::new(HirExpr::new(HirExprKind::Local(iter_id), iter_ty, span)),
                method: spark_intern::kw::LEN,
                args: vec![],
            },
            int_ty,
            span,
        ),
    };

    // __result local (only for `each via`)
    let result_id = if via {
        Some(definitions.next_synthetic_id(span))
    } else {
        None
    };

    // if __idx >= __len { break }
    let guard = HirExpr::new(
        HirExprKind::If {
            cond: Box::new(HirExpr::new(
                HirExprKind::BinaryOp {
                    op: BinaryOp::Ge,
                    lhs: Box::new(HirExpr::new(HirExprKind::Local(idx_id), int_ty, span)),
                    rhs: Box::new(HirExpr::new(HirExprKind::Local(len_id), int_ty, span)),
                },
                bool_ty,
                span,
            )),
            then_: Box::new(HirExpr::new(HirExprKind::Break(None), void_ty, span)),
            else_: None,
        },
        void_ty,
        span,
    );

    // let param: T = __iter[__idx]
    let param_let = HirStmt::Let {
        def_id: param_def_id,
        mutable: false,
        value: HirExpr::new(
            HirExprKind::Index {
                base: Box::new(HirExpr::new(HirExprKind::Local(iter_id), iter_ty, span)),
                index: Box::new(HirExpr::new(HirExprKind::Local(idx_id), int_ty, span)),
            },
            elem_ty,
            span,
        ),
    };

    // __idx = __idx + 1
    let increment = HirStmt::Assign {
        target: HirExpr::new(HirExprKind::Local(idx_id), int_ty, span),
        value: HirExpr::new(
            HirExprKind::BinaryOp {
                op: BinaryOp::Add,
                lhs: Box::new(HirExpr::new(HirExprKind::Local(idx_id), int_ty, span)),
                rhs: Box::new(HirExpr::new(HirExprKind::Literal(HirLiteral::Int(1)), int_ty, span)),
            },
            int_ty,
            span,
        ),
    };

    // Build loop body statements
    let mut loop_stmts: Vec<HirStmt> = vec![
        HirStmt::Expr(guard),
        param_let,
    ];

    if let Some(rid) = result_id {
        // each via: let __elem = body; __result.append(__elem)
        let elem_id = definitions.next_synthetic_id(span);
        let body_ty = body.ty;
        loop_stmts.push(HirStmt::Let {
            def_id: elem_id,
            mutable: false,
            value: body,
        });
        loop_stmts.push(HirStmt::Expr(HirExpr::new(
            HirExprKind::MethodCall {
                receiver: Box::new(HirExpr::new(HirExprKind::Local(rid), void_ty, span)),
                method: spark_intern::kw::APPEND,
                args: vec![HirExpr::new(HirExprKind::Local(elem_id), body_ty, span)],
            },
            void_ty,
            span,
        )));
    } else {
        // each: just the body as a statement
        loop_stmts.push(HirStmt::Expr(body));
    }

    loop_stmts.push(increment);

    let loop_body = HirExpr::new(
        HirExprKind::Block { stmts: loop_stmts, expr: None },
        void_ty,
        span,
    );

    let loop_expr = HirStmt::Expr(HirExpr::new(
        HirExprKind::Loop { body: Box::new(loop_body) },
        void_ty,
        span,
    ));

    // Build outer block statements
    let mut stmts = vec![iter_let, idx_let, len_let];

    if let Some(rid) = result_id {
        // let __result: List[U]! = []
        stmts.push(HirStmt::Let {
            def_id: rid,
            mutable: true,
            value: HirExpr::new(HirExprKind::ListLit(vec![]), void_ty, span),
        });
    }

    stmts.push(loop_expr);

    (stmts, result_id)
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
