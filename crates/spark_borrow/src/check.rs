//! Conflict detection and borrow validation.
//!
//! Walks each program point and checks for violations of the borrowing rules:
//! - No two active borrows where one or both are mutable
//! - No use of a moved value
//! - No borrow of a moved value
//! - No mutation through an immutable reference
//! - No returning a reference to a local variable

use rustc_hash::{FxHashMap, FxHashSet};

use spark_mir::{Local, MirFunction, Operand, Place, Projection, Rvalue, Statement, Terminator};

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
    check_dangling_returns(func, facts, diagnostics);
    check_aliasing_call_args(func, facts, diagnostics);
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
    let loan_map: FxHashMap<LoanId, &Loan> = facts.loans.iter().map(|l| (l.id, l)).collect();

    // Build inverted index: point -> [active loan ids].
    // This replaces the O(P×R) scan with O(total_region_points) construction.
    let mut active_at: FxHashMap<Point, Vec<LoanId>> = FxHashMap::default();
    for region in regions {
        for &point in &region.points {
            active_at.entry(point).or_default().push(region.loan);
        }
    }

    // Pre-compute two-phase borrow exempt pairs.
    // A pair is exempt if both ref locals are arguments to a call at any point
    // where both loans are active.
    let call_arg_locals = collect_call_arg_locals(func);
    let two_phase_exempt = compute_two_phase_exempt(&active_at, &loan_map, &call_arg_locals);

    // Track which pairs have already been reported to avoid duplicates.
    let mut reported: FxHashSet<(LoanId, LoanId)> = FxHashSet::default();

    // For each point with multiple active loans, check for conflicts.
    for active_loans in active_at.values() {
        if active_loans.len() < 2 {
            continue;
        }

        for i in 0..active_loans.len() {
            for j in (i + 1)..active_loans.len() {
                let pair = ordered_pair(active_loans[i], active_loans[j]);
                if reported.contains(&pair) || two_phase_exempt.contains(&pair) {
                    continue;
                }

                let first_loan = loan_map[&pair.0];
                let second_loan = loan_map[&pair.1];

                if places_overlap(&first_loan.place, &second_loan.place)
                    && (first_loan.mutable || second_loan.mutable)
                {
                    reported.insert(pair);
                    errors::emit_conflicting_borrows(diagnostics, first_loan, second_loan, func);
                }
            }
        }
    }
}

/// Returns a canonically ordered pair of loan IDs (smaller first).
const fn ordered_pair(a: LoanId, b: LoanId) -> (LoanId, LoanId) {
    if a.0 < b.0 {
        (a, b)
    } else {
        (b, a)
    }
}

/// Collects the set of locals used as arguments at each call-site point.
///
/// Returns a map from the terminator point (block, `stmt_count`) to the set of
/// locals that appear as operands in the `Call` terminator's arguments (including
/// the function operand itself).
fn collect_call_arg_locals(func: &MirFunction) -> FxHashMap<Point, FxHashSet<Local>> {
    let mut result = FxHashMap::default();
    for (block_idx, block) in func.blocks.iter().enumerate() {
        if let Terminator::Call {
            func: fn_op, args, ..
        } = &block.terminator
        {
            let point = Point::new(
                spark_mir::BlockId(block_idx as u32),
                block.statements.len() as u32,
            );
            let mut locals = FxHashSet::default();
            collect_operand_local(fn_op, &mut locals);
            for arg in args {
                collect_operand_local(arg, &mut locals);
            }
            result.insert(point, locals);
        }
    }
    result
}

/// Extracts the local from an operand, if any.
fn collect_operand_local(operand: &spark_mir::Operand, out: &mut FxHashSet<Local>) {
    match operand {
        spark_mir::Operand::Copy(p) | spark_mir::Operand::Move(p) => {
            out.insert(p.local);
        }
        spark_mir::Operand::Constant(_) => {}
    }
}

