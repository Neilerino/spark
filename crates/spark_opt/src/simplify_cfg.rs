//! Pass 3: CFG simplification.
//!
//! Three sub-transformations:
//!
//! 1. **Constant `SwitchInt`**: If the discriminant is a constant, replace
//!    with `Goto` to the matching arm (or `otherwise`).
//!
//! 2. **Goto chain merging**: If block A ends with `Goto { target: B }` and
//!    B has exactly one predecessor and B isn't the entry block and A != B,
//!    merge B into A.
//!
//! 3. **Goto forwarding**: For empty blocks with `Goto { target }`, rewrite
//!    all references to that block to point directly to `target`.

use rustc_hash::FxHashMap;
use spark_mir::{BlockId, Constant, MirFunction, Operand, Terminator};

/// Simplifies the CFG of `func`.
///
/// Returns `true` if any transformations were applied.
pub fn simplify_cfg(func: &mut MirFunction) -> bool {
    let mut changed = false;
    changed |= fold_constant_switches(func);
    changed |= merge_goto_chains(func);
    changed |= forward_gotos(func);
    changed
}

/// Replaces `SwitchInt` with constant discriminants with `Goto`.
fn fold_constant_switches(func: &mut MirFunction) -> bool {
    let mut changed = false;
    for block in &mut func.blocks {
        let new_term = match &block.terminator {
            Terminator::SwitchInt {
                discriminant: Operand::Constant(c),
                targets,
                otherwise,
            } => constant_to_i128(c).map(|value| {
                let target = targets
                    .iter()
                    .find(|(v, _)| *v == value)
                    .map_or(*otherwise, |(_, t)| *t);
                Terminator::Goto { target }
            }),
            _ => None,
        };
        if let Some(term) = new_term {
            block.terminator = term;
            changed = true;
        }
    }
    changed
}

/// Converts a `Constant` to `i128` for switch matching.
fn constant_to_i128(c: &Constant) -> Option<i128> {
    match c {
        Constant::Int(n) => Some(i128::from(*n)),
        Constant::Bool(b) => Some(i128::from(*b)),
        Constant::Char(ch) => Some(i128::from(u32::from(*ch))),
        _ => None,
    }
}

/// Merges blocks where A → B is the only path to B.
///
/// Uses `split_at_mut()` to get simultaneous mutable access to both blocks.
fn merge_goto_chains(func: &mut MirFunction) -> bool {
    let mut changed = false;

    // Recompute predecessor counts each iteration since merges change them.
    loop {
        let pred_counts = compute_predecessor_counts(func);
        let mut merged_any = false;

        for a_idx in 0..func.blocks.len() {
            let b_idx = match &func.blocks[a_idx].terminator {
                Terminator::Goto { target } => target.index(),
                _ => continue,
            };

            // B must have exactly 1 predecessor, not be the entry block,
            // and A != B.
            if b_idx == a_idx || b_idx == 0 || pred_counts.get(&BlockId(b_idx as u32)) != Some(&1)
            {
                continue;
            }

            // Use split_at_mut for simultaneous mutable access.
            let (lo, hi) = if a_idx < b_idx {
                let (left, right) = func.blocks.split_at_mut(b_idx);
                (&mut left[a_idx], &mut right[0])
            } else {
                let (left, right) = func.blocks.split_at_mut(a_idx);
                (&mut right[0], &mut left[b_idx])
            };

            // lo = A (source of goto), hi = B (target) in both branches above.
            let (a_block, b_block) = (lo, hi);

            // Move B's statements into A, then take B's terminator.
            a_block.statements.append(&mut b_block.statements);
            a_block.terminator =
                std::mem::replace(&mut b_block.terminator, Terminator::Unreachable);

            merged_any = true;
            changed = true;
            break; // Restart — predecessor counts have changed.
        }

        if !merged_any {
            break;
        }
    }

    changed
}

