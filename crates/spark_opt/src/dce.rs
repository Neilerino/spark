//! Pass 4: Dead code elimination.
//!
//! Two phases:
//!
//! 1. **Unreachable block removal**: BFS from the entry block. Blocks not
//!    reached have their statements cleared and terminator set to `Unreachable`.
//!    Block indices are preserved to avoid invalidating `BlockId`s.
//!
//! 2. **Dead assignment removal**: Locals that are assigned but never read
//!    have their assignments replaced with `Statement::Nop` (cleaned up by
//!    the nop elimination pass in the next iteration). `Local(0)` (the return
//!    place) is always considered used. `Statement::Drop` is never removed.

use rustc_hash::FxHashSet;
use spark_mir::{BlockId, Local, MirFunction, Operand, Place, Projection, Statement, Terminator};

/// Eliminates dead code from `func`.
///
/// Returns `true` if any blocks were cleared or assignments were removed.
pub fn eliminate_dead_code(func: &mut MirFunction) -> bool {
    let mut changed = false;
    changed |= remove_unreachable_blocks(func);
    changed |= remove_dead_assignments(func);
    changed
}

/// Clears unreachable blocks (BFS from entry).
fn remove_unreachable_blocks(func: &mut MirFunction) -> bool {
    let reachable = compute_reachable_blocks(func);
    let mut changed = false;

    for (i, block) in func.blocks.iter_mut().enumerate() {
        if !reachable.contains(&BlockId(i as u32))
            && (!block.statements.is_empty()
                || !matches!(block.terminator, Terminator::Unreachable))
        {
            block.statements.clear();
            block.terminator = Terminator::Unreachable;
            changed = true;
        }
    }

    changed
}

/// BFS from `BlockId(0)` to find all reachable blocks.
fn compute_reachable_blocks(func: &MirFunction) -> FxHashSet<BlockId> {
    let mut visited = FxHashSet::default();
    let mut worklist = vec![BlockId::ENTRY];
    visited.insert(BlockId::ENTRY);

    while let Some(block_id) = worklist.pop() {
        let block = &func.blocks[block_id.index()];
        for succ in terminator_successors(&block.terminator) {
            if visited.insert(succ) {
                worklist.push(succ);
            }
        }
    }

    visited
}

/// Removes assignments to locals that are never read.
fn remove_dead_assignments(func: &mut MirFunction) -> bool {
    let used = compute_used_locals(func);
    let mut changed = false;

    for block in &mut func.blocks {
        for stmt in &mut block.statements {
            if let Statement::Assign { place, .. } = stmt {
                // Only remove bare local assignments (no projections).
                if place.projections.is_empty() && !used.contains(&place.local) {
                    *stmt = Statement::Nop;
                    changed = true;
                }
            }
        }
    }

    changed
}

/// Computes the set of locals that are "used" (read from).
///
/// `Local(0)` is always used (return place). A local is used if it appears
/// as a source in any `Operand::Copy`/`Move`, in `Projection::Index`, or
/// in a `SwitchInt` discriminant.
fn compute_used_locals(func: &MirFunction) -> FxHashSet<Local> {
    let mut used = FxHashSet::default();
    used.insert(Local::RETURN_PLACE);

    // Also consider all parameters as used (they may be used by the caller).
    for i in 1..=func.param_count {
        used.insert(Local(i));
    }

    for block in &func.blocks {
        for stmt in &block.statements {
            collect_used_in_statement(stmt, &mut used);
        }
        collect_used_in_terminator(&block.terminator, &mut used);
    }

    used
}

/// Collects locals used as sources in a statement.
fn collect_used_in_statement(stmt: &Statement, used: &mut FxHashSet<Local>) {
    match stmt {
        Statement::Assign { place, rvalue, .. } => {
            // If the place has projections (e.g. `_9.*`, `_3.field`),
            // the base local is READ to compute the projected address.
            if !place.projections.is_empty() {
                used.insert(place.local);
            }
            // The place itself might use locals in projections.
            collect_used_in_place_projections(place, used);
            // Collect from the rvalue.
            collect_used_in_rvalue(rvalue, used);
        }
        Statement::Drop { place, .. } => {
            used.insert(place.local);
            collect_used_in_place_projections(place, used);
        }
        Statement::Nop => {}
    }
}

/// Collects locals used in place projections (e.g. `_1[_2]`).
fn collect_used_in_place_projections(place: &Place, used: &mut FxHashSet<Local>) {
    for proj in &place.projections {
        if let Projection::Index(local) = proj {
            used.insert(*local);
        }
    }
}