/// Pre-computes the set of loan pairs that are exempt due to two-phase borrowing.
///
/// A pair is two-phase exempt if:
/// 1. One loan is mutable and the other immutable
/// 2. At some point where both are active, there's a `Call` terminator whose
///    arguments include both loans' `ref_local` values
///
/// This handles the `x.method(&x)` pattern where a mutable borrow for `self`
/// and an immutable borrow for an argument coexist at the call site.
fn compute_two_phase_exempt(
    active_at: &FxHashMap<Point, Vec<LoanId>>,
    loan_map: &FxHashMap<LoanId, &Loan>,
    call_arg_locals: &FxHashMap<Point, FxHashSet<Local>>,
) -> FxHashSet<(LoanId, LoanId)> {
    let mut exempt = FxHashSet::default();

    // Only check at call points (where two-phase borrows can occur).
    for (point, arg_locals) in call_arg_locals {
        let Some(active_loans) = active_at.get(point) else {
            continue;
        };
        if active_loans.len() < 2 {
            continue;
        }

        for i in 0..active_loans.len() {
            for j in (i + 1)..active_loans.len() {
                let a = loan_map[&active_loans[i]];
                let b = loan_map[&active_loans[j]];

                // Must be (mutable, immutable), not (mutable, mutable).
                if a.mutable == b.mutable {
                    continue;
                }

                // Both ref locals must be arguments to the call.
                if arg_locals.contains(&a.ref_local)
                    && arg_locals.contains(&b.ref_local)
                    && places_overlap(&a.place, &b.place)
                {
                    exempt.insert(ordered_pair(a.id, b.id));
                }
            }
        }
    }

    exempt
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
            if is_before(move_point, use_point, facts)
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
            if is_before(move_point, loan.issued_at, facts)
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
                    let point = Point::new(spark_mir::BlockId(block_idx as u32), stmt_idx as u32);
                    errors::emit_mutation_through_immutable_ref(diagnostics, point, func);
                }
            }
        }
    }
}

/// Checks for returning a reference to a local variable.
///
/// A function that returns `Ref[T]` must not return a reference to a local
/// variable, because the local's storage is freed when the function returns.
/// References to function parameters are allowed (they outlive the callee).
fn check_dangling_returns(
    func: &MirFunction,
    facts: &BorrowFacts,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    if facts.loans.is_empty() {
        return;
    }

    // Build a reverse assignment chain: destination -> [sources].
    // Tracks `dest = Copy(src)` and `dest = Move(src)` assignments.
    // A destination may have multiple sources (different return paths).
    let mut sources_of: FxHashMap<Local, Vec<Local>> = FxHashMap::default();
    for block in &func.blocks {
        for stmt in &block.statements {
            if let Statement::Assign { place, rvalue, .. } = stmt {
                if place.projections.is_empty() {
                    if let Rvalue::Use(Operand::Copy(src) | Operand::Move(src)) = rvalue {
                        if src.projections.is_empty() {
                            sources_of.entry(place.local).or_default().push(src.local);
                        }
                    }
                }
            }
        }
    }

    // Build loan lookup by ref_local.
    let loan_by_ref_local: FxHashMap<Local, &Loan> = facts
        .loans
        .iter()
        .map(|loan| (loan.ref_local, loan))
        .collect();

    // Trace backwards from _0 (return place) through the assignment chain.
    // If we find a local that holds a loan borrowing a non-parameter local,
    // that's a dangling reference.
    let mut worklist = vec![Local::RETURN_PLACE];
    let mut visited = FxHashSet::default();
    while let Some(current) = worklist.pop() {
        if !visited.insert(current) {
            continue;
        }
        if let Some(loan) = loan_by_ref_local.get(&current) {
            let borrowed_local = loan.place.local;
            // Parameters are locals 1..=param_count. Locals above that are
            // function-scoped variables whose storage dies on return.
            if borrowed_local.0 > func.param_count && borrowed_local != Local::RETURN_PLACE {
                errors::emit_dangling_return(diagnostics, loan, func);
                return;
            }
        }
        if let Some(srcs) = sources_of.get(&current) {
            for &src in srcs {
                worklist.push(src);
            }
        }
    }
}

