//! Fact extraction from MIR.
//!
//! Walks each [`MirFunction`]'s CFG and extracts the borrow facts needed
//! for liveness analysis, region inference, and conflict detection.

use rustc_hash::FxHashSet;

use spark_mir::{
    BasicBlock, BlockId, Local, MirFunction, Operand, Place, Projection, Rvalue, Statement,
    Terminator,
};
use spark_types::LifetimeId;

/// A program point: (block, statement index).
///
/// An index equal to the block's statement count represents the terminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Point {
    /// The basic block.
    pub block: BlockId,
    /// Statement index within the block (past last statement = terminator).
    pub index: u32,
}

impl Point {
    /// Creates a new program point.
    #[must_use]
    pub const fn new(block: BlockId, index: u32) -> Self {
        Self { block, index }
    }
}

/// A unique identifier for a loan (borrow).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LoanId(pub u32);

/// Information about a single loan (borrow operation).
#[derive(Debug)]
pub struct Loan {
    /// The unique loan identifier.
    pub id: LoanId,
    /// The place being borrowed.
    pub place: Place,
    /// Whether this is a mutable borrow.
    pub mutable: bool,
    /// The program point where the borrow was issued.
    pub issued_at: Point,
    /// The local that holds the resulting reference.
    pub ref_local: Local,
    /// The lifetime region for this borrow.
    pub lifetime: LifetimeId,
}

/// All borrow facts extracted from a function's MIR.
#[derive(Debug)]
pub struct BorrowFacts {
    /// All loans (borrow operations) in the function.
    pub loans: Vec<Loan>,
    /// CFG edges: `(from, to)` pairs.
    pub cfg_edges: Vec<(Point, Point)>,
    /// Points where locals are assigned.
    pub assignments: Vec<(Point, Local)>,
    /// Points where locals are read.
    pub uses: Vec<(Point, Local)>,
    /// Points where locals are moved.
    pub moves: Vec<(Point, Local)>,
}

/// Extracts borrow facts from a MIR function.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn extract_facts(func: &MirFunction) -> BorrowFacts {
    let mut facts = BorrowFacts {
        loans: Vec::new(),
        cfg_edges: Vec::new(),
        assignments: Vec::new(),
        uses: Vec::new(),
        moves: Vec::new(),
    };

    let mut loan_counter: u32 = 0;

    for (block_idx, block) in func.blocks.iter().enumerate() {
        let block_id = BlockId(block_idx as u32);

        // Build intra-block edges: point(i) -> point(i+1)
        let stmt_count = block.statements.len() as u32;
        for i in 0..stmt_count {
            let from = Point::new(block_id, i);
            let to = Point::new(block_id, i + 1);
            facts.cfg_edges.push((from, to));
        }

        // Extract facts from each statement
        for (stmt_idx, stmt) in block.statements.iter().enumerate() {
            let point = Point::new(block_id, stmt_idx as u32);
            extract_statement_facts(stmt, point, &mut facts, &mut loan_counter);
        }

        // Extract facts from the terminator
        let term_point = Point::new(block_id, stmt_count);
        extract_terminator_facts(
            &block.terminator,
            term_point,
            block_id,
            &func.blocks,
            &mut facts,
        );
    }

    // Resolve move chains for ref_locals. When a loan's ref_local is
    // immediately moved to another local (e.g. `_3 = Ref(...); _4 = Move(_3)`),
    // update the ref_local to the final destination. This ensures the region
    // analysis tracks the actual local holding the reference, not just the temp.
    resolve_ref_local_moves(func, &mut facts);

    facts
}

/// Resolves move chains for loan `ref_locals`.
///
/// When the MIR lowering creates `_tmp = Ref(...)` followed by
/// `_user = Move(_tmp)`, the loan's `ref_local` should be the user variable
/// `_user`, not the temp `_tmp`. This matters for region analysis (liveness
/// of the ref local) and mutation checking.
fn resolve_ref_local_moves(func: &MirFunction, facts: &mut BorrowFacts) {
    // Build a map: source -> destination for move-only assignments.
    // Only consider simple moves: `_dest = Move(_src)` with no projections.
    let mut move_chain: rustc_hash::FxHashMap<Local, Local> = rustc_hash::FxHashMap::default();

    for block in &func.blocks {
        for stmt in &block.statements {
            if let Statement::Assign { place, rvalue, .. } = stmt {
                if place.projections.is_empty() {
                    if let Rvalue::Use(Operand::Move(src)) = rvalue {
                        if src.projections.is_empty() {
                            move_chain.insert(src.local, place.local);
                        }
                    }
                }
            }
        }
    }

    // Follow chains for each loan's ref_local.
    for loan in &mut facts.loans {
        let mut current = loan.ref_local;
        // Follow at most a few hops to avoid infinite loops.
        for _ in 0..8 {
            if let Some(&next) = move_chain.get(&current) {
                current = next;
            } else {
                break;
            }
        }
        loan.ref_local = current;
    }
}

