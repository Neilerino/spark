//! Backward dataflow liveness analysis.
//!
//! Computes which locals are **live** at each program point. A local is live
//! at a point if it may be read before being reassigned on some path from
//! that point.

use rustc_hash::{FxHashMap, FxHashSet};

use spark_mir::{BlockId, Local, MirFunction};

use crate::facts::{BorrowFacts, Point};

/// The result of liveness analysis.
#[derive(Debug)]
pub struct LivenessResult {
    /// For each program point, the set of locals that are live.
    pub live_at: FxHashMap<Point, FxHashSet<Local>>,
}

impl LivenessResult {
    /// Returns true if a local is live at the given point.
    #[must_use]
    pub fn is_live_at(&self, point: Point, local: Local) -> bool {
        self.live_at
            .get(&point)
            .is_some_and(|set| set.contains(&local))
    }
}

/// Computes liveness for all program points in a function.
///
/// Uses backward dataflow analysis with a worklist algorithm.
/// A local is live at a point if it is used before being reassigned
/// on some path from that point.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn compute_liveness(func: &MirFunction, facts: &BorrowFacts) -> LivenessResult {
    // Build reverse CFG edges: for each point, what points flow into it.
    let mut predecessors: FxHashMap<Point, Vec<Point>> = FxHashMap::default();
    for &(from, to) in &facts.cfg_edges {
        predecessors.entry(to).or_default().push(from);
    }

    // Build per-point gen (use) and kill (def) sets.
    let mut gen_sets: FxHashMap<Point, FxHashSet<Local>> = FxHashMap::default();
    let mut kill_sets: FxHashMap<Point, FxHashSet<Local>> = FxHashMap::default();

    for &(point, local) in &facts.uses {
        gen_sets.entry(point).or_default().insert(local);
    }
    // Moves also count as uses for liveness purposes (the value is read/consumed).
    for &(point, local) in &facts.moves {
        gen_sets.entry(point).or_default().insert(local);
    }
    for &(point, local) in &facts.assignments {
        kill_sets.entry(point).or_default().insert(local);
    }

    // Collect all program points.
    let worklist = collect_all_points(func);

    // Initialize live_at to empty sets.
    let mut live_at: FxHashMap<Point, FxHashSet<Local>> = FxHashMap::default();
    for &point in &worklist {
        live_at.insert(point, FxHashSet::default());
    }

    // Iterate to fixpoint.
    let mut changed = true;
    while changed {
        changed = false;

        // Process points in reverse order for faster convergence.
        for i in (0..worklist.len()).rev() {
            let point = worklist[i];

            // live_out = union of live_in of all successors
            let mut live_out = FxHashSet::default();
            for &(from, to) in &facts.cfg_edges {
                if from == point {
                    if let Some(succ_live) = live_at.get(&to) {
                        for &local in succ_live {
                            live_out.insert(local);
                        }
                    }
                }
            }

            // live_in = gen(point) ∪ (live_out - kill(point))
            let mut live_in = live_out.clone();
            if let Some(kills) = kill_sets.get(&point) {
                for kill in kills {
                    live_in.remove(kill);
                }
            }
            if let Some(gens) = gen_sets.get(&point) {
                for gen in gens {
                    live_in.insert(*gen);
                }
            }

            if live_at.get(&point) != Some(&live_in) {
                live_at.insert(point, live_in);
                changed = true;
            }
        }
    }

    LivenessResult { live_at }
}

