//! Pass 5: Copy propagation.
//!
//! Conservative single-block copy propagation. For each block, identifies
//! assignments of the form `temp = Copy(src)` where `temp` is a compiler
//! temporary (not a user variable), assigned exactly once in the block, and
//! `src` is not reassigned between the copy and the last use of `temp`.
//! All uses of `temp` in the block are replaced with `src`.
//!
//! The dead copy assignment is left in place and will be cleaned up by DCE
//! in the next iteration.

use rustc_hash::FxHashMap;
use spark_mir::{Local, MirFunction, Operand, Place, Projection, Statement, Terminator};
use spark_types::TypeTable;

/// Propagates copies within each block of `func`.
///
/// Returns `true` if any replacements were made.
pub fn propagate_copies(func: &mut MirFunction, table: &TypeTable) -> bool {
    let mut changed = false;

    // A droppable copy owns a separate reference.
    let preserved: rustc_hash::FxHashSet<Local> = func
        .locals
        .iter()
        .filter(|d| d.is_user_variable || spark_drop::needs_drop(d.ty, table))
        .map(|d| d.local)
        .collect();

    for block in &mut func.blocks {
        changed |= propagate_in_block(&mut block.statements, &mut block.terminator, &preserved);
    }

    changed
}

/// Propagates copies within a single block.
fn propagate_in_block(
    statements: &mut [Statement],
    terminator: &mut Terminator,
    preserved: &rustc_hash::FxHashSet<Local>,
) -> bool {
    // Phase 1: Find candidate copy assignments: temp = Copy(src)
    // where temp is not a user variable and has no projections.
    let candidates = find_candidates(statements, preserved);

    if candidates.is_empty() {
        return false;
    }

    // Phase 2: For each candidate, verify safety and apply.
    let mut changed = false;
    for (temp, src, def_idx) in &candidates {
        if is_safe_to_propagate(statements, terminator, *temp, *src, *def_idx) {
            // Replace all uses of temp with src in statements after def_idx.
            for stmt in statements.iter_mut().skip(def_idx + 1) {
                changed |= replace_local_in_statement(stmt, *temp, *src);
            }
            changed |= replace_local_in_terminator(terminator, *temp, *src);
        }
    }

    changed
}

/// Finds `temp = Copy(src)` where temp is a bare temporary.
/// Returns `(temp, src_local, statement_index)`.
fn find_candidates(
    statements: &[Statement],
    preserved: &rustc_hash::FxHashSet<Local>,
) -> Vec<(Local, Local, usize)> {
    let mut candidates = Vec::new();
    // Count assignments per local in this block.
    let mut assign_counts: FxHashMap<Local, usize> = FxHashMap::default();

    for stmt in statements {
        if let Statement::Assign { place, .. } = stmt {
            if place.projections.is_empty() {
                *assign_counts.entry(place.local).or_insert(0) += 1;
            }
        }
    }

    for (i, stmt) in statements.iter().enumerate() {
        if let Statement::Assign {
            place,
            rvalue: spark_mir::Rvalue::Use(Operand::Copy(src)),
            ..
        } = stmt
        {
            // temp must be a bare local (no projections), not a user variable,
            // and assigned exactly once in this block.
            if place.projections.is_empty()
                && src.projections.is_empty()
                && !preserved.contains(&place.local)
                && assign_counts.get(&place.local) == Some(&1)
            {
                candidates.push((place.local, src.local, i));
            }
        }
    }

    candidates
}

/// Checks whether it's safe to replace `temp` with `src` after `def_idx`.
///
/// It's safe if `src` is not reassigned between `def_idx` and the last use
/// of `temp` in the block.
fn is_safe_to_propagate(
    statements: &[Statement],
    terminator: &Terminator,
    temp: Local,
    src: Local,
    def_idx: usize,
) -> bool {
    // Find the last use of temp in the block.
    let mut last_use_idx = None;
    for (i, stmt) in statements.iter().enumerate().skip(def_idx + 1) {
        if statement_reads_local(stmt, temp) {
            last_use_idx = Some(i);
        }
    }
    if terminator_reads_local(terminator, temp) {
        // The terminator is conceptually after all statements.
        last_use_idx = Some(statements.len());
    }

    let Some(last_use) = last_use_idx else {
        // temp is never used after the copy — still safe (DCE will clean it up).
        return true;
    };

    // Check that src is not reassigned between def_idx (exclusive) and
    // last_use (inclusive for statements, or terminator).
    let check_end = last_use.min(statements.len());
    for stmt in statements.iter().take(check_end).skip(def_idx + 1) {
        if statement_assigns_local(stmt, src) {
            return false;
        }
    }

    true
}

