//! Lambda lifting pass.
//!
//! Extracts non-capturing lambda expressions into top-level [`HirFunction`]s.
//! Each `HirExprKind::Lambda` is replaced with `HirExprKind::Local(def_id)`
//! pointing at the lifted function, so downstream phases (MIR, codegen) treat
//! it as a regular function reference.
//!
//! Capturing closures are **not** supported in this pass — they require
//! environment allocation and a modified calling convention (Phase 2).

use spark_diagnostics::{Diagnostic, Diagnostics, Label};
use spark_modules::ModulePath;
use spark_resolve::{DefId, DefinitionInfo, DefinitionKind, DefinitionTable, Visibility};
use spark_types::{Ty, TypeId, TypeTable};

use crate::hir::{Hir, HirExpr, HirExprKind, HirFunction, HirMatchArm, HirStmt};

/// Lifts all lambda expressions in the HIR into top-level functions.
///
/// After this pass, no `HirExprKind::Lambda` nodes remain in any function body.
/// Each lambda is replaced by `HirExprKind::Local(def_id)` where `def_id`
/// refers to a newly created `HirFunction` inserted into `hir.functions`.
pub fn lift_lambdas(
    hir: &mut Hir,
    definitions: &mut DefinitionTable,
    table: &mut TypeTable,
    diagnostics: &mut Diagnostics,
) {
    // Collect existing function DefIds to avoid mutating the map while iterating.
    let func_ids: Vec<DefId> = hir.functions.keys().copied().collect();
    let mut lifted = Vec::new();
    let mut counter: u32 = 0;

    for def_id in func_ids {
        if let Some(func) = hir.functions.get_mut(&def_id) {
            walk_expr(&mut func.body, &mut lifted, definitions, table, &mut counter, diagnostics);
        }
    }

    for func in lifted {
        hir.functions.insert(func.def_id, func);
    }
}

