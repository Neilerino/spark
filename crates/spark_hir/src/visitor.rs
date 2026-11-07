//! Visitor traits for HIR traversal.
//!
//! The visitor pattern allows traversing the HIR without duplicating the
//! recursion structure. Two traits are provided:
//!
//! - [`HirVisitor`] — immutable traversal (read-only analysis)
//! - [`HirMutVisitor`] — mutable traversal (in-place transformation)
//!
//! # Example
//!
//! ```ignore
//! use spark_hir::visitor::{HirVisitor, walk_expr};
//! use spark_hir::HirExpr;
//!
//! struct ExprCounter { count: usize }
//!
//! impl HirVisitor for ExprCounter {
//!     fn visit_expr(&mut self, expr: &HirExpr) {
//!         self.count += 1;
//!         walk_expr(self, expr);
//!     }
//! }
//! ```

use crate::hir::{
    Hir, HirExpr, HirExprKind, HirFunction, HirMatchArm, HirPattern, HirStatic, HirStmt,
};

// ─── Immutable visitor ──────────────────────────────────────────────────────

/// A visitor for traversing HIR nodes without modification.
///
/// The default implementations traverse children in order.
/// Override specific methods to customize traversal or collect data.
///
/// # Important
///
/// When overriding a method, call the corresponding `walk_*` function
/// to continue traversal to children, unless you want to stop traversal.
pub trait HirVisitor: Sized {
    /// Visit a complete HIR.
    fn visit_hir(&mut self, hir: &Hir) {
        walk_hir(self, hir);
    }

    /// Visit a function.
    fn visit_function(&mut self, func: &HirFunction) {
        walk_function(self, func);
    }

    /// Visit a module-level static/constant.
    fn visit_static(&mut self, s: &HirStatic) {
        walk_static(self, s);
    }

    /// Visit an expression.
    fn visit_expr(&mut self, expr: &HirExpr) {
        walk_expr(self, expr);
    }

    /// Visit a statement.
    fn visit_stmt(&mut self, stmt: &HirStmt) {
        walk_stmt(self, stmt);
    }

    /// Visit a pattern.
    fn visit_pattern(&mut self, pat: &HirPattern) {
        walk_pattern(self, pat);
    }

    /// Visit a match arm.
    fn visit_match_arm(&mut self, arm: &HirMatchArm) {
        walk_match_arm(self, arm);
    }
}

/// Walk the children of a complete HIR.
pub fn walk_hir<V: HirVisitor>(v: &mut V, hir: &Hir) {
    for func in hir.functions.values() {
        v.visit_function(func);
    }
    for s in hir.statics.values() {
        v.visit_static(s);
    }
}

/// Walk the children of a function.
pub fn walk_function<V: HirVisitor>(v: &mut V, func: &HirFunction) {
    v.visit_expr(&func.body);
}

/// Walk the children of a static/constant.
pub fn walk_static<V: HirVisitor>(v: &mut V, s: &HirStatic) {
    v.visit_expr(&s.value);
}