/// Collects all program points from a function's blocks.
#[allow(clippy::cast_possible_truncation)]
fn collect_all_points(func: &MirFunction) -> Vec<Point> {
    let mut points = Vec::new();
    for (block_idx, block) in func.blocks.iter().enumerate() {
        let block_id = BlockId(block_idx as u32);
        for stmt_idx in 0..=block.statements.len() {
            points.push(Point::new(block_id, stmt_idx as u32));
        }
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::extract_facts;
    use spark_mir::{
        BasicBlock, Constant, LocalDecl, MirFunction, Operand, Place, Rvalue, Statement,
        Terminator,
    };
    use spark_resolve::DefId;
    use spark_span::Span;
    use spark_types::TypeId;

    fn make_simple_func() -> MirFunction {
        // fn f(): Int { x: Int = 42; x }
        // bb0: _1 = const 42; _0 = copy _1; return
        MirFunction {
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
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(1)))),
                        span: Span::DUMMY,
                    },
                ],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            span: Span::DUMMY,
        }
    }

    #[test]
    fn liveness_simple() {
        let func = make_simple_func();
        let facts = extract_facts(&func);
        let liveness = compute_liveness(&func, &facts);

        // At point (bb0, 0): _1 is assigned, so it should NOT be live before that.
        // Actually, _1 is not used before bb0:0, so it's not live at bb0:0.

        // At point (bb0, 1): _1 is used (Copy), so it IS live at bb0:1 (it's a gen)
        // But _0 is assigned at bb0:1, so we need to check the live_out.
        // At bb0:2 (terminator), _0 is used (return), so _0 is live at bb0:2.
        // At bb0:1, _0 is killed (assigned), so _0 is not live *before* bb0:1 unless
        // it's also used at bb0:1.

        // _1 should be live at point (bb0, 0) because it's used at point (bb0, 1)
        // and assigned at (bb0, 0). Since gen is applied after kill removal:
        // live_in(bb0:0) = gen(bb0:0) ∪ (live_out(bb0:0) - kill(bb0:0))
        // kill(bb0:0) = {_1}, gen(bb0:0) = {}, live_out(bb0:0) = live_in(bb0:1)
        // live_in(bb0:1) = gen(bb0:1) ∪ (live_out(bb0:1) - kill(bb0:1))
        //                = {_1} ∪ ({_0} - {_0}) = {_1}
        // So live_in(bb0:0) = {} ∪ ({_1} - {_1}) = {}
        // _1 is NOT live at bb0:0 because it's defined there and also live_out is {_1}
        // but _1 is in kill set so it's removed. gen is empty. So result is {}.

        // _1 should be live at point (bb0, 1) because it's used there
        let p1 = Point::new(BlockId(0), 1);
        assert!(
            liveness.is_live_at(p1, Local(1)),
            "Local(1) should be live at bb0:1 where it's used"
        );

        // _0 should be live at the terminator (bb0, 2) because return uses it
        let p2 = Point::new(BlockId(0), 2);
        assert!(
            liveness.is_live_at(p2, Local(0)),
            "Local(0) should be live at the return terminator"
        );
    }

    #[test]
    fn liveness_if_else() {
        // bb0: _1 = const 1; switchInt(_1) -> [1: bb1, otherwise: bb2]
        // bb1: _0 = const 10; goto bb3
        // bb2: _0 = const 20; goto bb3
        // bb3: return
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
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
            ],
            blocks: vec![
                // bb0
                BasicBlock {
                    statements: vec![Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Bool(true))),
                        span: Span::DUMMY,
                    }],
                    terminator: Terminator::SwitchInt {
                        discriminant: Operand::Copy(Place::from_local(Local(1))),
                        targets: vec![(1, BlockId(1))],
                        otherwise: BlockId(2),
                    },
                    is_cleanup: false,
                },
                // bb1
                BasicBlock {
                    statements: vec![Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(10))),
                        span: Span::DUMMY,
                    }],
                    terminator: Terminator::Goto {
                        target: BlockId(3),
                    },
                    is_cleanup: false,
                },
                // bb2
                BasicBlock {
                    statements: vec![Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(20))),
                        span: Span::DUMMY,
                    }],
                    terminator: Terminator::Goto {
                        target: BlockId(3),
                    },
                    is_cleanup: false,
                },
                // bb3
                BasicBlock {
                    statements: vec![],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
            span: Span::DUMMY,
        };

        let facts = extract_facts(&func);
        let liveness = compute_liveness(&func, &facts);

        // _0 should be live at bb3:0 (return terminator uses it)
        assert!(liveness.is_live_at(Point::new(BlockId(3), 0), Local(0)));

        // _1 should be live at bb0:1 (switchInt uses it)
        assert!(liveness.is_live_at(Point::new(BlockId(0), 1), Local(1)));
    }

    #[test]
    fn liveness_loop() {
        // bb0: _1 = const 0; goto bb1
        // bb1: _0 = copy _1; _1 = _1 + 1; switchInt(_1) -> [10: bb2, otherwise: bb1]
        // bb2: return
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
                    name: None,
                    is_user_variable: true,
                    mutable: true,
                    span: Span::DUMMY,
                },
            ],
            blocks: vec![
                // bb0: _1 = 0; goto bb1
                BasicBlock {
                    statements: vec![Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(0))),
                        span: Span::DUMMY,
                    }],
                    terminator: Terminator::Goto {
                        target: BlockId(1),
                    },
                    is_cleanup: false,
                },
                // bb1: _0 = _1; switchInt(_1) -> [10: bb2, _: bb1]
                BasicBlock {
                    statements: vec![Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(1)))),
                        span: Span::DUMMY,
                    }],
                    terminator: Terminator::SwitchInt {
                        discriminant: Operand::Copy(Place::from_local(Local(1))),
                        targets: vec![(10, BlockId(2))],
                        otherwise: BlockId(1),
                    },
                    is_cleanup: false,
                },
                // bb2: return
                BasicBlock {
                    statements: vec![],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
            span: Span::DUMMY,
        };

        let facts = extract_facts(&func);
        let liveness = compute_liveness(&func, &facts);

        // _1 should be live at the start of bb1 (it's used in statements and terminator)
        assert!(liveness.is_live_at(Point::new(BlockId(1), 0), Local(1)));

        // _0 should be live at bb2:0 (return)
        assert!(liveness.is_live_at(Point::new(BlockId(2), 0), Local(0)));
    }
}
