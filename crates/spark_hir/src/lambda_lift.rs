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

use crate::hir::{Hir, HirExpr, HirExprKind, HirFunction, HirParam, HirPattern, HirStmt};
use crate::visitor::{walk_expr, walk_expr_mut, walk_pattern, walk_stmt, HirMutVisitor, HirVisitor};

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

    let mut lifter = LambdaLifter {
        lifted: Vec::new(),
        definitions,
        table,
        counter: 0,
        diagnostics,
    };

    for def_id in func_ids {
        if let Some(func) = hir.functions.get_mut(&def_id) {
            lifter.visit_expr_mut(&mut func.body);
        }
    }

    for func in lifter.lifted {
        hir.functions.insert(func.def_id, func);
    }
}

// ─── Lambda lifter (mutable visitor) ────────────────────────────────────────

/// Mutable visitor that lifts `Lambda` and `Spawn` nodes into top-level functions.
struct LambdaLifter<'a> {
    /// Accumulated lifted functions to be inserted into `hir.functions`.
    lifted: Vec<HirFunction>,
    /// Definition table for allocating synthetic `DefId`s.
    definitions: &'a mut DefinitionTable,
    /// Type table for looking up and registering types.
    table: &'a mut TypeTable,
    /// Counter for generating unique lambda names.
    counter: u32,
    /// Diagnostic sink for errors (e.g. thread-safety violations).
    diagnostics: &'a mut Diagnostics,
}