/// For empty blocks with `Goto { target }`, rewrite all references to
/// that block to point directly to `target`.
fn forward_gotos(func: &mut MirFunction) -> bool {
    // Build the forwarding map: block_id → ultimate target.
    let mut forward: FxHashMap<BlockId, BlockId> = FxHashMap::default();
    for (i, block) in func.blocks.iter().enumerate() {
        if block.statements.is_empty() {
            if let Terminator::Goto { target } = &block.terminator {
                // Don't forward the entry block or self-loops.
                if i != 0 && target.index() != i {
                    forward.insert(BlockId(i as u32), *target);
                }
            }
        }
    }

    if forward.is_empty() {
        return false;
    }

    // Resolve chains: A → B → C becomes A → C.
    let mut resolved: FxHashMap<BlockId, BlockId> = FxHashMap::default();
    for &start in forward.keys() {
        let mut current = start;
        // Follow the chain with a bound to avoid infinite loops.
        for _ in 0..func.blocks.len() {
            if let Some(&next) = forward.get(&current) {
                current = next;
            } else {
                break;
            }
        }
        resolved.insert(start, current);
    }

    // Rewrite all terminator targets.
    let mut changed = false;
    for block in &mut func.blocks {
        changed |= rewrite_terminator_targets(&mut block.terminator, &resolved);
    }
    changed
}

/// Rewrites block targets in a terminator using the forwarding map.
fn rewrite_terminator_targets(
    term: &mut Terminator,
    forward: &FxHashMap<BlockId, BlockId>,
) -> bool {
    let mut changed = false;
    match term {
        Terminator::SwitchInt {
            targets, otherwise, ..
        } => {
            for (_, t) in targets.iter_mut() {
                if let Some(&new) = forward.get(t) {
                    *t = new;
                    changed = true;
                }
            }
            if let Some(&new) = forward.get(otherwise) {
                *otherwise = new;
                changed = true;
            }
        }
        Terminator::Goto { target } | Terminator::Call { target, .. } => {
            if let Some(&new) = forward.get(target) {
                *target = new;
                changed = true;
            }
        }
        Terminator::Return | Terminator::Unreachable => {}
    }
    changed
}