/// Returns true if a statement reads from `local`.
fn statement_reads_local(stmt: &Statement, local: Local) -> bool {
    match stmt {
        Statement::Assign { place, rvalue, .. } => {
            place_projections_read_local(place, local) || rvalue_reads_local(rvalue, local)
        }
        Statement::Drop { place, .. } => {
            place.local == local || place_projections_read_local(place, local)
        }
        Statement::Nop => false,
    }
}

/// Returns true if place projections read a local (e.g. Index).
fn place_projections_read_local(place: &Place, local: Local) -> bool {
    place
        .projections
        .iter()
        .any(|p| matches!(p, Projection::Index(l) if *l == local))
}

/// Returns true if an rvalue reads from `local`.
fn rvalue_reads_local(rvalue: &spark_mir::Rvalue, local: Local) -> bool {
    match rvalue {
        spark_mir::Rvalue::BinaryOp(_, a, b) => {
            operand_reads_local(a, local) || operand_reads_local(b, local)
        }
        spark_mir::Rvalue::Use(op) | spark_mir::Rvalue::UnaryOp(_, op) => {
            operand_reads_local(op, local)
        }
        spark_mir::Rvalue::Ref { place, .. } | spark_mir::Rvalue::Discriminant(place) => {
            place.local == local || place_projections_read_local(place, local)
        }
        spark_mir::Rvalue::Aggregate(_, ops) | spark_mir::Rvalue::StringInterp(ops) => {
            ops.iter().any(|op| operand_reads_local(op, local))
        }
        spark_mir::Rvalue::ListRange {
            start, end, step, ..
        }
        | spark_mir::Rvalue::RangeLen {
            start, end, step, ..
        } => {
            operand_reads_local(start, local)
                || operand_reads_local(end, local)
                || operand_reads_local(step, local)
        }
        spark_mir::Rvalue::CollectionGetOptional {
            collection, index, ..
        } => {
            collection.local == local
                || index.local == local
                || place_projections_read_local(collection, local)
                || place_projections_read_local(index, local)
        }
        spark_mir::Rvalue::Cast { operand, .. } => operand_reads_local(operand, local),
    }
}

/// Returns true if an operand reads from `local`.
fn operand_reads_local(op: &Operand, local: Local) -> bool {
    match op {
        Operand::Copy(place) | Operand::Move(place) => {
            place.local == local || place_projections_read_local(place, local)
        }
        Operand::Constant(_) => false,
    }
}

/// Returns true if the terminator reads from `local`.
fn terminator_reads_local(term: &Terminator, local: Local) -> bool {
    match term {
        Terminator::Goto { .. } | Terminator::Return | Terminator::Unreachable => false,
        Terminator::SwitchInt { discriminant, .. } => operand_reads_local(discriminant, local),
        Terminator::Call {
            func,
            args,
            destination,
            ..
        } => {
            operand_reads_local(func, local)
                || args.iter().any(|a| operand_reads_local(a, local))
                || place_projections_read_local(destination, local)
        }
    }
}

/// Returns true if a statement assigns to `local` (bare, no projections).
fn statement_assigns_local(stmt: &Statement, local: Local) -> bool {
    match stmt {
        Statement::Assign { place, .. } => place.local == local && place.projections.is_empty(),
        _ => false,
    }
}

/// Replaces reads of `from` with reads of `to` in a statement.
fn replace_local_in_statement(stmt: &mut Statement, from: Local, to: Local) -> bool {
    match stmt {
        Statement::Assign { place, rvalue, .. } => {
            let mut changed = replace_local_in_place_projections(place, from, to);
            changed |= replace_local_in_rvalue(rvalue, from, to);
            changed
        }
        Statement::Drop { place, .. } => {
            let changed = if place.local == from {
                place.local = to;
                true
            } else {
                false
            };
            changed | replace_local_in_place_projections(place, from, to)
        }
        Statement::Nop => false,
    }
}

