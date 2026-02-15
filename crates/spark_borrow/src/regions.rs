//! NLL-style region inference.
//!
//! Computes the **region** (set of program points) where each loan must be
//! active. A loan's region extends from its issue point to the last use of
//! the local holding the reference.

use rustc_hash::FxHashSet;

use crate::facts::{BorrowFacts, Loan, LoanId, Point};
use crate::liveness::LivenessResult;

/// A region is the set of program points where a borrow is active.
#[derive(Debug)]
pub struct Region {
    /// The loan this region belongs to.
    pub loan: LoanId,
    /// The set of program points where the loan is active.
    pub points: FxHashSet<Point>,
}

/// Computes regions for all loans.
///
/// A loan's region includes all program points that are:
/// 1. Reachable from the issue point via CFG edges, AND
/// 2. At which the local holding the reference is live.
///
/// This is the NLL approach: the borrow is active as long as the reference
/// it produced might still be used.
#[must_use]
pub fn compute_regions(
    facts: &BorrowFacts,
    liveness: &LivenessResult,
) -> Vec<Region> {
    facts
        .loans
        .iter()
        .map(|loan| compute_loan_region(loan, facts, liveness))
        .collect()
}

/// Computes the region for a single loan.
fn compute_loan_region(
    loan: &Loan,
    facts: &BorrowFacts,
    liveness: &LivenessResult,
) -> Region {
    let ref_local = loan.ref_local;
    let mut points = FxHashSet::default();
    let mut worklist = vec![loan.issued_at];

    while let Some(point) = worklist.pop() {
        if !points.insert(point) {
            continue;
        }

        // Only extend to successors if the ref local is still live at this point.
        if liveness.is_live_at(point, ref_local) || point == loan.issued_at {
            for &(from, to) in &facts.cfg_edges {
                if from == point && !points.contains(&to) {
                    worklist.push(to);
                }
            }
        }
    }

    Region {
        loan: loan.id,
        points,
    }
}

/// Returns the region for a given loan ID, if it exists.
#[must_use]
pub fn region_for_loan(regions: &[Region], loan_id: LoanId) -> Option<&Region> {
    regions.iter().find(|r| r.loan == loan_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::extract_facts;
    use crate::liveness::compute_liveness;
    use spark_mir::{
        BasicBlock, BlockId, Constant, Local, LocalDecl, MirFunction, Operand, Place, Rvalue,
        Statement, Terminator,
    };
    use spark_resolve::DefId;
    use spark_span::Span;
    use spark_types::TypeId;

    #[test]
    fn region_extends_to_last_use() {
        // bb0: _1 = const 42      (stmt 0)
        //      _2 = Ref(_1)       (stmt 1) — loan created, _2 holds ref
        //      _3 = copy _2.*     (stmt 2) — use of _2 (deref)
        //      _0 = copy _3       (stmt 3)
        //      return             (term, stmt 4)
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: vec![
                LocalDecl { local: Local(0), ty: TypeId::ERROR, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(1), ty: TypeId::ERROR, name: Some(spark_intern::sym("x")), is_user_variable: true, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(2), ty: TypeId::ERROR, name: Some(spark_intern::sym("r")), is_user_variable: true, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(3), ty: TypeId::ERROR, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
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
                        rvalue: Rvalue::Ref { place: Place::from_local(Local(1)), mutable: false },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(3)),
                        rvalue: Rvalue::Use(Operand::Copy({
                            let mut p = Place::from_local(Local(2));
                            p.projections.push(spark_mir::Projection::Deref);
                            p
                        })),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(3)))),
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

        let facts = extract_facts(&func);
        let liveness = compute_liveness(&func, &facts);
        let regions = compute_regions(&facts, &liveness);

        assert_eq!(regions.len(), 1);
        let region = &regions[0];

        // The loan was issued at bb0:1
        assert!(region.points.contains(&Point::new(BlockId(0), 1)));

        // The ref local (_2) is used at bb0:2, so the region should include bb0:2
        assert!(region.points.contains(&Point::new(BlockId(0), 2)));
    }

    #[test]
    fn no_regions_without_loans() {
        let func = MirFunction {
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
            blocks: vec![BasicBlock {
                statements: vec![],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };

        let facts = extract_facts(&func);
        let liveness = compute_liveness(&func, &facts);
        let regions = compute_regions(&facts, &liveness);

        assert!(regions.is_empty());
    }
}