/// Recursively walks an HIR expression, lifting any `Lambda` nodes encountered.
#[allow(clippy::too_many_lines)]
fn walk_expr(
    expr: &mut HirExpr,
    lifted: &mut Vec<HirFunction>,
    definitions: &mut DefinitionTable,
    table: &mut TypeTable,
    counter: &mut u32,
    diagnostics: &mut Diagnostics,
) {
    match &mut expr.kind {
        // === The case we care about: Lambda ===
        HirExprKind::Lambda { .. } => {
            // Take ownership of the lambda contents.
            let old_kind = std::mem::replace(&mut expr.kind, HirExprKind::Error);
            if let HirExprKind::Lambda { params, mut body } = old_kind {
                // Depth-first: lift any nested lambdas inside this body first.
                walk_expr(&mut body, lifted, definitions, table, counter, diagnostics);

                // Allocate a synthetic DefId for the lifted function.
                let name = spark_intern::sym(&format!("__lambda_{counter}"));
                *counter += 1;

                let def_id = definitions.insert(DefinitionInfo::new(
                    DefinitionKind::Lambda,
                    name,
                    expr.span,
                    Visibility::Private,
                    ModulePath::root(),
                ));

                // Extract return type from the function type.
                let ret_type = extract_ret_type(expr.ty, table);

                // Register the function type for this DefId in the type table.
                table.set_def_type(def_id, expr.ty);

                // Collect free variables (captures) from the lambda body.
                let captures = collect_captures(&body, &params, definitions);

                // Create the lifted function.
                lifted.push(HirFunction {
                    def_id,
                    params,
                    body: *body,
                    ret_type,
                    is_async: false,
                    span: expr.span,
                    captures: captures.clone(),
                });

                // Replace the lambda with the appropriate node:
                // - No captures: just a function reference (Local)
                // - Has captures: MakeClosure node (codegen allocates env)
                if captures.is_empty() {
                    expr.kind = HirExprKind::Local(def_id);
                } else {
                    expr.kind = HirExprKind::MakeClosure {
                        func_def_id: def_id,
                        captures,
                    };
                }
            }
        }

        // === Recursion into all sub-expressions ===

        // Leaf nodes — no children to walk.
        HirExprKind::Literal(_)
        | HirExprKind::Local(_)
        | HirExprKind::MakeClosure { .. }
        | HirExprKind::Continue
        | HirExprKind::Error => {}

        // Single sub-expression.
        HirExprKind::UnaryOp { operand, .. }
        | HirExprKind::Borrow { operand, .. } => {
            walk_expr(operand, lifted, definitions, table, counter, diagnostics);
        }
        HirExprKind::Deref(inner)
        | HirExprKind::Await(inner)
        | HirExprKind::Spawn(inner)
        | HirExprKind::Loop { body: inner }
        | HirExprKind::Is { expr: inner, .. }
        | HirExprKind::As { expr: inner, .. } => {
            walk_expr(inner, lifted, definitions, table, counter, diagnostics);
        }
        HirExprKind::Return(opt) | HirExprKind::Break(opt) => {
            if let Some(inner) = opt {
                walk_expr(inner, lifted, definitions, table, counter, diagnostics);
            }
        }

        // Two sub-expressions.
        HirExprKind::BinaryOp { lhs, rhs, .. } => {
            walk_expr(lhs, lifted, definitions, table, counter, diagnostics);
            walk_expr(rhs, lifted, definitions, table, counter, diagnostics);
        }
        HirExprKind::FieldAccess { base, .. } => {
            walk_expr(base, lifted, definitions, table, counter, diagnostics);
        }
        HirExprKind::Index { base, index } => {
            walk_expr(base, lifted, definitions, table, counter, diagnostics);
            walk_expr(index, lifted, definitions, table, counter, diagnostics);
        }

        // Calls.
        HirExprKind::Call { callee, args } => {
            walk_expr(callee, lifted, definitions, table, counter, diagnostics);
            for arg in args {
                walk_expr(arg, lifted, definitions, table, counter, diagnostics);
            }
        }
        HirExprKind::MethodCall { receiver, args, .. } => {
            walk_expr(receiver, lifted, definitions, table, counter, diagnostics);
            for arg in args {
                walk_expr(arg, lifted, definitions, table, counter, diagnostics);
            }
        }

        // Constructors / collections.
        HirExprKind::StructInit { fields, .. } => {
            for (_, val) in fields {
                walk_expr(val, lifted, definitions, table, counter, diagnostics);
            }
        }
        HirExprKind::EnumVariant { fields, .. } => {
            for f in fields {
                walk_expr(f, lifted, definitions, table, counter, diagnostics);
            }
        }
        HirExprKind::ListLit(elems)
        | HirExprKind::SetLit(elems)
        | HirExprKind::TupleLit(elems) => {
            for e in elems {
                walk_expr(e, lifted, definitions, table, counter, diagnostics);
            }
        }
        HirExprKind::MapLit(pairs) => {
            for (k, v) in pairs {
                walk_expr(k, lifted, definitions, table, counter, diagnostics);
                walk_expr(v, lifted, definitions, table, counter, diagnostics);
            }
        }

        // Control flow.
        HirExprKind::Block { stmts, expr: trailing } => {
            for stmt in stmts {
                walk_stmt(stmt, lifted, definitions, table, counter, diagnostics);
            }
            if let Some(e) = trailing {
                walk_expr(e, lifted, definitions, table, counter, diagnostics);
            }
        }
        HirExprKind::If { cond, then_, else_ } => {
            walk_expr(cond, lifted, definitions, table, counter, diagnostics);
            walk_expr(then_, lifted, definitions, table, counter, diagnostics);
            if let Some(e) = else_ {
                walk_expr(e, lifted, definitions, table, counter, diagnostics);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            walk_expr(scrutinee, lifted, definitions, table, counter, diagnostics);
            for arm in arms {
                walk_arm(arm, lifted, definitions, table, counter, diagnostics);
            }
        }

        // String interpolation.
        HirExprKind::StringInterp { parts } => {
            for part in parts {
                walk_expr(part, lifted, definitions, table, counter, diagnostics);
            }
        }

        // Parallel iteration.
        HirExprKind::ParallelForEach { list, callback, .. } => {
            walk_expr(list, lifted, definitions, table, counter, diagnostics);
            walk_expr(callback, lifted, definitions, table, counter, diagnostics);
            check_parallel_ref_captures(callback, table, diagnostics);
        }
        HirExprKind::ParallelMap { list, callback, .. } => {
            walk_expr(list, lifted, definitions, table, counter, diagnostics);
            walk_expr(callback, lifted, definitions, table, counter, diagnostics);
            check_parallel_ref_captures(callback, table, diagnostics);
        }
    }
}

/// Walks a statement, recursing into its sub-expressions.
fn walk_stmt(
    stmt: &mut HirStmt,
    lifted: &mut Vec<HirFunction>,
    definitions: &mut DefinitionTable,
    table: &mut TypeTable,
    counter: &mut u32,
    diagnostics: &mut Diagnostics,
) {
    match stmt {
        HirStmt::Let { value, .. } => walk_expr(value, lifted, definitions, table, counter, diagnostics),
        HirStmt::Assign { target, value } => {
            walk_expr(target, lifted, definitions, table, counter, diagnostics);
            walk_expr(value, lifted, definitions, table, counter, diagnostics);
        }
        HirStmt::Expr(e) => walk_expr(e, lifted, definitions, table, counter, diagnostics),
    }
}

/// Walks a match arm body.
fn walk_arm(
    arm: &mut HirMatchArm,
    lifted: &mut Vec<HirFunction>,
    definitions: &mut DefinitionTable,
    table: &mut TypeTable,
    counter: &mut u32,
    diagnostics: &mut Diagnostics,
) {
    walk_expr(&mut arm.body, lifted, definitions, table, counter, diagnostics);
}

/// Checks whether a parallel callback captures any reference types.
///
/// Capturing a `Ref[T]` in a `parallel each` or `parallel each via` callback
/// is unsafe because the reference points to shared memory — multiple threads
/// would read/write through the same pointer, causing a data race.
fn check_parallel_ref_captures(callback: &HirExpr, table: &TypeTable, diagnostics: &mut Diagnostics) {
    if let HirExprKind::MakeClosure { captures, .. } = &callback.kind {
        for &(_, ty_id) in captures {
            if type_contains_ref(ty_id, table) {
                diagnostics.push(
                    Diagnostic::error(
                        "`parallel each` callback cannot capture a reference — shared mutable state is not thread-safe",
                    )
                    .with_label(Label::primary(callback.span, "captures a reference")),
                );
                return;
            }
        }
    }
}

/// Returns `true` if the given type is or wraps a `Ty::Ref`.
fn type_contains_ref(ty_id: TypeId, table: &TypeTable) -> bool {
    match table.resolve(ty_id) {
        Ty::Ref { .. } => true,
        Ty::Mutable(inner) => type_contains_ref(*inner, table),
        _ => false,
    }
}

/// Collects free variables (captures) from a lambda body.
///
/// Walks the body recursively and finds any `HirExprKind::Local(def_id)` that
/// is not a lambda parameter and not a function/builtin reference. These are
/// variables from enclosing scopes that need to be captured.
fn collect_captures(
    body: &HirExpr,
    params: &[crate::hir::HirParam],
    definitions: &DefinitionTable,
) -> Vec<(DefId, TypeId)> {
    use rustc_hash::FxHashSet;
    let mut param_defs: FxHashSet<DefId> = FxHashSet::default();
    for p in params {
        param_defs.insert(p.def_id);
    }
    let mut captures = Vec::new();
    let mut seen = FxHashSet::default();
    collect_captures_walk(body, &param_defs, definitions, &mut captures, &mut seen);
    captures
}

/// Recursive walker for capture collection.
fn collect_captures_walk(
    expr: &HirExpr,
    param_defs: &rustc_hash::FxHashSet<DefId>,
    definitions: &DefinitionTable,
    captures: &mut Vec<(DefId, TypeId)>,
    seen: &mut rustc_hash::FxHashSet<DefId>,
) {
    match &expr.kind {
        HirExprKind::Local(def_id) => {
            // Skip lambda's own parameters.
            if param_defs.contains(def_id) {
                return;
            }
            // Skip functions, builtins, and binding methods — they're not captured values.
            if let Some(info) = definitions.get(*def_id) {
                if matches!(
                    info.kind,
                    DefinitionKind::Function
                        | DefinitionKind::BuiltinFunction
                        | DefinitionKind::BuiltinMethod
                        | DefinitionKind::Lambda
                        | DefinitionKind::BindingMethod(_)
                ) {
                    return;
                }
            }
            // Deduplicate by DefId.
            if seen.insert(*def_id) {
                captures.push((*def_id, expr.ty));
            }
        }
        // Leaf nodes.
        HirExprKind::Literal(_)
        | HirExprKind::MakeClosure { .. }
        | HirExprKind::Continue
        | HirExprKind::Error => {}
        // Single sub-expression.
        HirExprKind::UnaryOp { operand, .. }
        | HirExprKind::Borrow { operand, .. } => {
            collect_captures_walk(operand, param_defs, definitions, captures, seen);
        }
        HirExprKind::Deref(inner)
        | HirExprKind::Await(inner)
        | HirExprKind::Spawn(inner)
        | HirExprKind::Loop { body: inner }
        | HirExprKind::Is { expr: inner, .. }
        | HirExprKind::As { expr: inner, .. } => {
            collect_captures_walk(inner, param_defs, definitions, captures, seen);
        }
        HirExprKind::Return(opt) | HirExprKind::Break(opt) => {
            if let Some(inner) = opt {
                collect_captures_walk(inner, param_defs, definitions, captures, seen);
            }
        }
        // Two sub-expressions.
        HirExprKind::BinaryOp { lhs, rhs, .. } => {
            collect_captures_walk(lhs, param_defs, definitions, captures, seen);
            collect_captures_walk(rhs, param_defs, definitions, captures, seen);
        }
        HirExprKind::FieldAccess { base, .. } => {
            collect_captures_walk(base, param_defs, definitions, captures, seen);
        }
        HirExprKind::Index { base, index } => {
            collect_captures_walk(base, param_defs, definitions, captures, seen);
            collect_captures_walk(index, param_defs, definitions, captures, seen);
        }
        // Calls.
        HirExprKind::Call { callee, args } => {
            collect_captures_walk(callee, param_defs, definitions, captures, seen);
            for arg in args {
                collect_captures_walk(arg, param_defs, definitions, captures, seen);
            }
        }
        HirExprKind::MethodCall { receiver, args, .. } => {
            collect_captures_walk(receiver, param_defs, definitions, captures, seen);
            for arg in args {
                collect_captures_walk(arg, param_defs, definitions, captures, seen);
            }
        }
        // Constructors.
        HirExprKind::StructInit { fields, .. } => {
            for (_, val) in fields {
                collect_captures_walk(val, param_defs, definitions, captures, seen);
            }
        }
        HirExprKind::EnumVariant { fields, .. } => {
            for f in fields {
                collect_captures_walk(f, param_defs, definitions, captures, seen);
            }
        }
        HirExprKind::ListLit(elems)
        | HirExprKind::SetLit(elems)
        | HirExprKind::TupleLit(elems) => {
            for e in elems {
                collect_captures_walk(e, param_defs, definitions, captures, seen);
            }
        }
        HirExprKind::MapLit(pairs) => {
            for (k, v) in pairs {
                collect_captures_walk(k, param_defs, definitions, captures, seen);
                collect_captures_walk(v, param_defs, definitions, captures, seen);
            }
        }
        // Control flow.
        HirExprKind::Block { stmts, expr: trailing } => {
            for stmt in stmts {
                collect_captures_walk_stmt(stmt, param_defs, definitions, captures, seen);
            }
            if let Some(e) = trailing {
                collect_captures_walk(e, param_defs, definitions, captures, seen);
            }
        }
        HirExprKind::If { cond, then_, else_ } => {
            collect_captures_walk(cond, param_defs, definitions, captures, seen);
            collect_captures_walk(then_, param_defs, definitions, captures, seen);
            if let Some(e) = else_ {
                collect_captures_walk(e, param_defs, definitions, captures, seen);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            collect_captures_walk(scrutinee, param_defs, definitions, captures, seen);
            for arm in arms {
                collect_captures_walk(&arm.body, param_defs, definitions, captures, seen);
            }
        }
        HirExprKind::StringInterp { parts } => {
            for part in parts {
                collect_captures_walk(part, param_defs, definitions, captures, seen);
            }
        }
        // Parallel iteration.
        HirExprKind::ParallelForEach { list, callback, .. } => {
            collect_captures_walk(list, param_defs, definitions, captures, seen);
            collect_captures_walk(callback, param_defs, definitions, captures, seen);
        }
        HirExprKind::ParallelMap { list, callback, .. } => {
            collect_captures_walk(list, param_defs, definitions, captures, seen);
            collect_captures_walk(callback, param_defs, definitions, captures, seen);
        }
        // Lambda nodes inside a lambda body are already lifted by depth-first walk.
        HirExprKind::Lambda { .. } => {}
    }
}

/// Walks a statement for capture collection.
fn collect_captures_walk_stmt(
    stmt: &HirStmt,
    param_defs: &rustc_hash::FxHashSet<DefId>,
    definitions: &DefinitionTable,
    captures: &mut Vec<(DefId, TypeId)>,
    seen: &mut rustc_hash::FxHashSet<DefId>,
) {
    match stmt {
        HirStmt::Let { value, .. } => {
            collect_captures_walk(value, param_defs, definitions, captures, seen);
        }
        HirStmt::Assign { target, value } => {
            collect_captures_walk(target, param_defs, definitions, captures, seen);
            collect_captures_walk(value, param_defs, definitions, captures, seen);
        }
        HirStmt::Expr(e) => {
            collect_captures_walk(e, param_defs, definitions, captures, seen);
        }
    }
}

/// Extracts the return type from a `Ty::Function` type.
///
/// Returns [`TypeId::ERROR`] if the type is not a function type.
fn extract_ret_type(type_id: TypeId, table: &TypeTable) -> TypeId {
    match table.resolve(type_id) {
        Ty::Function { ret, .. } => *ret,
        _ => TypeId::ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hir::{HirExpr, HirExprKind, HirFunction, HirLiteral, HirParam};
    use spark_span::Span;
    use spark_types::IntWidth;
    use std::collections::HashMap;

    /// Creates a dummy Span for testing.
    fn dummy_span() -> Span {
        Span::DUMMY
    }

    /// Creates a dummy TypeTable with a function type registered.
    fn make_table_with_fn_type() -> (TypeTable, TypeId, TypeId) {
        let mut table = TypeTable::new();
        let int_ty = table.intern(Ty::Int(IntWidth::I64));
        let fn_ty = table.intern(Ty::Function {
            params: vec![int_ty],
            ret: int_ty,
            is_async: false,
        });
        (table, int_ty, fn_ty)
    }

    /// Creates a simple lambda HirExpr: fn(x: Int): Int { x }
    fn make_lambda(
        param_def_id: DefId,
        body_def_id: DefId,
        int_ty: TypeId,
        fn_ty: TypeId,
    ) -> HirExpr {
        HirExpr::new(
            HirExprKind::Lambda {
                params: vec![HirParam {
                    def_id: param_def_id,
                    ty: int_ty,
                    span: dummy_span(),
                }],
                body: Box::new(HirExpr::new(
                    HirExprKind::Local(body_def_id),
                    int_ty,
                    dummy_span(),
                )),
            },
            fn_ty,
            dummy_span(),
        )
    }

    #[test]
    fn lift_single_lambda() {
        let (mut table, int_ty, fn_ty) = make_table_with_fn_type();
        let mut definitions = DefinitionTable::new();

        // Create a param DefId for the lambda parameter.
        let param_def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Parameter,
            spark_intern::sym("x"),
            dummy_span(),
            Visibility::Private,
            ModulePath::root(),
        ));

        // Create main function that contains a lambda.
        let main_def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            spark_intern::sym("main"),
            dummy_span(),
            Visibility::Public,
            ModulePath::root(),
        ));

        let lambda_expr = make_lambda(param_def, param_def, int_ty, fn_ty);
        let main_fn = HirFunction {
            def_id: main_def,
            params: vec![],
            body: lambda_expr,
            ret_type: fn_ty,
            is_async: false,
            span: dummy_span(),
            captures: vec![],
        };

        let mut hir = Hir {
            functions: HashMap::from([(main_def, main_fn)]),
            statics: HashMap::new(),
        };

        lift_lambdas(&mut hir, &mut definitions, &mut table, &mut Diagnostics::new());

        // The lambda should be replaced with a Local reference.
        let main = hir.functions.get(&main_def).unwrap();
        assert!(
            matches!(&main.body.kind, HirExprKind::Local(id) if !id.is_dummy()),
            "Lambda should be replaced with Local, got: {:?}",
            main.body.kind
        );

        // A new lifted function should exist.
        assert_eq!(hir.functions.len(), 2, "Should have main + lifted lambda");

        // The lifted function should have DefinitionKind::Lambda.
        let lifted_def_id = match &main.body.kind {
            HirExprKind::Local(id) => *id,
            _ => unreachable!(),
        };
        let info = definitions.get(lifted_def_id).unwrap();
        assert_eq!(info.kind, DefinitionKind::Lambda);
        assert!(info.name.as_str().starts_with("__lambda_"));
    }

    #[test]
    fn no_lambdas_unchanged() {
        let mut table = TypeTable::new();
        let int_ty = table.intern(Ty::Int(IntWidth::I64));
        let mut definitions = DefinitionTable::new();

        let main_def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            spark_intern::sym("main"),
            dummy_span(),
            Visibility::Public,
            ModulePath::root(),
        ));

        let main_fn = HirFunction {
            def_id: main_def,
            params: vec![],
            body: HirExpr::new(HirExprKind::Literal(HirLiteral::Int(42)), int_ty, dummy_span()),
            ret_type: int_ty,
            is_async: false,
            span: dummy_span(),
            captures: vec![],
        };

        let mut hir = Hir {
            functions: HashMap::from([(main_def, main_fn)]),
            statics: HashMap::new(),
        };

        lift_lambdas(&mut hir, &mut definitions, &mut table, &mut Diagnostics::new());

        assert_eq!(hir.functions.len(), 1, "No new functions should be created");
        assert!(
            matches!(hir.functions[&main_def].body.kind, HirExprKind::Literal(HirLiteral::Int(42))),
        );
    }

    #[test]
    fn multiple_lambdas_get_unique_ids() {
        let (mut table, int_ty, fn_ty) = make_table_with_fn_type();
        let mut definitions = DefinitionTable::new();

        let param1 = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Parameter,
            spark_intern::sym("x"),
            dummy_span(),
            Visibility::Private,
            ModulePath::root(),
        ));
        let param2 = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Parameter,
            spark_intern::sym("y"),
            dummy_span(),
            Visibility::Private,
            ModulePath::root(),
        ));

        let main_def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            spark_intern::sym("main"),
            dummy_span(),
            Visibility::Public,
            ModulePath::root(),
        ));

        // Block with two let-bound lambdas.
        let lambda1 = make_lambda(param1, param1, int_ty, fn_ty);
        let lambda2 = make_lambda(param2, param2, int_ty, fn_ty);

        let body = HirExpr::new(
            HirExprKind::Block {
                stmts: vec![
                    HirStmt::Expr(lambda1),
                    HirStmt::Expr(lambda2),
                ],
                expr: None,
            },
            int_ty,
            dummy_span(),
        );

        let main_fn = HirFunction {
            def_id: main_def,
            params: vec![],
            body,
            ret_type: int_ty,
            is_async: false,
            span: dummy_span(),
            captures: vec![],
        };

        let mut hir = Hir {
            functions: HashMap::from([(main_def, main_fn)]),
            statics: HashMap::new(),
        };

        lift_lambdas(&mut hir, &mut definitions, &mut table, &mut Diagnostics::new());

        // Should have main + 2 lifted lambdas.
        assert_eq!(hir.functions.len(), 3);

        // Both lambda names should be unique.
        let lambda_names: Vec<_> = definitions
            .iter()
            .filter(|(_, info)| info.kind == DefinitionKind::Lambda)
            .map(|(_, info)| info.name.as_str().to_string())
            .collect();
        assert_eq!(lambda_names.len(), 2);
        assert_ne!(lambda_names[0], lambda_names[1]);
    }

    #[test]
    fn nested_lambdas_both_lifted() {
        let (mut table, int_ty, fn_ty) = make_table_with_fn_type();
        let mut definitions = DefinitionTable::new();

        let inner_param = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Parameter,
            spark_intern::sym("y"),
            dummy_span(),
            Visibility::Private,
            ModulePath::root(),
        ));
        let outer_param = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Parameter,
            spark_intern::sym("x"),
            dummy_span(),
            Visibility::Private,
            ModulePath::root(),
        ));

        let main_def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            spark_intern::sym("main"),
            dummy_span(),
            Visibility::Public,
            ModulePath::root(),
        ));

        // Nested: fn(x): fn_ty { fn(y): Int { y } }
        let inner_lambda = make_lambda(inner_param, inner_param, int_ty, fn_ty);
        let outer_lambda = HirExpr::new(
            HirExprKind::Lambda {
                params: vec![HirParam {
                    def_id: outer_param,
                    ty: int_ty,
                    span: dummy_span(),
                }],
                body: Box::new(inner_lambda),
            },
            fn_ty,
            dummy_span(),
        );

        let main_fn = HirFunction {
            def_id: main_def,
            params: vec![],
            body: outer_lambda,
            ret_type: fn_ty,
            is_async: false,
            span: dummy_span(),
            captures: vec![],
        };

        let mut hir = Hir {
            functions: HashMap::from([(main_def, main_fn)]),
            statics: HashMap::new(),
        };

        lift_lambdas(&mut hir, &mut definitions, &mut table, &mut Diagnostics::new());

        // main + inner lambda + outer lambda = 3 functions total.
        assert_eq!(hir.functions.len(), 3);

        // main's body should be Local (outer lambda ref).
        assert!(matches!(&hir.functions[&main_def].body.kind, HirExprKind::Local(_)));
    }
}