/// Extracts facts from a single MIR statement.
fn extract_statement_facts(
    stmt: &Statement,
    point: Point,
    facts: &mut BorrowFacts,
    loan_counter: &mut u32,
) {
    match stmt {
        Statement::Assign { place, rvalue, .. } => {
            // The destination local is assigned.
            facts.assignments.push((point, place.local));

            // Check if this is a borrow (Rvalue::Ref).
            if let Rvalue::Ref {
                place: borrowed_place,
                mutable,
            } = rvalue
            {
                let loan_id = LoanId(*loan_counter);
                *loan_counter += 1;

                // Determine the lifetime from the Ref type if available.
                // For now we use ANONYMOUS since lifetime inference happens later.
                facts.loans.push(Loan {
                    id: loan_id,
                    place: borrowed_place.clone(),
                    mutable: *mutable,
                    issued_at: point,
                    ref_local: place.local,
                    lifetime: LifetimeId::ANONYMOUS,
                });
            }

            // Record uses from the rvalue.
            collect_rvalue_uses(rvalue, point, facts);

            // Record uses from place projections.
            collect_place_uses(place, point, facts);
        }
        Statement::Drop { place, .. } => {
            facts.uses.push((point, place.local));
        }
        Statement::Nop => {}
    }
}

/// Extracts facts from a terminator.
#[allow(clippy::cast_possible_truncation)]
fn extract_terminator_facts(
    terminator: &Terminator,
    point: Point,
    _block_id: BlockId,
    blocks: &[BasicBlock],
    facts: &mut BorrowFacts,
) {
    match terminator {
        Terminator::Goto { target } => {
            let target_point = Point::new(*target, 0);
            facts.cfg_edges.push((point, target_point));
        }

        Terminator::Return => {
            // Return uses Local(0) (the return place).
            facts.uses.push((point, Local::RETURN_PLACE));
        }

        Terminator::SwitchInt {
            discriminant,
            targets,
            otherwise,
        } => {
            collect_operand_uses(discriminant, point, facts);

            for &(_, target) in targets {
                let target_point = Point::new(target, 0);
                facts.cfg_edges.push((point, target_point));
            }
            let otherwise_point = Point::new(*otherwise, 0);
            facts.cfg_edges.push((point, otherwise_point));
        }

        Terminator::Call {
            func,
            args,
            destination,
            target,
            ..
        } => {
            collect_operand_uses(func, point, facts);
            for arg in args {
                collect_operand_uses(arg, point, facts);
            }
            // The destination is assigned in the continuation block.
            // But conceptually the assignment happens at the call point.
            facts.assignments.push((point, destination.local));

            let target_point = Point::new(*target, 0);
            facts.cfg_edges.push((point, target_point));
        }

        Terminator::Unreachable => {
            // No successors, no uses.
        }
    }

    // Also add inter-block edges based on block structure (successors).
    let _ = blocks;
}

/// Collects all locals used in an rvalue.
fn collect_rvalue_uses(rvalue: &Rvalue, point: Point, facts: &mut BorrowFacts) {
    match rvalue {
        Rvalue::BinaryOp(_, lhs, rhs) => {
            collect_operand_uses(lhs, point, facts);
            collect_operand_uses(rhs, point, facts);
        }
        Rvalue::Ref { place, .. } => {
            // The borrowed place is "used" (its address is taken).
            facts.uses.push((point, place.local));
            collect_place_uses(place, point, facts);
        }
        Rvalue::Aggregate(_, operands) | Rvalue::StringInterp(operands) => {
            for op in operands {
                collect_operand_uses(op, point, facts);
            }
        }
        Rvalue::ListRange {
            start, end, step, ..
        }
        | Rvalue::RangeLen {
            start, end, step, ..
        } => {
            collect_operand_uses(start, point, facts);
            collect_operand_uses(end, point, facts);
            collect_operand_uses(step, point, facts);
        }
        Rvalue::MapGetOptional { map, key, .. } => {
            collect_place_uses(map, point, facts);
            collect_place_uses(key, point, facts);
        }
        Rvalue::Discriminant(place) => {
            facts.uses.push((point, place.local));
        }
        Rvalue::Use(operand) | Rvalue::UnaryOp(_, operand) | Rvalue::Cast { operand, .. } => {
            collect_operand_uses(operand, point, facts);
        }
    }
}