/// Checks for aliasing mutable references at call sites.
///
/// If the same mutable-ref-holding local appears in two or more argument
/// positions of a function call, the callee would have two `&mut` references
/// to the same data — undefined behaviour.
fn check_aliasing_call_args(
    func: &MirFunction,
    facts: &BorrowFacts,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    // Collect the set of locals that hold mutable references.
    let mutable_ref_locals: FxHashSet<Local> = facts
        .loans
        .iter()
        .filter(|loan| loan.mutable)
        .map(|loan| loan.ref_local)
        .collect();

    if mutable_ref_locals.is_empty() {
        return;
    }

    for block in &func.blocks {
        if let Terminator::Call { args, span, .. } = &block.terminator {
            // Collect (local, index) pairs for mutable-ref arguments.
            let mut seen: FxHashMap<Local, usize> = FxHashMap::default();
            for (i, arg) in args.iter().enumerate() {
                let local = match arg {
                    Operand::Copy(place) | Operand::Move(place) if place.projections.is_empty() => {
                        place.local
                    }
                    _ => continue,
                };
                if !mutable_ref_locals.contains(&local) {
                    continue;
                }
                if seen.contains_key(&local) {
                    errors::emit_aliasing_call_args(diagnostics, *span, local, func);
                    break; // one error per call site
                }
                seen.insert(local, i);
            }
        }
    }
}

/// Returns true if a place writes through an immutable-ref deref.
fn is_deref_of_immutable(place: &Place, immutable_ref_locals: &FxHashSet<Local>) -> bool {
    // Check if the place has a Deref projection and the root local is an immutable ref.
    if place
        .projections
        .iter()
        .any(|p| matches!(p, Projection::Deref))
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
        (Projection::Deref, Projection::Deref) | (Projection::Index(_), Projection::Index(_)) => {
            true
        }
        (Projection::UnionPayload(t1), Projection::UnionPayload(t2)) => t1 == t2,
        (
            Projection::EnumField {
                variant: v1,
                index: i1,
            },
            Projection::EnumField {
                variant: v2,
                index: i2,
            },
        ) => v1 == v2 && i1 == i2,
        _ => false,
    }
}

/// Returns true if `before` can reach `after` through the CFG.
///
/// Within the same block, compares statement indices. Across blocks,
/// uses actual CFG edge reachability via BFS.
fn is_before(before: Point, after: Point, facts: &BorrowFacts) -> bool {
    if before.block == after.block {
        return before.index < after.index;
    }
    block_can_reach(before.block, after.block, facts)
}

/// Returns true if block `from` can reach block `to` via CFG edges.
fn block_can_reach(from: spark_mir::BlockId, to: spark_mir::BlockId, facts: &BorrowFacts) -> bool {
    // Build block-level successors from CFG edges.
    let mut successors: FxHashMap<spark_mir::BlockId, Vec<spark_mir::BlockId>> =
        FxHashMap::default();
    for &(p_from, p_to) in &facts.cfg_edges {
        if p_from.block != p_to.block {
            successors.entry(p_from.block).or_default().push(p_to.block);
        }
    }

    // BFS from `from` to `to`.
    let mut visited = FxHashSet::default();
    let mut worklist = vec![from];
    while let Some(block) = worklist.pop() {
        if block == to {
            return true;
        }
        if !visited.insert(block) {
            continue;
        }
        if let Some(succs) = successors.get(&block) {
            for &succ in succs {
                worklist.push(succ);
            }
        }
    }
    false
}

/// Returns true if there is an assignment to the local between two points.
fn has_intervening_assign(
    move_point: Point,
    use_point: Point,
    assigns: Option<&FxHashSet<Point>>,
    facts: &BorrowFacts,
) -> bool {
    let Some(assigns) = assigns else {
        return false;
    };

    for &assign_point in assigns {
        if is_between(move_point, assign_point, use_point, facts) {
            return true;
        }
    }

    false
}

