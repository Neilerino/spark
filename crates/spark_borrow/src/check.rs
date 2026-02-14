//! Conflict detection and borrow validation.
//!
//! Walks each program point and checks for violations of the borrowing rules:
//! - No two active borrows where one or both are mutable
//! - No use of a moved value
//! - No borrow of a moved value
//! - No mutation through an immutable reference

use rustc_hash::{FxHashMap, FxHashSet};

use spark_mir::{Local, MirFunction, Place, Projection, Statement};

use crate::errors;
use crate::facts::{BorrowFacts, Loan, LoanId, Point};
use crate::regions::Region;

/// Runs all borrow checks on a single function.
pub fn check_borrows(
    func: &MirFunction,
    facts: &BorrowFacts,
    regions: &[Region],
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    check_conflicting_borrows(func, facts, regions, diagnostics);
    check_use_after_move(func, facts, diagnostics);
    check_borrow_of_moved(func, facts, diagnostics);
    check_mutation_through_immutable_ref(func, facts, diagnostics);
}

/// Checks for conflicting borrows at each program point.
///
/// For every pair of loans that are both active at some program point, checks
/// whether they conflict (overlapping places with at least one mutable borrow).
fn check_conflicting_borrows(
    func: &MirFunction,
    facts: &BorrowFacts,
    regions: &[Region],
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    if facts.loans.len() < 2 {
        return;
    }

    // Build a map from LoanId to the loan.
    let loan_map: FxHashMap<LoanId, &Loan> =
        facts.loans.iter().map(|l| (l.id, l)).collect();

    // Collect all unique program points across all regions.
    let mut all_points: FxHashSet<Point> = FxHashSet::default();
    for region in regions {
        for &point in &region.points {
            all_points.insert(point);
        }
    }

    // Track which pairs have already been reported to avoid duplicates.
    let mut reported: FxHashSet<(LoanId, LoanId)> = FxHashSet::default();

    // For each point, find which loans are active and check for conflicts.
    for point in &all_points {
        let active_loans: Vec<LoanId> = regions
            .iter()
            .filter(|r| r.points.contains(point))
            .map(|r| r.loan)
            .collect();

        for i in 0..active_loans.len() {
            for j in (i + 1)..active_loans.len() {
                let first_id = active_loans[i];
                let second_id = active_loans[j];

                let pair = if first_id.0 < second_id.0 {
                    (first_id, second_id)
                } else {
                    (second_id, first_id)
                };

                if reported.contains(&pair) {
                    continue;
                }

                let first_loan = loan_map[&first_id];
                let second_loan = loan_map[&second_id];

                if places_overlap(&first_loan.place, &second_loan.place)
                    && (first_loan.mutable || second_loan.mutable)
                {
                    reported.insert(pair);
                    errors::emit_conflicting_borrows(
                        diagnostics, first_loan, second_loan, func,
                    );
                }
            }
        }
    }
}

/// Checks for use of a moved value.
///
/// If a local was moved at some point and then used at a later point without
/// an intervening reassignment, that's an error.
#[allow(clippy::cast_possible_truncation)]
fn check_use_after_move(
    func: &MirFunction,
    facts: &BorrowFacts,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    if facts.moves.is_empty() {
        return;
    }

    // Build a map of move points per local.
    let mut moves_by_local: FxHashMap<Local, Vec<Point>> = FxHashMap::default();
    for &(point, local) in &facts.moves {
        moves_by_local.entry(local).or_default().push(point);
    }

    // Build a map of assignment (def) points per local.
    let mut assigns_by_local: FxHashMap<Local, FxHashSet<Point>> = FxHashMap::default();
    for &(point, local) in &facts.assignments {
        assigns_by_local.entry(local).or_default().insert(point);
    }

    // For each use, check if there's a preceding move with no intervening assignment.
    for &(use_point, local) in &facts.uses {
        let Some(move_points) = moves_by_local.get(&local) else {
            continue;
        };

        let assigns = assigns_by_local.get(&local);

        for &move_point in move_points {
            // Only report if the move happens before the use.
            if is_before(move_point, use_point, func)
                && !has_intervening_assign(move_point, use_point, assigns, facts)
            {
                errors::emit_use_after_move(diagnostics, use_point, move_point, local, func);
                // Only report one error per use-move pair.
                break;
            }
        }
    }
}