/// Replaces locals in place projections.
fn replace_local_in_place_projections(place: &mut Place, from: Local, to: Local) -> bool {
    let mut changed = false;
    for proj in &mut place.projections {
        if let Projection::Index(l) = proj {
            if *l == from {
                *l = to;
                changed = true;
            }
        }
    }
    changed
}

/// Replaces reads of `from` with `to` in an rvalue.
fn replace_local_in_rvalue(rvalue: &mut spark_mir::Rvalue, from: Local, to: Local) -> bool {
    match rvalue {
        spark_mir::Rvalue::BinaryOp(_, a, b) => {
            let changed = replace_local_in_operand(a, from, to);
            changed | replace_local_in_operand(b, from, to)
        }
        spark_mir::Rvalue::Use(op) | spark_mir::Rvalue::UnaryOp(_, op) => {
            replace_local_in_operand(op, from, to)
        }
        spark_mir::Rvalue::Ref { place, .. } | spark_mir::Rvalue::Discriminant(place) => {
            replace_local_in_place(place, from, to)
        }
        spark_mir::Rvalue::Aggregate(_, ops) | spark_mir::Rvalue::StringInterp(ops) => {
            let mut changed = false;
            for op in ops {
                changed |= replace_local_in_operand(op, from, to);
            }
            changed
        }
        spark_mir::Rvalue::ListRange {
            start, end, step, ..
        }
        | spark_mir::Rvalue::RangeLen {
            start, end, step, ..
        } => {
            let mut changed = replace_local_in_operand(start, from, to);
            changed |= replace_local_in_operand(end, from, to);
            changed |= replace_local_in_operand(step, from, to);
            changed
        }
        spark_mir::Rvalue::CollectionGetOptional {
            collection, index, ..
        } => {
            let changed = replace_local_in_place(collection, from, to);
            changed | replace_local_in_place(index, from, to)
        }
        spark_mir::Rvalue::Cast { operand, .. } => replace_local_in_operand(operand, from, to),
    }
}

/// Replaces the root local and projection locals in a place.
fn replace_local_in_place(place: &mut Place, from: Local, to: Local) -> bool {
    let changed = if place.local == from {
        place.local = to;
        true
    } else {
        false
    };
    changed | replace_local_in_place_projections(place, from, to)
}

/// Replaces reads of `from` with `to` in an operand.
fn replace_local_in_operand(op: &mut Operand, from: Local, to: Local) -> bool {
    match op {
        Operand::Copy(place) | Operand::Move(place) => replace_local_in_place(place, from, to),
        Operand::Constant(_) => false,
    }
}