/// Collects locals used in an rvalue.
fn collect_used_in_rvalue(rvalue: &spark_mir::Rvalue, used: &mut FxHashSet<Local>) {
    match rvalue {
        spark_mir::Rvalue::BinaryOp(_, lhs, rhs) => {
            collect_used_in_operand(lhs, used);
            collect_used_in_operand(rhs, used);
        }
        spark_mir::Rvalue::Use(op) | spark_mir::Rvalue::UnaryOp(_, op) => {
            collect_used_in_operand(op, used);
        }
        spark_mir::Rvalue::Ref { place, .. } | spark_mir::Rvalue::Discriminant(place) => {
            used.insert(place.local);
            collect_used_in_place_projections(place, used);
        }
        spark_mir::Rvalue::Aggregate(_, ops) | spark_mir::Rvalue::StringInterp(ops) => {
            for op in ops {
                collect_used_in_operand(op, used);
            }
        }
        spark_mir::Rvalue::ListRange {
            start, end, step, ..
        }
        | spark_mir::Rvalue::RangeLen {
            start, end, step, ..
        } => {
            collect_used_in_operand(start, used);
            collect_used_in_operand(end, used);
            collect_used_in_operand(step, used);
        }
        spark_mir::Rvalue::CollectionGetOptional {
            collection, index, ..
        } => {
            for place in [collection, index] {
                used.insert(place.local);
                collect_used_in_place_projections(place, used);
            }
        }
        spark_mir::Rvalue::Cast { operand, .. } => collect_used_in_operand(operand, used),
    }
}

/// Collects the local used by an operand.
fn collect_used_in_operand(op: &Operand, used: &mut FxHashSet<Local>) {
    match op {
        Operand::Copy(place) | Operand::Move(place) => {
            used.insert(place.local);
            collect_used_in_place_projections(place, used);
        }
        Operand::Constant(_) => {}
    }
}

/// Collects locals used in a terminator.
fn collect_used_in_terminator(term: &Terminator, used: &mut FxHashSet<Local>) {
    match term {
        Terminator::Goto { .. } | Terminator::Return | Terminator::Unreachable => {}
        Terminator::SwitchInt { discriminant, .. } => {
            collect_used_in_operand(discriminant, used);
        }
        Terminator::Call {
            func: func_op,
            args,
            destination,
            ..
        } => {
            collect_used_in_operand(func_op, used);
            for arg in args {
                collect_used_in_operand(arg, used);
            }
            // The destination is written to, but if it has projections
            // those project locals are reads.
            collect_used_in_place_projections(destination, used);
        }
    }
}