/// Checks for borrows of moved values.
fn check_borrow_of_moved(
    func: &MirFunction,
    facts: &BorrowFacts,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    if facts.loans.is_empty() || facts.moves.is_empty() {
        return;
    }

    let mut moves_by_local: FxHashMap<Local, Vec<Point>> = FxHashMap::default();
    for &(point, local) in &facts.moves {
        moves_by_local.entry(local).or_default().push(point);
    }

    let mut assigns_by_local: FxHashMap<Local, FxHashSet<Point>> = FxHashMap::default();
    for &(point, local) in &facts.assignments {
        assigns_by_local.entry(local).or_default().insert(point);
    }

    for loan in &facts.loans {
        let borrowed_local = loan.place.local;
        let Some(move_points) = moves_by_local.get(&borrowed_local) else {
            continue;
        };

        let assigns = assigns_by_local.get(&borrowed_local);

        for &move_point in move_points {
            if is_before(move_point, loan.issued_at, func)
                && !has_intervening_assign(move_point, loan.issued_at, assigns, facts)
            {
                errors::emit_borrow_of_moved(diagnostics, loan, move_point, func);
                break;
            }
        }
    }
}

/// Checks for mutation through an immutable reference.
///
/// If an `Assign` statement targets a place that goes through an immutable
/// deref (the root is a `Ref { mutable: false }`), that's an error.
#[allow(clippy::cast_possible_truncation)]
fn check_mutation_through_immutable_ref(
    func: &MirFunction,
    facts: &BorrowFacts,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    // Build a set of locals that are immutable refs (assigned via Rvalue::Ref { mutable: false }).
    let immutable_ref_locals: FxHashSet<Local> = facts
        .loans
        .iter()
        .filter(|loan| !loan.mutable)
        .map(|loan| loan.ref_local)
        .collect();

    for (block_idx, block) in func.blocks.iter().enumerate() {
        for (stmt_idx, stmt) in block.statements.iter().enumerate() {
            if let Statement::Assign { place, .. } = stmt {
                if is_deref_of_immutable(place, &immutable_ref_locals) {
                    let point = Point::new(
                        spark_mir::BlockId(block_idx as u32),
                        stmt_idx as u32,
                    );
                    errors::emit_mutation_through_immutable_ref(diagnostics, point, func);
                }
            }
        }
    }
}

/// Returns true if a place writes through an immutable-ref deref.
fn is_deref_of_immutable(place: &Place, immutable_ref_locals: &FxHashSet<Local>) -> bool {
    // Check if the place has a Deref projection and the root local is an immutable ref.
    if place.projections.iter().any(|p| matches!(p, Projection::Deref))
        && immutable_ref_locals.contains(&place.local)
    {
        return true;
    }
    false
}

/// Returns true if two places potentially overlap.
///
/// Two places overlap if they share the same root local and one is a prefix
/// of the other's projection path.
fn places_overlap(a: &Place, b: &Place) -> bool {
    if a.local != b.local {
        return false;
    }

    // If either has no projections, they overlap (one is a prefix of the other).
    if a.projections.is_empty() || b.projections.is_empty() {
        return true;
    }

    // Check if projections form a prefix relationship.
    let min_len = a.projections.len().min(b.projections.len());
    for i in 0..min_len {
        if !projections_equal(&a.projections[i], &b.projections[i]) {
            return false;
        }
    }

    true
}

/// Compares two projections for equality.
fn projections_equal(a: &Projection, b: &Projection) -> bool {
    match (a, b) {
        (Projection::Field(f1), Projection::Field(f2)) => f1 == f2,
        (Projection::Deref, Projection::Deref)
        | (Projection::Index(_), Projection::Index(_)) => true,
        (Projection::UnionPayload(t1), Projection::UnionPayload(t2)) => t1 == t2,
        _ => false,
    }
}

/// Returns true if `before` comes before `after` in the same function.
///
/// Simple heuristic: same block with lower index, or different block where
/// `before` can reach `after` (we use the issue order as a proxy).
fn is_before(before: Point, after: Point, _func: &MirFunction) -> bool {
    if before.block == after.block {
        return before.index < after.index;
    }
    // Cross-block: use block ID as a rough ordering heuristic.
    // A proper implementation would do reachability, but for the common
    // case (straight-line code, simple control flow) this suffices.
    before.block.0 < after.block.0
}