impl HirMutVisitor for LambdaLifter<'_> {
    fn visit_expr_mut(&mut self, expr: &mut HirExpr) {
        // Lambda lifting — take ownership and transform.
        if matches!(expr.kind, HirExprKind::Lambda { .. }) {
            let old_kind = std::mem::replace(&mut expr.kind, HirExprKind::Error);
            if let HirExprKind::Lambda { params, mut body } = old_kind {
                // Depth-first: lift any nested lambdas inside this body first.
                self.visit_expr_mut(&mut body);

                // Allocate a synthetic DefId for the lifted function.
                let name = spark_intern::sym(&format!("__lambda_{}", self.counter));
                self.counter += 1;

                let def_id = self.definitions.insert(DefinitionInfo::new(
                    DefinitionKind::Lambda,
                    name,
                    expr.span,
                    Visibility::Private,
                    ModulePath::root(),
                ));

                // Extract return type from the function type.
                let ret_type = extract_ret_type(expr.ty, self.table);

                // Register the function type for this DefId in the type table.
                self.table.set_def_type(def_id, expr.ty);

                // Collect free variables (captures) from the lambda body.
                let captures = collect_captures(&body, &params, self.definitions);

                // Create the lifted function.
                self.lifted.push(HirFunction {
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
            return;
        }

        // Determine post-walk action before walking children.
        let is_spawn = matches!(expr.kind, HirExprKind::Spawn(_));
        let is_parallel_callback = matches!(
            expr.kind,
            HirExprKind::ParallelForEach { .. } | HirExprKind::ParallelMap { .. }
        );
        let is_parallel_map_reduce = matches!(expr.kind, HirExprKind::ParallelMapReduce { .. });

        // Walk all children (default recursion).
        walk_expr_mut(self, expr);

        // Post-walk: spawn body lifting.
        if is_spawn {
            lift_spawn_body(
                expr,
                &mut self.lifted,
                self.definitions,
                self.table,
                &mut self.counter,
                self.diagnostics,
            );
        }

        // Post-walk: thread-safety checks on parallel callbacks.
        if is_parallel_callback {
            if let HirExprKind::ParallelForEach { callback, .. }
            | HirExprKind::ParallelMap { callback, .. } = &expr.kind
            {
                check_parallel_ref_captures(callback, self.table, self.diagnostics);
            }
        } else if is_parallel_map_reduce {
            if let HirExprKind::ParallelMapReduce { mapper, .. } = &expr.kind {
                check_parallel_ref_captures(mapper, self.table, self.diagnostics);
            }
        }
    }
}

// ─── Capture collector (immutable visitor) ──────────────────────────────────

/// Immutable visitor that collects free variables referenced from a lambda body.
struct CaptureCollector<'a> {
    /// The lambda's own parameter `DefId`s (excluded from captures).
    param_defs: &'a rustc_hash::FxHashSet<DefId>,
    /// Definition table for checking definition kinds.
    definitions: &'a DefinitionTable,
    /// Accumulated captures `(DefId, TypeId)`.
    captures: Vec<(DefId, TypeId)>,
    /// Deduplication set.
    seen: rustc_hash::FxHashSet<DefId>,
    /// `DefId`s defined locally within the lambda body (let bindings,
    /// match pattern bindings). These are NOT captures.
    local_defs: rustc_hash::FxHashSet<DefId>,
}

impl HirVisitor for CaptureCollector<'_> {
    fn visit_expr(&mut self, expr: &HirExpr) {
        match &expr.kind {
            HirExprKind::Local(def_id) => {
                // Skip lambda's own parameters.
                if self.param_defs.contains(def_id) {
                    return;
                }
                // Skip variables defined within the lambda body (let bindings,
                // match/to pattern bindings).
                if self.local_defs.contains(def_id) {
                    return;
                }
                // Skip functions, builtins, and binding methods — they're not captured values.
                if let Some(info) = self.definitions.get(*def_id) {
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
                if self.seen.insert(*def_id) {
                    self.captures.push((*def_id, expr.ty));
                }
            }
            // Lambda nodes inside a lambda body are already lifted by depth-first walk.
            HirExprKind::Lambda { .. } => {}
            _ => walk_expr(self, expr),
        }
    }

    fn visit_stmt(&mut self, stmt: &HirStmt) {
        // Register let-binding def_ids as local before visiting the body,
        // so references to them aren't treated as captures.
        if let HirStmt::Let { def_id, .. } = stmt {
            self.local_defs.insert(*def_id);
        }
        walk_stmt(self, stmt);
    }

    fn visit_pattern(&mut self, pat: &HirPattern) {
        // Register pattern-binding def_ids as local.
        match pat {
            HirPattern::Binding { def_id } | HirPattern::TypeNarrow { def_id, .. } => {
                self.local_defs.insert(*def_id);
            }
            HirPattern::Variant { fields, .. } => {
                for f in fields {
                    self.visit_pattern(f);
                }
            }
            HirPattern::Struct { fields, .. } => {
                for (_, p) in fields {
                    self.visit_pattern(p);
                }
            }
            _ => {}
        }
        walk_pattern(self, pat);
    }
}

// ─── Helper functions ───────────────────────────────────────────────────────

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

/// Lifts the body of a `spawn` expression into a synthetic zero-arg closure.
///
/// The spawn body becomes the body of a lifted function. Any free variables
/// in the body are captured (just like a regular lambda). The `Spawn` node's
/// inner expression is replaced with either `Local(def_id)` (non-capturing)
/// or `MakeClosure { func_def_id, captures }` (capturing).
fn lift_spawn_body(
    expr: &mut HirExpr,
    lifted: &mut Vec<HirFunction>,
    definitions: &mut DefinitionTable,
    table: &mut TypeTable,
    counter: &mut u32,
    diagnostics: &mut Diagnostics,
) {
    // Take ownership of the Spawn's inner expression.
    let old_kind = std::mem::replace(&mut expr.kind, HirExprKind::Error);
    let HirExprKind::Spawn(body) = old_kind else {
        expr.kind = old_kind;
        return;
    };

    // Determine the result type: unwrap Future[T] → T.
    let result_ty = match table.resolve(expr.ty) {
        Ty::Future { inner } => *inner,
        _ => expr.ty,
    };

    // Create the closure type: fn() -> T
    let closure_ty = table.intern(Ty::Function {
        params: vec![],
        ret: result_ty,
        is_async: false,
    });

    // Allocate a DefId for the lifted spawn function.
    let name = spark_intern::sym(&format!("__spawn_{counter}"));
    *counter += 1;

    let def_id = definitions.insert(DefinitionInfo::new(
        DefinitionKind::Lambda,
        name,
        expr.span,
        Visibility::Private,
        ModulePath::root(),
    ));

    table.set_def_type(def_id, closure_ty);

    // Collect captures from the body (no params to exclude).
    let captures = collect_captures(&body, &[], definitions);

    // Check thread safety of captures.
    check_spawn_captures(&captures, expr.span, table, diagnostics);

    // Create the lifted function.
    lifted.push(HirFunction {
        def_id,
        params: vec![],
        body: *body,
        ret_type: result_ty,
        is_async: false,
        span: expr.span,
        captures: captures.clone(),
    });

    // Build the closure reference.
    let closure_expr = if captures.is_empty() {
        HirExpr {
            kind: HirExprKind::Local(def_id),
            ty: closure_ty,
            span: expr.span,
        }
    } else {
        HirExpr {
            kind: HirExprKind::MakeClosure {
                func_def_id: def_id,
                captures,
            },
            ty: closure_ty,
            span: expr.span,
        }
    };

    // Replace the Spawn's inner with the closure.
    expr.kind = HirExprKind::Spawn(Box::new(closure_expr));
}

/// Checks whether a spawn block captures any types that are unsafe for threading.
///
/// Capturing a `Ref[T]` or mutable binding in a `spawn` block is unsafe because
/// the spawned task runs on a separate thread — shared mutable state causes data races.
fn check_spawn_captures(
    captures: &[(DefId, TypeId)],
    span: spark_span::Span,
    table: &TypeTable,
    diagnostics: &mut Diagnostics,
) {
    for &(_, ty_id) in captures {
        if type_contains_ref(ty_id, table) {
            diagnostics.push(
                Diagnostic::error(
                    "`spawn` block cannot capture a reference — shared mutable state is not thread-safe",
                )
                .with_label(Label::primary(span, "captures a reference")),
            );
            return;
        }
        if type_is_mutable(ty_id, table) {
            diagnostics.push(
                Diagnostic::error(
                    "`spawn` block cannot capture a mutable binding — use a channel or mutex for shared state",
                )
                .with_label(Label::primary(span, "captures a mutable binding")),
            );
            return;
        }
    }
}

/// Returns `true` if the given type is a `Ty::Mutable`.
fn type_is_mutable(ty_id: TypeId, table: &TypeTable) -> bool {
    matches!(table.resolve(ty_id), Ty::Mutable(_))
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
    params: &[HirParam],
    definitions: &DefinitionTable,
) -> Vec<(DefId, TypeId)> {
    use rustc_hash::FxHashSet;
    let mut param_defs: FxHashSet<DefId> = FxHashSet::default();
    for p in params {
        param_defs.insert(p.def_id);
    }
    let mut collector = CaptureCollector {
        param_defs: &param_defs,
        definitions,
        captures: Vec::new(),
        seen: FxHashSet::default(),
        local_defs: FxHashSet::default(),
    };
    collector.visit_expr(body);
    collector.captures
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
    use crate::hir::{HirExpr, HirExprKind, HirFunction, HirLiteral, HirParam, HirStmt};
    use spark_span::Span;
    use spark_types::IntWidth;
    use std::collections::HashMap;

    /// Creates a dummy Span for testing.
    fn dummy_span() -> Span {
        Span::DUMMY
    }

    /// Creates a dummy `TypeTable` with a function type registered.
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

    /// Creates a simple lambda `HirExpr`: fn(x: Int): Int { x }
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