/// Collects locals used in an operand.
fn collect_operand_uses(operand: &Operand, point: Point, facts: &mut BorrowFacts) {
    match operand {
        Operand::Copy(place) => {
            facts.uses.push((point, place.local));
            collect_place_uses(place, point, facts);
        }
        Operand::Move(place) => {
            facts.moves.push((point, place.local));
            collect_place_uses(place, point, facts);
        }
        Operand::Constant(_) => {}
    }
}

/// Collects locals used in place projections (e.g., indexing or deref).
///
/// When a place has projections, the base local is read to compute the
/// projected address. For `Deref`, the pointer value is read; for `Index`,
/// the index local is read.
fn collect_place_uses(place: &Place, point: Point, facts: &mut BorrowFacts) {
    if !place.projections.is_empty() {
        // The base local is read when it has projections (e.g. `_1.*` reads _1).
        facts.uses.push((point, place.local));
    }
    for proj in &place.projections {
        if let Projection::Index(idx_local) = proj {
            facts.uses.push((point, *idx_local));
        }
    }
}

/// Returns the set of all locals used in the function (for liveness boundary).
#[must_use]
pub fn all_locals(func: &MirFunction) -> FxHashSet<Local> {
    func.locals.iter().map(|decl| decl.local).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_mir::{LocalDecl, MirFunction, Rvalue, Statement};
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
                        rvalue: Rvalue::Use(Operand::Constant(spark_mir::Constant::Int(42))),
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
        }
    }

    #[test]
    fn extract_basic_facts() {
        let func = make_simple_func();
        let facts = extract_facts(&func);

        // Two assign statements
        assert_eq!(facts.assignments.len(), 2);
        assert_eq!(facts.assignments[0].1, Local(1));
        assert_eq!(facts.assignments[1].1, Local(0));

        // Statement 1 uses Local(1) via Copy
        assert!(!facts.uses.is_empty());
        assert!(facts.uses.iter().any(|(_, l)| *l == Local(1)));

        // Return terminator uses Local(0)
        assert!(facts.uses.iter().any(|(_, l)| *l == Local(0)));
    }

    #[test]
    fn extract_cfg_edges() {
        let func = make_simple_func();
        let facts = extract_facts(&func);

        // Intra-block edges: (bb0, 0) -> (bb0, 1) and (bb0, 1) -> (bb0, 2)
        assert!(facts.cfg_edges.len() >= 2);
        let edge0 = (Point::new(BlockId(0), 0), Point::new(BlockId(0), 1));
        assert!(facts.cfg_edges.contains(&edge0));
    }

    #[test]
    fn no_loans_without_ref() {
        let func = make_simple_func();
        let facts = extract_facts(&func);
        assert!(facts.loans.is_empty());
    }

    #[test]
    fn extract_loan_from_ref() {
        // bb0: _2 = Ref(_1); return
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
                    name: Some(spark_intern::sym("r")),
                    is_user_variable: true,
                    mutable: false,
                    span: Span::DUMMY,
                },
            ],
            blocks: vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(2)),
                    rvalue: Rvalue::Ref {
                        place: Place::from_local(Local(1)),
                        mutable: false,
                    },
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };

        let facts = extract_facts(&func);
        assert_eq!(facts.loans.len(), 1);
        assert_eq!(facts.loans[0].ref_local, Local(2));
        assert!(!facts.loans[0].mutable);
    }

    #[test]
    fn all_locals_collects_all() {
        let func = make_simple_func();
        let locals = all_locals(&func);
        assert!(locals.contains(&Local(0)));
        assert!(locals.contains(&Local(1)));
        assert_eq!(locals.len(), 2);
    }
}