/// Replaces reads of `from` with `to` in a terminator.
fn replace_local_in_terminator(term: &mut Terminator, from: Local, to: Local) -> bool {
    match term {
        Terminator::Goto { .. } | Terminator::Return | Terminator::Unreachable => false,
        Terminator::SwitchInt { discriminant, .. } => {
            replace_local_in_operand(discriminant, from, to)
        }
        Terminator::Call {
            func,
            args,
            destination,
            ..
        } => {
            let mut changed = replace_local_in_operand(func, from, to);
            for arg in args {
                changed |= replace_local_in_operand(arg, from, to);
            }
            changed |= replace_local_in_place_projections(destination, from, to);
            changed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_mir::{
        BasicBlock, Constant, Local, LocalDecl, MirFunction, Operand, Place, Rvalue, Statement,
        Terminator,
    };
    use spark_resolve::DefId;
    use spark_span::Span;
    use spark_types::TypeId;

    fn make_func(blocks: Vec<BasicBlock>, locals: Vec<LocalDecl>) -> MirFunction {
        MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals,
            blocks,
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        }
    }

    fn local_decl(idx: u32, is_user: bool) -> LocalDecl {
        LocalDecl {
            local: Local(idx),
            ty: TypeId::ERROR,
            name: if is_user {
                Some(spark_intern::sym("x"))
            } else {
                None
            },
            is_user_variable: is_user,
            mutable: false,
            span: Span::DUMMY,
        }
    }

    fn assign_copy(dst: u32, src: u32) -> Statement {
        Statement::Assign {
            place: Place::from_local(Local(dst)),
            rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(src)))),
            span: Span::DUMMY,
        }
    }

    fn assign_int(local: u32, value: i64) -> Statement {
        Statement::Assign {
            place: Place::from_local(Local(local)),
            rvalue: Rvalue::Use(Operand::Constant(Constant::Int(value))),
            span: Span::DUMMY,
        }
    }

    // ── Basic propagation ──────────────────────────────────────

    #[test]
    fn simple_copy_propagation() {
        // _1 = const 42
        // _2 = Copy(_1)   ← temp copy
        // _0 = Copy(_2)   ← should become Copy(_1)
        let mut func = make_func(
            vec![BasicBlock {
                statements: vec![assign_int(1, 42), assign_copy(2, 1), assign_copy(0, 2)],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            vec![
                local_decl(0, false), // return place
                local_decl(1, false), // temp
                local_decl(2, false), // temp to propagate
            ],
        );

        assert!(propagate_copies(&mut func, &TypeTable::new()));

        // _0 should now read from _1
        match &func.blocks[0].statements[2] {
            Statement::Assign {
                rvalue: Rvalue::Use(Operand::Copy(place)),
                ..
            } => assert_eq!(place.local, Local(1)),
            other => panic!("expected Copy(_1), got {other:?}"),
        }
    }

    #[test]
    fn user_variable_not_propagated() {
        // _1 = const 42
        // _2 = Copy(_1)   ← _2 is a user variable
        // _0 = Copy(_2)
        let mut func = make_func(
            vec![BasicBlock {
                statements: vec![assign_int(1, 42), assign_copy(2, 1), assign_copy(0, 2)],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            vec![
                local_decl(0, false),
                local_decl(1, false),
                local_decl(2, true), // user variable!
            ],
        );

        assert!(!propagate_copies(&mut func, &TypeTable::new()));
    }

    #[test]
    fn multi_def_not_propagated() {
        // _2 = Copy(_1)
        // _0 = Copy(_2)
        // _2 = Copy(_3)   ← _2 assigned twice → not propagated
        let mut func = make_func(
            vec![BasicBlock {
                statements: vec![assign_copy(2, 1), assign_copy(0, 2), assign_copy(2, 3)],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            vec![
                local_decl(0, false),
                local_decl(1, false),
                local_decl(2, false),
                local_decl(3, false),
            ],
        );

        assert!(!propagate_copies(&mut func, &TypeTable::new()));
    }

    #[test]
    fn src_reassigned_before_last_use_not_propagated() {
        // _2 = Copy(_1)
        // _1 = const 99   ← _1 reassigned!
        // _0 = Copy(_2)   ← cannot replace _2 with _1
        let mut func = make_func(
            vec![BasicBlock {
                statements: vec![assign_copy(2, 1), assign_int(1, 99), assign_copy(0, 2)],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            vec![
                local_decl(0, false),
                local_decl(1, false),
                local_decl(2, false),
            ],
        );

        assert!(!propagate_copies(&mut func, &TypeTable::new()));
    }

    #[test]
    fn no_candidates_returns_false() {
        let mut func = make_func(
            vec![BasicBlock {
                statements: vec![assign_int(0, 42)],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            vec![local_decl(0, false)],
        );

        assert!(!propagate_copies(&mut func, &TypeTable::new()));
    }

    #[test]
    fn propagation_into_terminator() {
        // _2 = Copy(_1)
        // switchInt(_2) { ... }
        // Should replace _2 with _1 in the terminator.
        let mut func = make_func(
            vec![BasicBlock {
                statements: vec![assign_int(1, 42), assign_copy(2, 1)],
                terminator: Terminator::SwitchInt {
                    discriminant: Operand::Copy(Place::from_local(Local(2))),
                    targets: vec![(0, spark_mir::BlockId(0))],
                    otherwise: spark_mir::BlockId(0),
                },
                is_cleanup: false,
            }],
            vec![
                local_decl(0, false),
                local_decl(1, false),
                local_decl(2, false),
            ],
        );

        assert!(propagate_copies(&mut func, &TypeTable::new()));

        match &func.blocks[0].terminator {
            Terminator::SwitchInt { discriminant, .. } => match discriminant {
                Operand::Copy(place) => assert_eq!(place.local, Local(1)),
                other => panic!("expected Copy(_1), got {other:?}"),
            },
            other => panic!("expected SwitchInt, got {other:?}"),
        }
    }
}