/// Returns true if there is an assignment to the local between two points.
fn has_intervening_assign(
    move_point: Point,
    use_point: Point,
    assigns: Option<&FxHashSet<Point>>,
    _facts: &BorrowFacts,
) -> bool {
    let Some(assigns) = assigns else {
        return false;
    };

    // Simple check: is there an assignment at a point between the two?
    for &assign_point in assigns {
        if is_between(move_point, assign_point, use_point) {
            return true;
        }
    }

    false
}

/// Returns true if `middle` is between `before` and `after`.
fn is_between(before: Point, middle: Point, after: Point) -> bool {
    // Same block case.
    if before.block == middle.block && middle.block == after.block {
        return before.index < middle.index && middle.index < after.index;
    }

    // Cross-block: rough ordering by block ID.
    if before.block == middle.block {
        return middle.index > before.index && middle.block.0 <= after.block.0;
    }
    if middle.block == after.block {
        return middle.index < after.index && before.block.0 <= middle.block.0;
    }

    before.block.0 < middle.block.0 && middle.block.0 < after.block.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::extract_facts;
    use crate::liveness::compute_liveness;
    use crate::regions::compute_regions;
    use spark_diagnostics::Diagnostics;
    use spark_mir::{
        BasicBlock, Constant, Local, LocalDecl, MirFunction, Operand, Place, Projection, Rvalue,
        Statement, Terminator,
    };
    use spark_resolve::DefId;
    use spark_span::Span;
    use spark_types::TypeId;

    /// Helper: creates a function, extracts facts, runs liveness + regions + check.
    fn check_func(func: &MirFunction) -> Diagnostics {
        let facts = extract_facts(func);
        let liveness = compute_liveness(func, &facts);
        let regions = compute_regions(&facts, &liveness);
        let mut diagnostics = Diagnostics::new();
        check_borrows(func, &facts, &regions, &mut diagnostics);
        diagnostics
    }

    #[test]
    fn no_borrows_no_errors() {
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: vec![
                LocalDecl { local: Local(0), ty: TypeId::ERROR, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(1), ty: TypeId::ERROR, name: Some(spark_intern::sym("x")), is_user_variable: true, mutable: false, span: Span::DUMMY },
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
            captures: vec![],
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(!diags.has_errors(), "Expected no errors for simple copy code");
    }

    #[test]
    fn multiple_immutable_borrows_ok() {
        // _1 = const 42
        // _2 = Ref(_1) immut
        // _3 = Ref(_1) immut
        // _4 = copy _2.*
        // _5 = copy _3.*
        // _0 = _4
        // return
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: (0..6).map(|i| LocalDecl {
                local: Local(i),
                ty: TypeId::ERROR,
                name: if i == 1 { Some(spark_intern::sym("x")) } else { None },
                is_user_variable: i > 0,
                mutable: false,
                span: Span::DUMMY,
            }).collect(),
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Ref { place: Place::from_local(Local(1)), mutable: false },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(3)),
                        rvalue: Rvalue::Ref { place: Place::from_local(Local(1)), mutable: false },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(4)),
                        rvalue: Rvalue::Use(Operand::Copy({
                            let mut p = Place::from_local(Local(2));
                            p.projections.push(Projection::Deref);
                            p
                        })),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(5)),
                        rvalue: Rvalue::Use(Operand::Copy({
                            let mut p = Place::from_local(Local(3));
                            p.projections.push(Projection::Deref);
                            p
                        })),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(4)))),
                        span: Span::DUMMY,
                    },
                ],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(
            !diags.has_errors(),
            "Multiple immutable borrows should be allowed: {:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn conflicting_mutable_borrows_error() {
        // _1 = const 42
        // _2 = Ref(_1) mut
        // _3 = Ref(_1) mut
        // _4 = copy _2.*
        // _5 = copy _3.*
        // _0 = _4
        // return
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: (0..6).map(|i| LocalDecl {
                local: Local(i),
                ty: TypeId::ERROR,
                name: if i == 1 { Some(spark_intern::sym("x")) } else { None },
                is_user_variable: i > 0,
                mutable: i == 1,
                span: Span::DUMMY,
            }).collect(),
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Ref { place: Place::from_local(Local(1)), mutable: true },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(3)),
                        rvalue: Rvalue::Ref { place: Place::from_local(Local(1)), mutable: true },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(4)),
                        rvalue: Rvalue::Use(Operand::Copy({
                            let mut p = Place::from_local(Local(2));
                            p.projections.push(Projection::Deref);
                            p
                        })),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(5)),
                        rvalue: Rvalue::Use(Operand::Copy({
                            let mut p = Place::from_local(Local(3));
                            p.projections.push(Projection::Deref);
                            p
                        })),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(4)))),
                        span: Span::DUMMY,
                    },
                ],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(
            diags.has_errors(),
            "Two mutable borrows of the same place should be an error"
        );
    }

    #[test]
    fn mixed_mut_immut_borrows_error() {
        // _1 = const 42
        // _2 = Ref(_1) immut
        // _3 = Ref(_1) mut
        // _4 = copy _2.*
        // _5 = copy _3.*
        // _0 = _4
        // return
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: (0..6).map(|i| LocalDecl {
                local: Local(i),
                ty: TypeId::ERROR,
                name: if i == 1 { Some(spark_intern::sym("x")) } else { None },
                is_user_variable: i > 0,
                mutable: i == 1,
                span: Span::DUMMY,
            }).collect(),
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Ref { place: Place::from_local(Local(1)), mutable: false },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(3)),
                        rvalue: Rvalue::Ref { place: Place::from_local(Local(1)), mutable: true },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(4)),
                        rvalue: Rvalue::Use(Operand::Copy({
                            let mut p = Place::from_local(Local(2));
                            p.projections.push(Projection::Deref);
                            p
                        })),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(5)),
                        rvalue: Rvalue::Use(Operand::Copy({
                            let mut p = Place::from_local(Local(3));
                            p.projections.push(Projection::Deref);
                            p
                        })),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(4)))),
                        span: Span::DUMMY,
                    },
                ],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(
            diags.has_errors(),
            "Mixed mutable + immutable borrows of the same place should be an error"
        );
    }

    #[test]
    fn use_after_move_error() {
        // _1 = const 42
        // _2 = move _1    (move)
        // _0 = copy _1    (use after move — should be error)
        // return
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: (0..3).map(|i| LocalDecl {
                local: Local(i),
                ty: TypeId::ERROR,
                name: if i == 1 { Some(spark_intern::sym("x")) } else { None },
                is_user_variable: i > 0,
                mutable: false,
                span: Span::DUMMY,
            }).collect(),
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Use(Operand::Move(Place::from_local(Local(1)))),
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
            captures: vec![],
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(
            diags.has_errors(),
            "Use after move should be an error"
        );
    }

    #[test]
    fn places_overlap_same_local() {
        let a = Place::from_local(Local(1));
        let b = Place::from_local(Local(1));
        assert!(places_overlap(&a, &b));
    }

    #[test]
    fn places_no_overlap_different_locals() {
        let a = Place::from_local(Local(1));
        let b = Place::from_local(Local(2));
        assert!(!places_overlap(&a, &b));
    }

    #[test]
    fn places_overlap_prefix() {
        let a = Place::from_local(Local(1));
        let mut b = Place::from_local(Local(1));
        b.projections.push(Projection::Field(spark_intern::sym("x")));
        assert!(places_overlap(&a, &b));
    }

    #[test]
    fn function_without_refs_passes() {
        // No refs, no borrows — should have zero diagnostics.
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 1,
            return_type: TypeId::ERROR,
            locals: vec![
                LocalDecl { local: Local(0), ty: TypeId::ERROR, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(1), ty: TypeId::ERROR, name: Some(spark_intern::sym("x")), is_user_variable: true, mutable: false, span: Span::DUMMY },
            ],
            blocks: vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(1)))),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(!diags.has_errors());
    }

    #[test]
    fn mutation_through_immutable_ref_error() {
        // _1 = const 42
        // _2 = Ref(_1) immut
        // _2.* = const 99      (mutation through immutable ref — should error)
        // _0 = void
        // return
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: (0..3).map(|i| LocalDecl {
                local: Local(i),
                ty: TypeId::ERROR,
                name: if i == 1 { Some(spark_intern::sym("x")) } else { None },
                is_user_variable: i > 0,
                mutable: false,
                span: Span::DUMMY,
            }).collect(),
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Ref { place: Place::from_local(Local(1)), mutable: false },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: {
                            let mut p = Place::from_local(Local(2));
                            p.projections.push(Projection::Deref);
                            p
                        },
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(99))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Void)),
                        span: Span::DUMMY,
                    },
                ],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(
            diags.has_errors(),
            "Mutation through immutable reference should be an error"
        );
    }
}