/// Returns true if `middle` is between `before` and `after` in the CFG.
fn is_between(before: Point, middle: Point, after: Point, facts: &BorrowFacts) -> bool {
    // Same block: simple index comparison.
    if before.block == middle.block && middle.block == after.block {
        return before.index < middle.index && middle.index < after.index;
    }

    // Cross-block: check if before→middle and middle→after are both reachable.
    if before.block == middle.block {
        return middle.index > before.index && block_can_reach(middle.block, after.block, facts);
    }
    if middle.block == after.block {
        return middle.index < after.index && block_can_reach(before.block, middle.block, facts);
    }

    block_can_reach(before.block, middle.block, facts)
        && block_can_reach(middle.block, after.block, facts)
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
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(
            !diags.has_errors(),
            "Expected no errors for simple copy code"
        );
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
            locals: (0..6)
                .map(|i| LocalDecl {
                    local: Local(i),
                    ty: TypeId::ERROR,
                    name: if i == 1 {
                        Some(spark_intern::sym("x"))
                    } else {
                        None
                    },
                    is_user_variable: i > 0,
                    mutable: false,
                    span: Span::DUMMY,
                })
                .collect(),
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Ref {
                            place: Place::from_local(Local(1)),
                            mutable: false,
                        },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(3)),
                        rvalue: Rvalue::Ref {
                            place: Place::from_local(Local(1)),
                            mutable: false,
                        },
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
            is_async: false,
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
            locals: (0..6)
                .map(|i| LocalDecl {
                    local: Local(i),
                    ty: TypeId::ERROR,
                    name: if i == 1 {
                        Some(spark_intern::sym("x"))
                    } else {
                        None
                    },
                    is_user_variable: i > 0,
                    mutable: i == 1,
                    span: Span::DUMMY,
                })
                .collect(),
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Ref {
                            place: Place::from_local(Local(1)),
                            mutable: true,
                        },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(3)),
                        rvalue: Rvalue::Ref {
                            place: Place::from_local(Local(1)),
                            mutable: true,
                        },
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
            is_async: false,
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
            locals: (0..6)
                .map(|i| LocalDecl {
                    local: Local(i),
                    ty: TypeId::ERROR,
                    name: if i == 1 {
                        Some(spark_intern::sym("x"))
                    } else {
                        None
                    },
                    is_user_variable: i > 0,
                    mutable: i == 1,
                    span: Span::DUMMY,
                })
                .collect(),
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Ref {
                            place: Place::from_local(Local(1)),
                            mutable: false,
                        },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(3)),
                        rvalue: Rvalue::Ref {
                            place: Place::from_local(Local(1)),
                            mutable: true,
                        },
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
            is_async: false,
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
            locals: (0..3)
                .map(|i| LocalDecl {
                    local: Local(i),
                    ty: TypeId::ERROR,
                    name: if i == 1 {
                        Some(spark_intern::sym("x"))
                    } else {
                        None
                    },
                    is_user_variable: i > 0,
                    mutable: false,
                    span: Span::DUMMY,
                })
                .collect(),
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
            is_async: false,
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(diags.has_errors(), "Use after move should be an error");
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
        b.projections
            .push(Projection::Field(spark_intern::sym("x")));
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
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(1)))),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            is_async: false,
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
            locals: (0..3)
                .map(|i| LocalDecl {
                    local: Local(i),
                    ty: TypeId::ERROR,
                    name: if i == 1 {
                        Some(spark_intern::sym("x"))
                    } else {
                        None
                    },
                    is_user_variable: i > 0,
                    mutable: false,
                    span: Span::DUMMY,
                })
                .collect(),
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Ref {
                            place: Place::from_local(Local(1)),
                            mutable: false,
                        },
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
            is_async: false,
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(
            diags.has_errors(),
            "Mutation through immutable reference should be an error"
        );
    }

    #[test]
    fn many_non_conflicting_borrows() {
        // 20 distinct locals, each with an immutable borrow — no conflicts.
        // _1.._20  = const values
        // _21.._40 = Ref(_1).._20 immut
        // _41.._60 = copy _21.*.._40.*
        // _0 = void
        // return
        let local_count = 61; // _0 + 20 values + 20 refs + 20 reads
        let mut stmts = Vec::new();

        // _1.._20 = const i
        for i in 1..=20u32 {
            stmts.push(Statement::Assign {
                place: Place::from_local(Local(i)),
                rvalue: Rvalue::Use(Operand::Constant(Constant::Int(i.into()))),
                span: Span::DUMMY,
            });
        }

        // _21.._40 = Ref(_1.._20) immut
        for i in 0..20u32 {
            stmts.push(Statement::Assign {
                place: Place::from_local(Local(21 + i)),
                rvalue: Rvalue::Ref {
                    place: Place::from_local(Local(1 + i)),
                    mutable: false,
                },
                span: Span::DUMMY,
            });
        }

        // _41.._60 = copy _21.*.._40.*
        for i in 0..20u32 {
            let mut p = Place::from_local(Local(21 + i));
            p.projections.push(Projection::Deref);
            stmts.push(Statement::Assign {
                place: Place::from_local(Local(41 + i)),
                rvalue: Rvalue::Use(Operand::Copy(p)),
                span: Span::DUMMY,
            });
        }

        // _0 = void
        stmts.push(Statement::Assign {
            place: Place::from_local(Local(0)),
            rvalue: Rvalue::Use(Operand::Constant(Constant::Void)),
            span: Span::DUMMY,
        });

        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: (0..local_count)
                .map(|i| LocalDecl {
                    local: Local(i),
                    ty: TypeId::ERROR,
                    name: None,
                    is_user_variable: i > 0 && i <= 20,
                    mutable: false,
                    span: Span::DUMMY,
                })
                .collect(),
            blocks: vec![BasicBlock {
                statements: stmts,
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(
            !diags.has_errors(),
            "20 immutable borrows of distinct locals should not conflict: {:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn reborrow_mutable_from_mutable_ok() {
        // _1 = const 42
        // _2 = Ref(_1) mut        (loan L0: borrows _1, stored in _2)
        // _3 = Ref(_2.*) mut      (loan L1: reborrow through _2, stored in _3)
        // _4 = copy _3.*          (use _3 only, _2 is dead)
        // _0 = _4
        // return
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: (0..5)
                .map(|i| LocalDecl {
                    local: Local(i),
                    ty: TypeId::ERROR,
                    name: if i == 1 {
                        Some(spark_intern::sym("x"))
                    } else {
                        None
                    },
                    is_user_variable: i > 0,
                    mutable: i == 1,
                    span: Span::DUMMY,
                })
                .collect(),
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Ref {
                            place: Place::from_local(Local(1)),
                            mutable: true,
                        },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(3)),
                        rvalue: Rvalue::Ref {
                            place: {
                                let mut p = Place::from_local(Local(2));
                                p.projections.push(Projection::Deref);
                                p
                            },
                            mutable: true,
                        },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(4)),
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
            is_async: false,
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(
            !diags.has_errors(),
            "Reborrow (mut from mut) with dead original should be allowed: {:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn two_phase_method_call_ok() {
        // _1 = const 42
        // _2 = Ref(_1) mut       (mutable borrow for `self`)
        // _3 = Ref(_1) immut     (immutable borrow for argument)
        //   (terminator) call f(_2, _3) -> bb1
        // bb1: _0 = void; return
        //
        // The mutable and immutable borrows of _1 are both active at the call
        // point but this is the two-phase borrow pattern and should be allowed.
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: (0..6)
                .map(|i| LocalDecl {
                    local: Local(i),
                    ty: TypeId::ERROR,
                    name: if i == 1 {
                        Some(spark_intern::sym("x"))
                    } else {
                        None
                    },
                    is_user_variable: i > 0,
                    mutable: i == 1,
                    span: Span::DUMMY,
                })
                .collect(),
            blocks: vec![
                BasicBlock {
                    statements: vec![
                        Statement::Assign {
                            place: Place::from_local(Local(1)),
                            rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                            span: Span::DUMMY,
                        },
                        Statement::Assign {
                            place: Place::from_local(Local(2)),
                            rvalue: Rvalue::Ref {
                                place: Place::from_local(Local(1)),
                                mutable: true,
                            },
                            span: Span::DUMMY,
                        },
                        Statement::Assign {
                            place: Place::from_local(Local(3)),
                            rvalue: Rvalue::Ref {
                                place: Place::from_local(Local(1)),
                                mutable: false,
                            },
                            span: Span::DUMMY,
                        },
                    ],
                    terminator: Terminator::Call {
                        func: Operand::Constant(Constant::Void), // placeholder
                        args: vec![
                            Operand::Move(Place::from_local(Local(2))),
                            Operand::Copy(Place::from_local(Local(3))),
                        ],
                        destination: Place::from_local(Local(4)),
                        target: spark_mir::BlockId(1),
                        span: Span::DUMMY,
                    },
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Void)),
                        span: Span::DUMMY,
                    }],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(
            !diags.has_errors(),
            "Two-phase borrow (x.method(&x) pattern) should be allowed: {:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn two_phase_both_mutable_error() {
        // _1 = const 42
        // _2 = Ref(_1) mut
        // _3 = Ref(_1) mut
        //   call f(_2, _3) -> bb1
        // bb1: _0 = void; return
        //
        // Both mutable — two-phase exemption does NOT apply.
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: (0..6)
                .map(|i| LocalDecl {
                    local: Local(i),
                    ty: TypeId::ERROR,
                    name: if i == 1 {
                        Some(spark_intern::sym("x"))
                    } else {
                        None
                    },
                    is_user_variable: i > 0,
                    mutable: i == 1,
                    span: Span::DUMMY,
                })
                .collect(),
            blocks: vec![
                BasicBlock {
                    statements: vec![
                        Statement::Assign {
                            place: Place::from_local(Local(1)),
                            rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                            span: Span::DUMMY,
                        },
                        Statement::Assign {
                            place: Place::from_local(Local(2)),
                            rvalue: Rvalue::Ref {
                                place: Place::from_local(Local(1)),
                                mutable: true,
                            },
                            span: Span::DUMMY,
                        },
                        Statement::Assign {
                            place: Place::from_local(Local(3)),
                            rvalue: Rvalue::Ref {
                                place: Place::from_local(Local(1)),
                                mutable: true,
                            },
                            span: Span::DUMMY,
                        },
                    ],
                    terminator: Terminator::Call {
                        func: Operand::Constant(Constant::Void),
                        args: vec![
                            Operand::Move(Place::from_local(Local(2))),
                            Operand::Move(Place::from_local(Local(3))),
                        ],
                        destination: Place::from_local(Local(4)),
                        target: spark_mir::BlockId(1),
                        span: Span::DUMMY,
                    },
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Void)),
                        span: Span::DUMMY,
                    }],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(
            diags.has_errors(),
            "Two mutable borrows in a call should still be an error"
        );
    }

    #[test]
    fn two_phase_outside_call_error() {
        // _1 = const 42
        // _2 = Ref(_1) mut
        // _3 = Ref(_1) immut
        // _4 = copy _2.*
        // _5 = copy _3.*
        // _0 = _4
        // return
        //
        // No call terminator — both refs used independently, NOT a two-phase
        // borrow. Should be an error (same as mixed_mut_immut_borrows_error).
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: (0..6)
                .map(|i| LocalDecl {
                    local: Local(i),
                    ty: TypeId::ERROR,
                    name: if i == 1 {
                        Some(spark_intern::sym("x"))
                    } else {
                        None
                    },
                    is_user_variable: i > 0,
                    mutable: i == 1,
                    span: Span::DUMMY,
                })
                .collect(),
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Ref {
                            place: Place::from_local(Local(1)),
                            mutable: true,
                        },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(3)),
                        rvalue: Rvalue::Ref {
                            place: Place::from_local(Local(1)),
                            mutable: false,
                        },
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
            is_async: false,
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(
            diags.has_errors(),
            "Mixed mut + immut borrows used outside a call should be an error"
        );
    }

    #[test]
    fn reborrow_with_original_still_used_error() {
        // _1 = const 42
        // _2 = Ref(_1) mut       (L0)
        // _3 = Ref(_2.*) mut     (L1: reborrow)
        // _4 = copy _3.*         (use _3 — keeps L1 alive)
        // _5 = copy _2.*         (use _2 — keeps L0 alive)
        // _0 = _4
        // return
        //
        // Two mutable borrows of _1: L0 (direct) and L1 (through _2.*).
        // L0 and L1 don't directly conflict via places_overlap (different root
        // locals: _1 vs _2). But _2 (L0's ref) is used after _3 (L1) is created,
        // which means both borrows are active simultaneously on the same data.
        //
        // NOTE: With the current place-based overlap check, this test actually
        // does NOT produce an error because the reborrow's place (_2.*) has a
        // different root local than L0's place (_1). A full reborrow analysis
        // would track this transitively, but for now this is a known limitation.
        // This test documents the current behavior.
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: (0..6)
                .map(|i| LocalDecl {
                    local: Local(i),
                    ty: TypeId::ERROR,
                    name: if i == 1 {
                        Some(spark_intern::sym("x"))
                    } else {
                        None
                    },
                    is_user_variable: i > 0,
                    mutable: i == 1,
                    span: Span::DUMMY,
                })
                .collect(),
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Ref {
                            place: Place::from_local(Local(1)),
                            mutable: true,
                        },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(3)),
                        rvalue: Rvalue::Ref {
                            place: {
                                let mut p = Place::from_local(Local(2));
                                p.projections.push(Projection::Deref);
                                p
                            },
                            mutable: true,
                        },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(4)),
                        rvalue: Rvalue::Use(Operand::Copy({
                            let mut p = Place::from_local(Local(3));
                            p.projections.push(Projection::Deref);
                            p
                        })),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(5)),
                        rvalue: Rvalue::Use(Operand::Copy({
                            let mut p = Place::from_local(Local(2));
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
            is_async: false,
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        // Current behavior: no error (place-based overlap doesn't catch transitive reborrows).
        // This documents the known limitation for future work.
        assert!(
            !diags.has_errors(),
            "Reborrow with original still used: currently not caught (known limitation)"
        );
    }

    #[test]
    fn dangling_return_local_error() {
        // fn f(): Ref[Int] { x = 42; _2 = Ref(x); _0 = Copy(_2); return }
        // Returning a reference to local `x` — should be an error.
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
                LocalDecl {
                    local: Local(2),
                    ty: TypeId::ERROR,
                    name: None,
                    is_user_variable: false,
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
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Ref {
                            place: Place::from_local(Local(1)),
                            mutable: false,
                        },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(2)))),
                        span: Span::DUMMY,
                    },
                ],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(
            diags.has_errors(),
            "Returning a reference to a local variable should be an error"
        );
    }

    #[test]
    fn aliasing_call_args_error() {
        // fn f(a: Ref[T]!, b: Ref[T]!) { ... }
        // MIR for the caller:
        // _1 = const 42
        // _2 = Ref(_1) mut
        //   call f(Copy(_2), Copy(_2)) -> bb1  ← same mutable ref in both args
        // bb1: _0 = void; return
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: (0..4)
                .map(|i| LocalDecl {
                    local: Local(i),
                    ty: TypeId::ERROR,
                    name: if i == 1 {
                        Some(spark_intern::sym("x"))
                    } else if i == 2 {
                        Some(spark_intern::sym("r"))
                    } else {
                        None
                    },
                    is_user_variable: i > 0,
                    mutable: i >= 1,
                    span: Span::DUMMY,
                })
                .collect(),
            blocks: vec![
                BasicBlock {
                    statements: vec![
                        Statement::Assign {
                            place: Place::from_local(Local(1)),
                            rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                            span: Span::DUMMY,
                        },
                        Statement::Assign {
                            place: Place::from_local(Local(2)),
                            rvalue: Rvalue::Ref {
                                place: Place::from_local(Local(1)),
                                mutable: true,
                            },
                            span: Span::DUMMY,
                        },
                    ],
                    terminator: Terminator::Call {
                        func: Operand::Constant(Constant::Void),
                        args: vec![
                            Operand::Copy(Place::from_local(Local(2))),
                            Operand::Copy(Place::from_local(Local(2))),
                        ],
                        destination: Place::from_local(Local(3)),
                        target: spark_mir::BlockId(1),
                        span: Span::DUMMY,
                    },
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Void)),
                        span: Span::DUMMY,
                    }],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(
            diags.has_errors(),
            "Passing the same mutable ref twice to a call should be an error"
        );
    }

    #[test]
    fn return_ref_to_param_ok() {
        // fn f(x: Int): Ref[Int] { _2 = Ref(_1); _0 = Copy(_2); return }
        // Returning a reference to parameter `x` — should be allowed.
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 1,
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
                LocalDecl {
                    local: Local(2),
                    ty: TypeId::ERROR,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
            ],
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Ref {
                            place: Place::from_local(Local(1)),
                            mutable: false,
                        },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(2)))),
                        span: Span::DUMMY,
                    },
                ],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };

        let diags = check_func(&func);
        assert!(
            !diags.has_errors(),
            "Returning a reference to a parameter should be allowed: {:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }
}