/// Returns the successor block IDs of a terminator.
fn terminator_successors(term: &Terminator) -> Vec<BlockId> {
    match term {
        Terminator::SwitchInt {
            targets, otherwise, ..
        } => {
            let mut succs: Vec<BlockId> = targets.iter().map(|(_, t)| *t).collect();
            succs.push(*otherwise);
            succs
        }
        Terminator::Goto { target } | Terminator::Call { target, .. } => vec![*target],
        Terminator::Return | Terminator::Unreachable => vec![],
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
            name: None,
            is_user_variable: is_user,
            mutable: false,
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

    fn assign_copy(dst: u32, src: u32) -> Statement {
        Statement::Assign {
            place: Place::from_local(Local(dst)),
            rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(src)))),
            span: Span::DUMMY,
        }
    }

    // ── Unreachable block removal ──────────────────────────────

    #[test]
    fn removes_unreachable_blocks() {
        // bb0: _0 = 42; return
        // bb1: _0 = 99; return  (unreachable)
        let mut func = make_func(
            vec![
                BasicBlock {
                    statements: vec![assign_int(0, 42)],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![assign_int(0, 99)],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
            vec![local_decl(0, false)],
        );

        assert!(eliminate_dead_code(&mut func));
        assert!(func.blocks[1].statements.is_empty());
        assert!(matches!(func.blocks[1].terminator, Terminator::Unreachable));
    }

    #[test]
    fn reachable_blocks_preserved() {
        // bb0: goto → bb1
        // bb1: _0 = 42; return
        let mut func = make_func(
            vec![
                BasicBlock {
                    statements: vec![],
                    terminator: Terminator::Goto { target: BlockId(1) },
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![assign_int(0, 42)],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
            vec![local_decl(0, false)],
        );

        // bb1 is reachable, so no unreachable removal. But dead assignments
        // might or might not apply.
        let _ = eliminate_dead_code(&mut func);
        assert_eq!(func.blocks[1].statements.len(), 1);
        assert!(matches!(func.blocks[1].terminator, Terminator::Return));
    }

    #[test]
    fn already_unreachable_not_changed() {
        // bb0: return
        // bb1: unreachable (already cleared)
        let mut func = make_func(
            vec![
                BasicBlock {
                    statements: vec![assign_int(0, 42)],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![],
                    terminator: Terminator::Unreachable,
                    is_cleanup: false,
                },
            ],
            vec![local_decl(0, false)],
        );

        assert!(!eliminate_dead_code(&mut func));
    }

    // ── Dead assignment removal ──────────────────────────────────

    #[test]
    fn removes_dead_assignment() {
        // _0 = 42 (used: return place)
        // _1 = 99 (dead: never read)
        // return
        let mut func = make_func(
            vec![BasicBlock {
                statements: vec![assign_int(0, 42), assign_int(1, 99)],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            vec![local_decl(0, false), local_decl(1, false)],
        );

        assert!(eliminate_dead_code(&mut func));
        // _1 = 99 should be replaced with Nop
        assert!(matches!(func.blocks[0].statements[1], Statement::Nop));
        // _0 = 42 should remain
        assert!(matches!(
            func.blocks[0].statements[0],
            Statement::Assign { .. }
        ));
    }

    #[test]
    fn return_place_always_preserved() {
        // _0 = 42; return
        let mut func = make_func(
            vec![BasicBlock {
                statements: vec![assign_int(0, 42)],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            vec![local_decl(0, false)],
        );

        assert!(!eliminate_dead_code(&mut func));
        assert!(matches!(
            func.blocks[0].statements[0],
            Statement::Assign { .. }
        ));
    }

    #[test]
    fn used_local_preserved() {
        // _1 = 42; _0 = Copy(_1); return
        let mut func = make_func(
            vec![BasicBlock {
                statements: vec![assign_int(1, 42), assign_copy(0, 1)],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            vec![local_decl(0, false), local_decl(1, false)],
        );

        assert!(!eliminate_dead_code(&mut func));
    }

    #[test]
    fn drop_never_removed() {
        // _1 = 42; drop(_1); return
        let mut func = make_func(
            vec![BasicBlock {
                statements: vec![
                    assign_int(0, 0),
                    assign_int(1, 42),
                    Statement::Drop {
                        place: Place::from_local(Local(1)),
                        span: Span::DUMMY,
                    },
                ],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            vec![local_decl(0, false), local_decl(1, false)],
        );

        let _ = eliminate_dead_code(&mut func);
        // Drop should always be present
        let has_drop = func.blocks[0]
            .statements
            .iter()
            .any(|s| matches!(s, Statement::Drop { .. }));
        assert!(has_drop, "Drop should never be removed");
    }

    #[test]
    fn projected_assign_keeps_base_local_alive() {
        // _1 = 42; _1.* = 10; return
        // _1 is read by the projected assignment _1.*, so _1 = 42 must not be removed.
        let mut func = make_func(
            vec![BasicBlock {
                statements: vec![
                    assign_int(0, 0),
                    assign_int(1, 42),
                    Statement::Assign {
                        place: Place {
                            local: Local(1),
                            projections: vec![Projection::Deref],
                        },
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(10))),
                        span: Span::DUMMY,
                    },
                ],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            vec![local_decl(0, false), local_decl(1, false)],
        );

        let _ = eliminate_dead_code(&mut func);
        // _1 = 42 must NOT be removed (it's read by _1.*)
        assert!(
            matches!(func.blocks[0].statements[1], Statement::Assign { .. }),
            "Assignment to _1 should be preserved because _1.* reads it"
        );
    }

    #[test]
    fn switchint_discriminant_keeps_local_alive() {
        // _1 = 42; switchInt(_1) { ... }
        // _1 is used by SwitchInt discriminant, so not dead.
        let mut func = make_func(
            vec![BasicBlock {
                statements: vec![assign_int(0, 0), assign_int(1, 42)],
                terminator: Terminator::SwitchInt {
                    discriminant: Operand::Copy(Place::from_local(Local(1))),
                    targets: vec![(0, BlockId(0))],
                    otherwise: BlockId(0),
                },
                is_cleanup: false,
            }],
            vec![local_decl(0, false), local_decl(1, false)],
        );

        assert!(!eliminate_dead_code(&mut func));
    }
}
