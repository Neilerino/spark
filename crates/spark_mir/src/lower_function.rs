//! Function-level MIR lowering.
//!
//! Allocates the return place, parameter locals, and entry block,
//! then lowers the function body and adds a `Return` terminator.

use rustc_hash::FxHashSet;
use spark_hir::{walk_stmt, HirExprKind, HirFunction, HirParam, HirStmt, HirVisitor};
use spark_resolve::{DefId, DefinitionTable};
use spark_types::{Ty, TypeTable};

use crate::builder::{FunctionBuilder, MirBuilder};
use crate::lower_expr::{emit_assign_with_union_wrap, lower_expr};
use crate::mir::{Local, MirFunction, Operand, Place, Rvalue, Statement, Terminator};

/// Lowers a single HIR function into a MIR function.
pub fn lower_function(
    hir_fn: &HirFunction,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) -> MirFunction {
    let mut fb = FunctionBuilder::new_with_async(hir_fn.def_id, hir_fn.ret_type, hir_fn.span);

    for param in &hir_fn.params {
        let local = alloc_param(&mut fb, param, defs);
        fb.register_def(param.def_id, local);

        // Register RC'd parameters for drop at scope exit.
        // The caller rc_inc'd the argument, so the callee must rc_dec.
        if spark_drop::needs_drop(param.ty, type_table) {
            fb.register_drop(local);
        }
    }

    let param_count = u32::try_from(hir_fn.params.len()).unwrap_or(u32::MAX);

    lower_captures(hir_fn, &mut fb, type_table, defs);

    // Lower the body expression. The result local holds the value of the
    // trailing expression (if any), enabling implicit returns.
    let body_local = lower_expr(&hir_fn.body, &mut fb, mir, type_table, defs);

    if !fb.is_terminated() {
        let span = hir_fn.body.span;
        if matches!(type_table.resolve(hir_fn.ret_type), Ty::Void) {
            if !fb.is_def_local(body_local)
                && !fb.owns_local(body_local)
                && spark_drop::needs_drop(fb.local_type(body_local), type_table)
            {
                fb.push_stmt(Statement::Drop {
                    place: Place::from_local(body_local),
                    span,
                });
            }
            fb.drops_for_return(None, span);
        } else {
            emit_assign_with_union_wrap(&mut fb, Local::RETURN_PLACE, body_local, type_table, span);
            fb.drops_for_return(Some(body_local), span);
        }
        fb.terminate(Terminator::Return);
    }

    fb.finish_with_captures(
        param_count,
        hir_fn.span,
        hir_fn.captures.clone(),
        hir_fn.is_async,
    )
}

fn lower_captures(
    hir_fn: &HirFunction,
    fb: &mut FunctionBuilder,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) {
    if hir_fn.captures.is_empty() {
        return;
    }
    let mut written = IndexedWriteRoots::default();
    written.visit_expr(&hir_fn.body);

    // Environment captures occupy consecutive locals at entry.
    let mut owned_captures = Vec::new();
    for &(def_id, ty) in &hir_fn.captures {
        let local = match defs.get(def_id) {
            Some(info) => fb.fresh_local(ty, info.name, false, hir_fn.span),
            None => fb.fresh_temp(ty, hir_fn.span),
        };
        fb.register_def(def_id, local);
        if written.0.contains(&def_id) && spark_drop::needs_drop(ty, type_table) {
            owned_captures.push((def_id, ty, local));
        }
    }

    // TAG [SPARK-115]
    // Indexed COW consumes the old owner. Leave the environment's owner intact.
    for (def_id, ty, borrowed) in owned_captures {
        let local = fb.fresh_temp(ty, hir_fn.span);
        fb.push_assign(
            Place::from_local(local),
            Rvalue::Use(Operand::Copy(Place::from_local(borrowed))),
            hir_fn.span,
        );
        fb.register_def(def_id, local);
        fb.register_drop(local);
    }
}

#[derive(Default)]
struct IndexedWriteRoots(FxHashSet<DefId>);

impl HirVisitor for IndexedWriteRoots {
    fn visit_stmt(&mut self, stmt: &HirStmt) {
        if let HirStmt::IndexedAssign { root, .. } = stmt {
            if let HirExprKind::Local(def_id) = root.kind {
                self.0.insert(def_id);
            }
        }
        walk_stmt(self, stmt);
    }
}

/// Allocates a local for a function parameter.
fn alloc_param(fb: &mut FunctionBuilder, param: &HirParam, defs: &DefinitionTable) -> Local {
    let name = defs.get(param.def_id).map(|info| info.name);
    match name {
        Some(sym) => fb.fresh_local(param.ty, sym, false, param.span),
        None => fb.fresh_temp(param.ty, param.span),
    }
}