/// Computes how many predecessors each block has.
fn compute_predecessor_counts(func: &MirFunction) -> FxHashMap<BlockId, usize> {
    let mut counts: FxHashMap<BlockId, usize> = FxHashMap::default();
    for block in &func.blocks {
        for target in terminator_successors(&block.terminator) {
            *counts.entry(target).or_insert(0) += 1;
        }
    }
    counts
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

    fn make_func(blocks: Vec<BasicBlock>) -> MirFunction {
        MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: vec![LocalDecl {
                local: Local(0),
                ty: TypeId::ERROR,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            }],
            blocks,
            captures: vec![],
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

    // ── Constant SwitchInt ──────────────────────────────────────

    #[test]
    fn constant_switch_folds_to_goto() {
        // bb0: switchInt(const 1) { 0 → bb1, 1 → bb2 } otherwise bb3
        // fold_constant_switches → goto bb2
        // merge_goto_chains → bb2 merged into bb0 (1 predecessor)
        // Result: bb0 has `_0 = 20; return`
        let mut func = make_func(vec![
            BasicBlock {
                statements: vec![],
                terminator: Terminator::SwitchInt {
                    discriminant: Operand::Constant(Constant::Int(1)),
                    targets: vec![(0, BlockId(1)), (1, BlockId(2))],
                    otherwise: BlockId(3),
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![assign_int(0, 10)],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![assign_int(0, 20)],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![assign_int(0, 30)],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
        ]);

        assert!(simplify_cfg(&mut func));
        // bb2's content merged into bb0 → bb0 now returns directly
        assert!(matches!(func.blocks[0].terminator, Terminator::Return));
        assert_eq!(func.blocks[0].statements.len(), 1);
    }

    #[test]
    fn constant_switch_only_folds() {
        // Test fold_constant_switches in isolation.
        let mut func = make_func(vec![
            BasicBlock {
                statements: vec![],
                terminator: Terminator::SwitchInt {
                    discriminant: Operand::Constant(Constant::Int(99)),
                    targets: vec![(0, BlockId(1))],
                    otherwise: BlockId(2),
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![assign_int(0, 10)],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![assign_int(0, 20)],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
        ]);

        assert!(fold_constant_switches(&mut func));
        assert!(matches!(
            func.blocks[0].terminator,
            Terminator::Goto { target } if target == BlockId(2)
        ));
    }

    #[test]
    fn constant_switch_falls_through_to_otherwise() {
        // Full simplify_cfg: switchInt(const 99) { 0 → bb1 } otherwise bb2
        // Folds to goto bb2, then merges bb2 into bb0.
        let mut func = make_func(vec![
            BasicBlock {
                statements: vec![],
                terminator: Terminator::SwitchInt {
                    discriminant: Operand::Constant(Constant::Int(99)),
                    targets: vec![(0, BlockId(1))],
                    otherwise: BlockId(2),
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![assign_int(0, 10)],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![assign_int(0, 20)],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
        ]);

        assert!(simplify_cfg(&mut func));
        assert!(matches!(func.blocks[0].terminator, Terminator::Return));
        assert_eq!(func.blocks[0].statements.len(), 1);
    }

    #[test]
    fn constant_switch_with_bool() {
        // Test fold_constant_switches with bool in isolation.
        let mut func = make_func(vec![
            BasicBlock {
                statements: vec![],
                terminator: Terminator::SwitchInt {
                    discriminant: Operand::Constant(Constant::Bool(true)),
                    targets: vec![(0, BlockId(1))],
                    otherwise: BlockId(2),
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![assign_int(0, 10)],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![assign_int(0, 20)],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
        ]);

        assert!(fold_constant_switches(&mut func));
        assert!(matches!(
            func.blocks[0].terminator,
            Terminator::Goto { target } if target == BlockId(2)
        ));
    }

    // ── Goto chain merging ──────────────────────────────────────

    #[test]
    fn merge_linear_goto_chain() {
        // bb0: _0 = 1; goto → bb1
        // bb1: _0 = 2; return
        // bb1 has 1 predecessor (bb0), so merge.
        let mut func = make_func(vec![
            BasicBlock {
                statements: vec![assign_int(0, 1)],
                terminator: Terminator::Goto {
                    target: BlockId(1),
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![assign_int(0, 2)],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
        ]);

        assert!(simplify_cfg(&mut func));
        // bb0 should now contain both assignments and return
        assert_eq!(func.blocks[0].statements.len(), 2);
        assert!(matches!(func.blocks[0].terminator, Terminator::Return));
        // bb1 should be cleared
        assert!(func.blocks[1].statements.is_empty());
        assert!(matches!(func.blocks[1].terminator, Terminator::Unreachable));
    }

    #[test]
    fn no_merge_when_multiple_predecessors() {
        // bb0: goto → bb2
        // bb1: goto → bb2
        // bb2: return
        // bb2 has 2 predecessors — should NOT be merged.
        let mut func = make_func(vec![
            BasicBlock {
                statements: vec![],
                terminator: Terminator::Goto {
                    target: BlockId(2),
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![],
                terminator: Terminator::Goto {
                    target: BlockId(2),
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![assign_int(0, 42)],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
        ]);

        // Goto forwarding may still fire (since bb0 and bb1 are empty gotos),
        // but merge should not.
        let _ = simplify_cfg(&mut func);
        // bb2 should still have its statement
        assert_eq!(func.blocks[2].statements.len(), 1);
    }

    #[test]
    fn no_merge_entry_block() {
        // bb0 (entry) should never be merged away as a target.
        // bb1: goto → bb0  (self-referencing, but entry is not merged into bb1)
        let mut func = make_func(vec![
            BasicBlock {
                statements: vec![assign_int(0, 42)],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![],
                terminator: Terminator::Goto {
                    target: BlockId(0),
                },
                is_cleanup: false,
            },
        ]);

        let _ = simplify_cfg(&mut func);
        // Entry block should still exist with its statement.
        assert_eq!(func.blocks[0].statements.len(), 1);
    }

    #[test]
    fn self_loop_preserved() {
        // bb0: goto → bb0 (infinite loop)
        let mut func = make_func(vec![BasicBlock {
            statements: vec![],
            terminator: Terminator::Goto {
                target: BlockId(0),
            },
            is_cleanup: false,
        }]);

        let _ = simplify_cfg(&mut func);
        // Self-loop should remain
        assert!(matches!(
            func.blocks[0].terminator,
            Terminator::Goto { target } if target == BlockId(0)
        ));
    }

    // ── Goto forwarding ──────────────────────────────────────────

    #[test]
    fn forward_empty_goto_block() {
        // Test forward_gotos in isolation.
        // bb0: switchInt(_1) { 0 → bb1 } otherwise bb2
        // bb1: goto → bb3  (empty, just a goto)
        // bb1 should be forwarded: bb0's target 0→bb1 becomes 0→bb3
        let mut func = make_func(vec![
            BasicBlock {
                statements: vec![],
                terminator: Terminator::SwitchInt {
                    discriminant: Operand::Copy(Place::from_local(Local(1))),
                    targets: vec![(0, BlockId(1))],
                    otherwise: BlockId(2),
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![],
                terminator: Terminator::Goto {
                    target: BlockId(3),
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![assign_int(0, 10)],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![assign_int(0, 20)],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
        ]);

        assert!(forward_gotos(&mut func));
        // bb0's target for arm 0 should now be bb3
        match &func.blocks[0].terminator {
            Terminator::SwitchInt { targets, .. } => {
                assert_eq!(targets[0].1, BlockId(3));
            }
            other => panic!("expected SwitchInt, got {other:?}"),
        }
    }

    #[test]
    fn forward_chain_of_gotos() {
        // bb0: goto → bb1
        // bb1: goto → bb2 (empty)
        // bb2: goto → bb3 (empty)
        // bb3: _0 = 42; return
        // After full simplify_cfg: bb0 merges with bb3 (forwarded + merged)
        let mut func = make_func(vec![
            BasicBlock {
                statements: vec![],
                terminator: Terminator::Goto {
                    target: BlockId(1),
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![],
                terminator: Terminator::Goto {
                    target: BlockId(2),
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![],
                terminator: Terminator::Goto {
                    target: BlockId(3),
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![assign_int(0, 42)],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
        ]);

        assert!(simplify_cfg(&mut func));
        // After forwarding bb1→bb3, bb2→bb3, then bb0→bb1 becomes bb0→bb3,
        // then merge_goto_chains merges bb3 into bb0 (single predecessor after
        // forwarding). Final state: bb0 has _0=42, return.
        assert!(matches!(func.blocks[0].terminator, Terminator::Return));
        assert_eq!(func.blocks[0].statements.len(), 1);
    }

    #[test]
    fn forward_gotos_only() {
        // Test forward_gotos in isolation on a chain.
        let mut func = make_func(vec![
            BasicBlock {
                statements: vec![assign_int(0, 1)],
                terminator: Terminator::Goto {
                    target: BlockId(1),
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![],
                terminator: Terminator::Goto {
                    target: BlockId(2),
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![],
                terminator: Terminator::Goto {
                    target: BlockId(3),
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![assign_int(0, 42)],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
        ]);

        assert!(forward_gotos(&mut func));
        // bb0's target should now be bb3 (resolved through bb1→bb2→bb3)
        assert!(matches!(
            func.blocks[0].terminator,
            Terminator::Goto { target } if target == BlockId(3)
        ));
    }

    #[test]
    fn no_changes_returns_false() {
        // A function that's already optimal.
        let mut func = make_func(vec![BasicBlock {
            statements: vec![assign_int(0, 42)],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(!simplify_cfg(&mut func));
    }
}