/// Walk the children of an expression.
#[allow(clippy::too_many_lines)]
pub fn walk_expr<V: HirVisitor>(v: &mut V, expr: &HirExpr) {
    match &expr.kind {
        // Leaf nodes — no children.
        HirExprKind::Literal(_)
        | HirExprKind::Local(_)
        | HirExprKind::MakeClosure { .. }
        | HirExprKind::Continue
        | HirExprKind::Error => {}

        // Single sub-expression.
        HirExprKind::UnaryOp { operand, .. } | HirExprKind::Borrow { operand, .. } => {
            v.visit_expr(operand);
        }
        HirExprKind::Deref(inner)
        | HirExprKind::Await(inner)
        | HirExprKind::Spawn(inner)
        | HirExprKind::Loop { body: inner }
        | HirExprKind::Is { expr: inner, .. }
        | HirExprKind::As { expr: inner, .. } => {
            v.visit_expr(inner);
        }
        HirExprKind::Return(opt) | HirExprKind::Break(opt) => {
            if let Some(inner) = opt {
                v.visit_expr(inner);
            }
        }

        // Two sub-expressions.
        HirExprKind::BinaryOp { lhs, rhs, .. } => {
            v.visit_expr(lhs);
            v.visit_expr(rhs);
        }
        HirExprKind::FieldAccess { base, .. } => {
            v.visit_expr(base);
        }
        HirExprKind::Index { base, index } | HirExprKind::CollectionLookup { base, index } => {
            v.visit_expr(base);
            v.visit_expr(index);
        }

        // Calls.
        HirExprKind::Call { callee, args } => {
            v.visit_expr(callee);
            for arg in args {
                v.visit_expr(arg);
            }
        }
        HirExprKind::MethodCall { receiver, args, .. } => {
            v.visit_expr(receiver);
            for arg in args {
                v.visit_expr(arg);
            }
        }

        // Constructors / collections.
        HirExprKind::StructInit { fields, .. } => {
            for (_, val) in fields {
                v.visit_expr(val);
            }
        }
        HirExprKind::EnumVariant { fields, .. } => {
            for f in fields {
                v.visit_expr(f);
            }
        }
        HirExprKind::ListLit(elems) | HirExprKind::SetLit(elems) | HirExprKind::TupleLit(elems) => {
            for e in elems {
                v.visit_expr(e);
            }
        }
        HirExprKind::ListRange {
            start, end, step, ..
        } => {
            v.visit_expr(start);
            v.visit_expr(end);
            if let Some(s) = step {
                v.visit_expr(s);
            }
        }
        HirExprKind::MapLit(pairs) => {
            for (k, val) in pairs {
                v.visit_expr(k);
                v.visit_expr(val);
            }
        }

        // Control flow.
        HirExprKind::Block {
            stmts,
            expr: trailing,
        } => {
            for stmt in stmts {
                v.visit_stmt(stmt);
            }
            if let Some(e) = trailing {
                v.visit_expr(e);
            }
        }
        HirExprKind::If { cond, then_, else_ } => {
            v.visit_expr(cond);
            v.visit_expr(then_);
            if let Some(e) = else_ {
                v.visit_expr(e);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            v.visit_expr(scrutinee);
            for arm in arms {
                v.visit_match_arm(arm);
            }
        }

        // Closures.
        HirExprKind::Lambda { body, .. } => {
            v.visit_expr(body);
        }

        // String interpolation.
        HirExprKind::StringInterp { parts } => {
            for part in parts {
                v.visit_expr(part);
            }
        }

        // Parallel iteration.
        HirExprKind::ParallelForEach { list, callback, .. }
        | HirExprKind::ParallelMap { list, callback, .. } => {
            v.visit_expr(list);
            v.visit_expr(callback);
        }
        HirExprKind::ParallelMapReduce {
            list,
            mapper,
            combiner,
            initial,
            ..
        } => {
            v.visit_expr(list);
            v.visit_expr(mapper);
            v.visit_expr(combiner);
            v.visit_expr(initial);
        }
    }
}

/// Walk the children of a statement.
pub fn walk_stmt<V: HirVisitor>(v: &mut V, stmt: &HirStmt) {
    match stmt {
        HirStmt::Let { value, .. } => v.visit_expr(value),
        HirStmt::Assign { target, value } => {
            v.visit_expr(target);
            v.visit_expr(value);
        }
        HirStmt::Expr(e) => v.visit_expr(e),
    }
}

/// Walk the children of a pattern.
pub fn walk_pattern<V: HirVisitor>(v: &mut V, pat: &HirPattern) {
    match pat {
        HirPattern::Wildcard
        | HirPattern::Binding { .. }
        | HirPattern::Literal(_)
        | HirPattern::TypeNarrow { .. }
        | HirPattern::Error => {}
        HirPattern::Variant { fields, .. } => {
            for f in fields {
                v.visit_pattern(f);
            }
        }
        HirPattern::Struct { fields, .. } => {
            for (_, p) in fields {
                v.visit_pattern(p);
            }
        }
        HirPattern::Tuple(elems) => {
            for e in elems {
                v.visit_pattern(e);
            }
        }
        HirPattern::Or(pats) => {
            for p in pats {
                v.visit_pattern(p);
            }
        }
    }
}

/// Walk the children of a match arm.
pub fn walk_match_arm<V: HirVisitor>(v: &mut V, arm: &HirMatchArm) {
    v.visit_pattern(&arm.pattern);
    if let Some(guard) = &arm.guard {
        v.visit_expr(guard);
    }
    v.visit_expr(&arm.body);
}

// ─── Mutable visitor ────────────────────────────────────────────────────────

/// A visitor for traversing and transforming HIR nodes in place.
///
/// The default implementations traverse children in order.
/// Override specific methods to perform transformations.
///
/// # Important
///
/// When overriding a method, call the corresponding `walk_*_mut` function
/// to continue traversal to children, unless you want to stop traversal.
pub trait HirMutVisitor: Sized {
    /// Visit a complete HIR (mutable).
    fn visit_hir_mut(&mut self, hir: &mut Hir) {
        walk_hir_mut(self, hir);
    }

    /// Visit a function (mutable).
    fn visit_function_mut(&mut self, func: &mut HirFunction) {
        walk_function_mut(self, func);
    }

    /// Visit a module-level static/constant (mutable).
    fn visit_static_mut(&mut self, s: &mut HirStatic) {
        walk_static_mut(self, s);
    }

    /// Visit an expression (mutable).
    fn visit_expr_mut(&mut self, expr: &mut HirExpr) {
        walk_expr_mut(self, expr);
    }

    /// Visit a statement (mutable).
    fn visit_stmt_mut(&mut self, stmt: &mut HirStmt) {
        walk_stmt_mut(self, stmt);
    }

    /// Visit a pattern (mutable).
    fn visit_pattern_mut(&mut self, pat: &mut HirPattern) {
        walk_pattern_mut(self, pat);
    }

    /// Visit a match arm (mutable).
    fn visit_match_arm_mut(&mut self, arm: &mut HirMatchArm) {
        walk_match_arm_mut(self, arm);
    }
}

/// Walk the children of a complete HIR (mutable).
pub fn walk_hir_mut<V: HirMutVisitor>(v: &mut V, hir: &mut Hir) {
    for func in hir.functions.values_mut() {
        v.visit_function_mut(func);
    }
    for s in hir.statics.values_mut() {
        v.visit_static_mut(s);
    }
}

/// Walk the children of a function (mutable).
pub fn walk_function_mut<V: HirMutVisitor>(v: &mut V, func: &mut HirFunction) {
    v.visit_expr_mut(&mut func.body);
}

/// Walk the children of a static/constant (mutable).
pub fn walk_static_mut<V: HirMutVisitor>(v: &mut V, s: &mut HirStatic) {
    v.visit_expr_mut(&mut s.value);
}

/// Walk the children of an expression (mutable).
#[allow(clippy::too_many_lines)]
pub fn walk_expr_mut<V: HirMutVisitor>(v: &mut V, expr: &mut HirExpr) {
    match &mut expr.kind {
        // Leaf nodes — no children.
        HirExprKind::Literal(_)
        | HirExprKind::Local(_)
        | HirExprKind::MakeClosure { .. }
        | HirExprKind::Continue
        | HirExprKind::Error => {}

        // Single sub-expression.
        HirExprKind::UnaryOp { operand, .. } | HirExprKind::Borrow { operand, .. } => {
            v.visit_expr_mut(operand);
        }
        HirExprKind::Deref(inner)
        | HirExprKind::Await(inner)
        | HirExprKind::Spawn(inner)
        | HirExprKind::Loop { body: inner }
        | HirExprKind::Is { expr: inner, .. }
        | HirExprKind::As { expr: inner, .. } => {
            v.visit_expr_mut(inner);
        }
        HirExprKind::Return(opt) | HirExprKind::Break(opt) => {
            if let Some(inner) = opt {
                v.visit_expr_mut(inner);
            }
        }

        // Two sub-expressions.
        HirExprKind::BinaryOp { lhs, rhs, .. } => {
            v.visit_expr_mut(lhs);
            v.visit_expr_mut(rhs);
        }
        HirExprKind::FieldAccess { base, .. } => {
            v.visit_expr_mut(base);
        }
        HirExprKind::Index { base, index } | HirExprKind::CollectionLookup { base, index } => {
            v.visit_expr_mut(base);
            v.visit_expr_mut(index);
        }

        // Calls.
        HirExprKind::Call { callee, args } => {
            v.visit_expr_mut(callee);
            for arg in args {
                v.visit_expr_mut(arg);
            }
        }
        HirExprKind::MethodCall { receiver, args, .. } => {
            v.visit_expr_mut(receiver);
            for arg in args {
                v.visit_expr_mut(arg);
            }
        }

        // Constructors / collections.
        HirExprKind::StructInit { fields, .. } => {
            for (_, val) in fields {
                v.visit_expr_mut(val);
            }
        }
        HirExprKind::EnumVariant { fields, .. } => {
            for f in fields {
                v.visit_expr_mut(f);
            }
        }
        HirExprKind::ListLit(elems) | HirExprKind::SetLit(elems) | HirExprKind::TupleLit(elems) => {
            for e in elems {
                v.visit_expr_mut(e);
            }
        }
        HirExprKind::ListRange {
            start, end, step, ..
        } => {
            v.visit_expr_mut(start);
            v.visit_expr_mut(end);
            if let Some(s) = step {
                v.visit_expr_mut(s);
            }
        }
        HirExprKind::MapLit(pairs) => {
            for (k, val) in pairs {
                v.visit_expr_mut(k);
                v.visit_expr_mut(val);
            }
        }

        // Control flow.
        HirExprKind::Block {
            stmts,
            expr: trailing,
        } => {
            for stmt in stmts {
                v.visit_stmt_mut(stmt);
            }
            if let Some(e) = trailing {
                v.visit_expr_mut(e);
            }
        }
        HirExprKind::If { cond, then_, else_ } => {
            v.visit_expr_mut(cond);
            v.visit_expr_mut(then_);
            if let Some(e) = else_ {
                v.visit_expr_mut(e);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            v.visit_expr_mut(scrutinee);
            for arm in arms {
                v.visit_match_arm_mut(arm);
            }
        }

        // Closures.
        HirExprKind::Lambda { body, .. } => {
            v.visit_expr_mut(body);
        }

        // String interpolation.
        HirExprKind::StringInterp { parts } => {
            for part in parts {
                v.visit_expr_mut(part);
            }
        }

        // Parallel iteration.
        HirExprKind::ParallelForEach { list, callback, .. }
        | HirExprKind::ParallelMap { list, callback, .. } => {
            v.visit_expr_mut(list);
            v.visit_expr_mut(callback);
        }
        HirExprKind::ParallelMapReduce {
            list,
            mapper,
            combiner,
            initial,
            ..
        } => {
            v.visit_expr_mut(list);
            v.visit_expr_mut(mapper);
            v.visit_expr_mut(combiner);
            v.visit_expr_mut(initial);
        }
    }
}

/// Walk the children of a statement (mutable).
pub fn walk_stmt_mut<V: HirMutVisitor>(v: &mut V, stmt: &mut HirStmt) {
    match stmt {
        HirStmt::Let { value, .. } => v.visit_expr_mut(value),
        HirStmt::Assign { target, value } => {
            v.visit_expr_mut(target);
            v.visit_expr_mut(value);
        }
        HirStmt::Expr(e) => v.visit_expr_mut(e),
    }
}

/// Walk the children of a pattern (mutable).
pub fn walk_pattern_mut<V: HirMutVisitor>(v: &mut V, pat: &mut HirPattern) {
    match pat {
        HirPattern::Wildcard
        | HirPattern::Binding { .. }
        | HirPattern::Literal(_)
        | HirPattern::TypeNarrow { .. }
        | HirPattern::Error => {}
        HirPattern::Variant { fields, .. } => {
            for f in fields {
                v.visit_pattern_mut(f);
            }
        }
        HirPattern::Struct { fields, .. } => {
            for (_, p) in fields {
                v.visit_pattern_mut(p);
            }
        }
        HirPattern::Tuple(elems) => {
            for e in elems {
                v.visit_pattern_mut(e);
            }
        }
        HirPattern::Or(pats) => {
            for p in pats {
                v.visit_pattern_mut(p);
            }
        }
    }
}

/// Walk the children of a match arm (mutable).
pub fn walk_match_arm_mut<V: HirMutVisitor>(v: &mut V, arm: &mut HirMatchArm) {
    v.visit_pattern_mut(&mut arm.pattern);
    if let Some(guard) = &mut arm.guard {
        v.visit_expr_mut(guard);
    }
    v.visit_expr_mut(&mut arm.body);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hir::{HirExpr, HirExprKind, HirFunction, HirLiteral, HirStatic};
    use spark_resolve::DefId;
    use spark_span::Span;
    use spark_types::TypeId;
    use std::collections::HashMap;

    fn int_expr(n: i64) -> HirExpr {
        HirExpr::new(
            HirExprKind::Literal(HirLiteral::Int(n)),
            TypeId::ERROR,
            Span::DUMMY,
        )
    }

    // ── Immutable visitor tests ─────────────────────────────────────────

    struct ExprCounter {
        count: usize,
    }

    impl HirVisitor for ExprCounter {
        fn visit_expr(&mut self, expr: &HirExpr) {
            self.count += 1;
            walk_expr(self, expr);
        }
    }

    #[test]
    fn visitor_counts_leaf_expr() {
        let expr = int_expr(42);
        let mut counter = ExprCounter { count: 0 };
        counter.visit_expr(&expr);
        assert_eq!(counter.count, 1);
    }

    #[test]
    fn visitor_counts_binary_expr() {
        use spark_ast::BinaryOp;
        let expr = HirExpr::new(
            HirExprKind::BinaryOp {
                op: BinaryOp::Add,
                lhs: Box::new(int_expr(1)),
                rhs: Box::new(int_expr(2)),
            },
            TypeId::ERROR,
            Span::DUMMY,
        );
        let mut counter = ExprCounter { count: 0 };
        counter.visit_expr(&expr);
        // binary + 2 literals = 3
        assert_eq!(counter.count, 3);
    }

    #[test]
    fn visitor_walks_block_stmts() {
        let expr = HirExpr::new(
            HirExprKind::Block {
                stmts: vec![HirStmt::Expr(int_expr(1)), HirStmt::Expr(int_expr(2))],
                expr: Some(Box::new(int_expr(3))),
            },
            TypeId::ERROR,
            Span::DUMMY,
        );
        let mut counter = ExprCounter { count: 0 };
        counter.visit_expr(&expr);
        // block + 3 int literals = 4
        assert_eq!(counter.count, 4);
    }

    #[test]
    fn visitor_walks_match_arms() {
        let arm = HirMatchArm {
            pattern: HirPattern::Wildcard,
            guard: Some(int_expr(10)),
            body: int_expr(20),
            span: Span::DUMMY,
        };
        let expr = HirExpr::new(
            HirExprKind::Match {
                scrutinee: Box::new(int_expr(0)),
                arms: vec![arm],
            },
            TypeId::ERROR,
            Span::DUMMY,
        );
        let mut counter = ExprCounter { count: 0 };
        counter.visit_expr(&expr);
        // match + scrutinee + guard + body = 4
        assert_eq!(counter.count, 4);
    }

    #[test]
    fn visitor_walks_hir() {
        let def_id = DefId::new(0);
        let func = HirFunction {
            def_id,
            params: vec![],
            body: int_expr(42),
            ret_type: TypeId::ERROR,
            is_async: false,
            span: Span::DUMMY,
            captures: vec![],
        };
        let hir = Hir {
            functions: HashMap::from([(def_id, func)]),
            statics: HashMap::new(),
        };
        let mut counter = ExprCounter { count: 0 };
        counter.visit_hir(&hir);
        assert_eq!(counter.count, 1);
    }

    #[test]
    fn visitor_walks_static() {
        let def_id = DefId::new(0);
        let s = HirStatic {
            def_id,
            ty: TypeId::ERROR,
            value: int_expr(99),
            span: Span::DUMMY,
        };
        let hir = Hir {
            functions: HashMap::new(),
            statics: HashMap::from([(def_id, s)]),
        };
        let mut counter = ExprCounter { count: 0 };
        counter.visit_hir(&hir);
        assert_eq!(counter.count, 1);
    }

    #[test]
    fn visitor_override_stops_recursion() {
        /// A visitor that only counts top-level expressions (no recursion).
        struct ShallowCounter {
            count: usize,
        }
        impl HirVisitor for ShallowCounter {
            fn visit_expr(&mut self, _expr: &HirExpr) {
                self.count += 1;
                // Deliberately do NOT call walk_expr — stops recursion.
            }
        }

        use spark_ast::BinaryOp;
        let expr = HirExpr::new(
            HirExprKind::BinaryOp {
                op: BinaryOp::Add,
                lhs: Box::new(int_expr(1)),
                rhs: Box::new(int_expr(2)),
            },
            TypeId::ERROR,
            Span::DUMMY,
        );
        let mut counter = ShallowCounter { count: 0 };
        counter.visit_expr(&expr);
        // Only counted the top-level binary, not the children.
        assert_eq!(counter.count, 1);
    }

    // ── Mutable visitor tests ───────────────────────────────────────────

    #[test]
    fn mut_visitor_transforms_expr() {
        /// A visitor that replaces all Int literals with 0.
        struct Zeroing;
        impl HirMutVisitor for Zeroing {
            fn visit_expr_mut(&mut self, expr: &mut HirExpr) {
                if matches!(&expr.kind, HirExprKind::Literal(HirLiteral::Int(_))) {
                    expr.kind = HirExprKind::Literal(HirLiteral::Int(0));
                }
                walk_expr_mut(self, expr);
            }
        }

        use spark_ast::BinaryOp;
        let mut expr = HirExpr::new(
            HirExprKind::BinaryOp {
                op: BinaryOp::Add,
                lhs: Box::new(int_expr(1)),
                rhs: Box::new(int_expr(2)),
            },
            TypeId::ERROR,
            Span::DUMMY,
        );
        Zeroing.visit_expr_mut(&mut expr);

        match &expr.kind {
            HirExprKind::BinaryOp { lhs, rhs, .. } => {
                assert!(matches!(
                    &lhs.kind,
                    HirExprKind::Literal(HirLiteral::Int(0))
                ));
                assert!(matches!(
                    &rhs.kind,
                    HirExprKind::Literal(HirLiteral::Int(0))
                ));
            }
            other => panic!("Expected BinaryOp, got: {other:?}"),
        }
    }

    #[test]
    fn mut_visitor_walks_stmts() {
        struct StmtCounter {
            count: usize,
        }
        impl HirMutVisitor for StmtCounter {
            fn visit_stmt_mut(&mut self, stmt: &mut HirStmt) {
                self.count += 1;
                walk_stmt_mut(self, stmt);
            }
        }

        let mut expr = HirExpr::new(
            HirExprKind::Block {
                stmts: vec![HirStmt::Expr(int_expr(1)), HirStmt::Expr(int_expr(2))],
                expr: None,
            },
            TypeId::ERROR,
            Span::DUMMY,
        );
        let mut counter = StmtCounter { count: 0 };
        counter.visit_expr_mut(&mut expr);
        assert_eq!(counter.count, 2);
    }

    #[test]
    fn mut_visitor_walks_function() {
        struct ExprMutCounter {
            count: usize,
        }
        impl HirMutVisitor for ExprMutCounter {
            fn visit_expr_mut(&mut self, expr: &mut HirExpr) {
                self.count += 1;
                walk_expr_mut(self, expr);
            }
        }

        let def_id = DefId::new(0);
        let mut func = HirFunction {
            def_id,
            params: vec![],
            body: int_expr(42),
            ret_type: TypeId::ERROR,
            is_async: false,
            span: Span::DUMMY,
            captures: vec![],
        };
        let mut counter = ExprMutCounter { count: 0 };
        counter.visit_function_mut(&mut func);
        assert_eq!(counter.count, 1);
    }
}
