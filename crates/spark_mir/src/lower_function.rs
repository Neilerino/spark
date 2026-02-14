//! Function-level MIR lowering.
//!
//! Allocates the return place, parameter locals, and entry block,
//! then lowers the function body and adds a `Return` terminator.

use spark_hir::{HirFunction, HirParam};
use spark_resolve::DefinitionTable;
use spark_types::TypeTable;

use crate::builder::{FunctionBuilder, MirBuilder};
use crate::lower_expr::lower_expr;
use crate::mir::{
    Local, MirFunction, Operand, Place, Rvalue, Statement, Terminator,
};

/// Lowers a single HIR function into a MIR function.
pub fn lower_function(
    hir_fn: &HirFunction,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) -> MirFunction {
    let mut fb = FunctionBuilder::new(hir_fn.def_id, hir_fn.ret_type, hir_fn.span);

    // Allocate parameter locals
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

    // Allocate locals for captured variables.
    // The codegen will load these from the env_ptr at function entry.
    // The body's `HirExprKind::Local(captured_def_id)` resolves to these locals.
    for (cap_def_id, cap_ty) in &hir_fn.captures {
        let name = defs.get(*cap_def_id).map(|info| info.name);
        let local = match name {
            Some(sym) => fb.fresh_local(*cap_ty, sym, false, hir_fn.span),
            None => fb.fresh_temp(*cap_ty, hir_fn.span),
        };
        fb.register_def(*cap_def_id, local);
    }

    // Lower the body expression. The result local holds the value of the
    // trailing expression (if any), enabling implicit returns.
    let body_local = lower_expr(&hir_fn.body, &mut fb, mir, type_table, defs);

    // On fallthrough (no explicit `return`), assign the body result to the
    // return place. This handles implicit returns from trailing expressions
    // (e.g., `fn(x: Int): Int { x * 2 }` where `x * 2` is the return value).
    if !fb.is_terminated() {
        fb.push_stmt(Statement::Assign {
            place: Place::from_local(Local::RETURN_PLACE),
            rvalue: Rvalue::Use(Operand::Copy(Place::from_local(body_local))),
            span: hir_fn.body.span,
        });
        // Drop all remaining locals before the fallthrough return.
        fb.drops_for_return(None, hir_fn.body.span);
        fb.terminate(Terminator::Return);
    }

    fb.finish_with_captures(param_count, hir_fn.span, hir_fn.captures.clone())
}

/// Allocates a local for a function parameter.
fn alloc_param(fb: &mut FunctionBuilder, param: &HirParam, defs: &DefinitionTable) -> Local {
    let name = defs.get(param.def_id).map(|info| info.name);
    match name {
        Some(sym) => fb.fresh_local(param.ty, sym, false, param.span),
        None => fb.fresh_temp(param.ty, param.span),
    }
}
