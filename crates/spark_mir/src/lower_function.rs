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
    }

    let param_count = u32::try_from(hir_fn.params.len()).unwrap_or(u32::MAX);

    // Lower the body expression (value discarded; explicit `return` sets RETURN_PLACE)
    let _body_local = lower_expr(&hir_fn.body, &mut fb, mir, type_table, defs);

    // Assign Void to return place on fallthrough (explicit `return` handles non-void)
    if !fb.is_terminated() {
        fb.push_stmt(Statement::Assign {
            place: Place::from_local(Local::RETURN_PLACE),
            rvalue: Rvalue::Use(Operand::Constant(crate::mir::Constant::Void)),
            span: hir_fn.body.span,
        });
        fb.terminate(Terminator::Return);
    }

    fb.finish(param_count, hir_fn.span)
}

/// Allocates a local for a function parameter.
fn alloc_param(fb: &mut FunctionBuilder, param: &HirParam, defs: &DefinitionTable) -> Local {
    let name = defs.get(param.def_id).map(|info| info.name);
    match name {
        Some(sym) => fb.fresh_local(param.ty, sym, false, param.span),
        None => fb.fresh_temp(param.ty, param.span),
    }
}
