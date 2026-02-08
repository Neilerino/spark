//! Diagnostic messages for borrow check errors.
//!
//! Each function constructs a rich [`Diagnostic`] with primary and secondary
//! labels, following the established pattern in other Spark crates.

use spark_diagnostics::{Diagnostic, Label};
use spark_mir::{Local, MirFunction};
use spark_span::Span;

use crate::facts::{Loan, Point};

/// Emits a diagnostic for conflicting borrows.
///
/// Two borrows of overlapping places are active at the same time and at
/// least one is mutable.
pub fn emit_conflicting_borrows(
    diagnostics: &mut spark_diagnostics::Diagnostics,
    first: &Loan,
    second: &Loan,
    func: &MirFunction,
) {
    let first_span = span_for_point(first.issued_at, func);
    let second_span = span_for_point(second.issued_at, func);

    let msg = if first.mutable && second.mutable {
        "cannot borrow value as mutable more than once at a time"
    } else {
        "cannot borrow value as mutable because it is also borrowed as immutable"
    };

    diagnostics.push(
        Diagnostic::error(msg)
            .with_label(Label::primary(second_span, "second borrow occurs here"))
            .with_label(Label::secondary(first_span, "first borrow occurs here")),
    );
}

/// Emits a diagnostic for use after move.
pub fn emit_use_after_move(
    diagnostics: &mut spark_diagnostics::Diagnostics,
    use_point: Point,
    move_point: Point,
    local: Local,
    func: &MirFunction,
) {
    let use_span = span_for_point(use_point, func);
    let move_span = span_for_point(move_point, func);
    let name = local_name(local, func);

    diagnostics.push(
        Diagnostic::error(format!("use of moved value `{name}`"))
            .with_label(Label::primary(use_span, "value used here after move"))
            .with_label(Label::secondary(move_span, "value moved here")),
    );
}

/// Emits a diagnostic for a borrow of a moved value.
pub fn emit_borrow_of_moved(
    diagnostics: &mut spark_diagnostics::Diagnostics,
    loan: &Loan,
    move_point: Point,
    func: &MirFunction,
) {
    let borrow_span = span_for_point(loan.issued_at, func);
    let move_span = span_for_point(move_point, func);
    let name = local_name(loan.place.local, func);

    diagnostics.push(
        Diagnostic::error(format!("cannot borrow `{name}` because it has been moved"))
            .with_label(Label::primary(borrow_span, "borrow occurs here"))
            .with_label(Label::secondary(move_span, "value moved here")),
    );
}

/// Emits a diagnostic for mutation through an immutable reference.
pub fn emit_mutation_through_immutable_ref(
    diagnostics: &mut spark_diagnostics::Diagnostics,
    assign_point: Point,
    func: &MirFunction,
) {
    let span = span_for_point(assign_point, func);

    diagnostics.push(
        Diagnostic::error("cannot assign through an immutable reference")
            .with_label(Label::primary(span, "assignment through immutable reference")),
    );
}

/// Returns the span for a given program point.
#[allow(clippy::cast_possible_truncation)]
fn span_for_point(point: Point, func: &MirFunction) -> Span {
    let block_idx = point.block.index();
    if block_idx >= func.blocks.len() {
        return func.span;
    }
    let block = &func.blocks[block_idx];
    let stmt_idx = point.index as usize;

    if stmt_idx < block.statements.len() {
        match &block.statements[stmt_idx] {
            spark_mir::Statement::Assign { span, .. }
            | spark_mir::Statement::Drop { span, .. } => *span,
            spark_mir::Statement::Nop => func.span,
        }
    } else {
        // Terminator — use the terminator's span if available, else function span.
        match &block.terminator {
            spark_mir::Terminator::Call { span, .. } => *span,
            _ => func.span,
        }
    }
}

/// Returns a display name for a local.
fn local_name(local: Local, func: &MirFunction) -> String {
    let idx = local.index();
    if idx < func.locals.len() {
        if let Some(sym) = func.locals[idx].name {
            return sym.as_str().to_owned();
        }
    }
    format!("_{}", local.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_name_with_symbol() {
        use spark_mir::{BasicBlock, LocalDecl, Terminator};
        use spark_resolve::DefId;
        use spark_types::TypeId;

        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: vec![
                LocalDecl {
                    local: Local(0),
                    ty: TypeId::ERROR,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(1),
                    ty: TypeId::ERROR,
                    name: Some(spark_intern::sym("x")),
                    is_user_variable: true,
                    mutable: false,
                    span: Span::DUMMY,
                },
            ],
            blocks: vec![BasicBlock {
                statements: vec![],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            span: Span::DUMMY,
        };

        assert_eq!(local_name(Local(0), &func), "_0");
        assert_eq!(local_name(Local(1), &func), "x");
    }
}
