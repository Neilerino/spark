//! MIR function inlining pass.
//!
//! Substitutes small, non-recursive function bodies at their call sites.
//! This eliminates call overhead and enables subsequent passes (constant
//! folding, copy propagation, DCE) to optimize across what were previously
//! function boundaries.
//!
//! ## Inlining criteria
//!
//! A function is eligible for inlining when:
//! - Its cost score (`block_count * 3 + statement_count`) is below the threshold
//! - It is not recursive (no self-calls)
//! - It is not async
//! - It has no captures (not a closure)
//!
//! ## Algorithm
//!
//! For each inlineable call site:
//! 1. Create fresh locals in the caller for all of the callee's locals
//! 2. Create fresh blocks in the caller for all of the callee's blocks
//! 3. Add parameter initialization statements at the inlined entry
//! 4. Replace `Return` terminators with assignment to destination + `Goto`
//! 5. Replace the `Call` terminator with `Goto` to the inlined entry block

use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::{
    AggregateKind, BasicBlock, BlockId, Constant, Local, LocalDecl, Mir, MirFunction, Operand,
    Place, Projection, Rvalue, Statement, Terminator,
};
use spark_resolve::DefId;
use spark_span::Span;

/// Maximum inlining cost score. Functions with a higher score are not inlined.
const INLINE_THRESHOLD: usize = 100;

/// Inlines eligible function calls within each function of the MIR.
///
/// Returns `true` if any inlining occurred.
pub fn inline_calls(mir: &mut Mir) -> bool {
    // Phase 1: Identify inline candidates (functions small enough to inline).
    let candidates = collect_candidates(mir);
    if candidates.is_empty() {
        return false;
    }

    // Phase 2: Clone candidate bodies (we need immutable access to callee
    // bodies while mutating callers).
    let callee_bodies: FxHashMap<DefId, MirFunction> = candidates
        .iter()
        .filter_map(|def_id| mir.functions.get(def_id).map(|f| (*def_id, f.clone())))
        .collect();

    // Phase 3: Apply inlining to each function.
    let mut changed = false;
    let func_ids: Vec<DefId> = mir.functions.keys().copied().collect();
    for caller_id in func_ids {
        let Some(caller) = mir.functions.get_mut(&caller_id) else {
            continue;
        };
        changed |= inline_in_function(caller, &callee_bodies);
    }

    changed
}

/// Collects `DefId`s of functions eligible for inlining.
///
/// Both regular functions and lambda/closure functions are considered,
/// as long as they are small enough and not recursive or async.
fn collect_candidates(mir: &Mir) -> FxHashSet<DefId> {
    let mut candidates = FxHashSet::default();

    for (def_id, func) in &mir.functions {
        if func.is_async {
            continue;
        }
        if is_recursive(func) {
            continue;
        }
        let score = inline_cost(func);
        if score <= INLINE_THRESHOLD {
            candidates.insert(*def_id);
        }
    }

    candidates
}

/// Computes the inlining cost of a function.
fn inline_cost(func: &MirFunction) -> usize {
    let stmt_count: usize = func.blocks.iter().map(|b| b.statements.len()).sum();
    func.blocks.len() * 3 + stmt_count
}

/// Returns `true` if the function directly calls itself.
fn is_recursive(func: &MirFunction) -> bool {
    for block in &func.blocks {
        if let Terminator::Call {
            func: Operand::Constant(Constant::FunctionRef(callee)),
            ..
        } = &block.terminator
        {
            if *callee == func.def_id {
                return true;
            }
        }
    }
    false
}

/// Inlines all eligible calls within a single function.
///
/// Returns `true` if any inlining occurred.
fn inline_in_function(
    caller: &mut MirFunction,
    callee_bodies: &FxHashMap<DefId, MirFunction>,
) -> bool {
    let mut changed = false;

    // We may need multiple iterations since inlining can create new
    // opportunities, but limit to prevent blowup.
    for _ in 0..3 {
        let inlined_this_round = inline_one_round(caller, callee_bodies);
        if !inlined_this_round {
            break;
        }
        changed = true;
    }

    changed
}

/// Performs one round of inlining across all blocks. Returns true if any
/// call was inlined.
#[allow(clippy::similar_names)]
fn inline_one_round(
    caller: &mut MirFunction,
    callee_bodies: &FxHashMap<DefId, MirFunction>,
) -> bool {
    // Scan blocks for Call terminators to inline. We collect first to
    // avoid borrowing issues.
    let mut sites = Vec::new();
    for (block_idx, block) in caller.blocks.iter().enumerate() {
        if let Terminator::Call {
            func,
            args,
            destination,
            target,
            span,
        } = &block.terminator
        {
            // Try to resolve as a FunctionRef first, then as a Closure.
            let resolved = caller.resolve_callee_def_id(func).map_or_else(
                || resolve_closure_local(func, caller),
                |id| Some((id, Vec::new())),
            );

            if let Some((def_id, captures)) = resolved {
                // Don't inline self-calls (would be infinite).
                if def_id != caller.def_id && callee_bodies.contains_key(&def_id) {
                    sites.push(InlineSite {
                        block_idx,
                        callee_def_id: def_id,
                        args: args.clone(),
                        captures,
                        destination: destination.clone(),
                        target: *target,
                        span: *span,
                    });
                }
            }
        }
    }

    if sites.is_empty() {
        return false;
    }

    // Inline each site (process in reverse order so block indices stay valid
    // for sites we haven't processed yet — new blocks are appended at the end).
    for site in sites.into_iter().rev() {
        let callee = &callee_bodies[&site.callee_def_id];
        apply_inline(caller, &site, callee);
    }

    true
}

/// Information about a call site to be inlined.
struct InlineSite {
    block_idx: usize,
    #[allow(dead_code)]
    callee_def_id: DefId,
    args: Vec<Operand>,
    /// Capture operands from a `Closure` aggregate (empty for non-closure calls).
    captures: Vec<Operand>,
    destination: Place,
    target: BlockId,
    span: Span,
}

/// Resolves a function operand that is Copy/Move of a local back to a
/// `Closure` aggregate, returning `(func_def_id, capture_operands)`.
fn resolve_closure_local(func_op: &Operand, caller: &MirFunction) -> Option<(DefId, Vec<Operand>)> {
    let local = match func_op {
        Operand::Copy(place) | Operand::Move(place) if place.projections.is_empty() => place.local,
        _ => return None,
    };

    for block in &caller.blocks {
        for stmt in &block.statements {
            if let Statement::Assign { place, rvalue, .. } = stmt {
                if place.local == local && place.projections.is_empty() {
                    if let Rvalue::Aggregate(AggregateKind::Closure { func_def_id }, operands) =
                        rvalue
                    {
                        return Some((*func_def_id, operands.clone()));
                    }
                }
            }
        }
    }

    None
}

/// Applies inlining of `callee` at the given call `site` in `caller`.
#[allow(clippy::similar_names)]
fn apply_inline(caller: &mut MirFunction, site: &InlineSite, callee: &MirFunction) {
    let local_offset = caller.locals.len() as u32;
    let block_offset = caller.blocks.len() as u32;

    // Step 1: Add fresh locals for all of the callee's locals.
    for callee_local in &callee.locals {
        caller.locals.push(LocalDecl {
            local: Local(local_offset + callee_local.local.0),
            ty: callee_local.ty,
            name: None,
            is_user_variable: false,
            mutable: callee_local.mutable,
            span: callee_local.span,
        });
    }

    // Continuation block for after the inlined body returns.
    let continuation = site.target;

    // Step 2: Create remapped blocks from the callee.
    let inlined_entry = BlockId(block_offset);
    let mut inlined_blocks: Vec<BasicBlock> = Vec::with_capacity(callee.blocks.len());

    for callee_block in &callee.blocks {
        let remapped_stmts: Vec<Statement> = callee_block
            .statements
            .iter()
            .map(|s| remap_statement(s, local_offset, block_offset))
            .collect();

        let remapped_term = match &callee_block.terminator {
            Terminator::Return => {
                // Transfer the callee's return value to the caller's destination,
                // then jump to the continuation block.
                // We handle this specially below.
                Terminator::Return // placeholder, replaced below
            }
            other => remap_terminator(other, local_offset, block_offset),
        };

        inlined_blocks.push(BasicBlock {
            statements: remapped_stmts,
            terminator: remapped_term,
            is_cleanup: callee_block.is_cleanup,
        });
    }

    // Step 3: Replace Return terminators in inlined blocks with
    // assignment + goto to continuation.
    let callee_return_local = Local(local_offset); // callee's Local(0) remapped
    for block in &mut inlined_blocks {
        if matches!(block.terminator, Terminator::Return) {
            if site.destination.projections.is_empty() {
                block.statements.push(Statement::Assign {
                    place: site.destination.clone(),
                    rvalue: Rvalue::Use(Operand::Move(Place::from_local(callee_return_local))),
                    span: site.span,
                });
            }
            block.terminator = Terminator::Goto {
                target: continuation,
            };
        }
    }

    // Step 4: Prepend parameter + capture initialization to the inlined entry block.
    // Callee params are Local(1)..=Local(param_count), remapped to
    // Local(local_offset + 1)..=Local(local_offset + param_count).
    let mut init_stmts = Vec::with_capacity(callee.param_count as usize + site.captures.len());

    // Initialize parameters from call arguments.
    for (i, arg) in site.args.iter().enumerate() {
        let param_local = Local(local_offset + 1 + i as u32);
        let remapped_arg = remap_operand(arg, 0, 0); // args are in caller's space, no remap
        init_stmts.push(Statement::Assign {
            place: Place::from_local(param_local),
            rvalue: Rvalue::Use(remapped_arg),
            span: site.span,
        });
    }

    // Initialize captures from the Closure aggregate operands.
    // Capture locals in the callee start at index (1 + param_count).
    let capture_start = 1 + callee.param_count;
    for (i, cap_op) in site.captures.iter().enumerate() {
        let capture_local = Local(local_offset + capture_start + i as u32);
        let remapped_cap = remap_operand(cap_op, 0, 0); // captures are in caller's space
        init_stmts.push(Statement::Assign {
            place: Place::from_local(capture_local),
            rvalue: Rvalue::Use(remapped_cap),
            span: site.span,
        });
    }

    if let Some(entry) = inlined_blocks.first_mut() {
        let mut new_stmts = init_stmts;
        new_stmts.append(&mut entry.statements);
        entry.statements = new_stmts;
    }

    // Step 5: Append inlined blocks to the caller.
    caller.blocks.extend(inlined_blocks);

    // Step 6: Replace the original Call terminator with Goto to the
    // inlined entry block.
    caller.blocks[site.block_idx].terminator = Terminator::Goto {
        target: inlined_entry,
    };
}

// ── Remapping helpers ─────────────────────────────────────────────────

/// Remaps locals and block IDs in a statement.
fn remap_statement(stmt: &Statement, local_off: u32, block_off: u32) -> Statement {
    match stmt {
        Statement::Assign {
            place,
            rvalue,
            span,
        } => Statement::Assign {
            place: remap_place(place, local_off),
            rvalue: remap_rvalue(rvalue, local_off, block_off),
            span: *span,
        },
        Statement::Drop { place, span } => Statement::Drop {
            place: remap_place(place, local_off),
            span: *span,
        },
        Statement::Nop => Statement::Nop,
    }
}

/// Remaps locals and block IDs in a terminator.
fn remap_terminator(term: &Terminator, local_off: u32, block_off: u32) -> Terminator {
    match term {
        Terminator::Goto { target } => Terminator::Goto {
            target: remap_block(*target, block_off),
        },
        Terminator::Return => Terminator::Return,
        Terminator::SwitchInt {
            discriminant,
            targets,
            otherwise,
        } => Terminator::SwitchInt {
            discriminant: remap_operand(discriminant, local_off, block_off),
            targets: targets
                .iter()
                .map(|(val, bid)| (*val, remap_block(*bid, block_off)))
                .collect(),
            otherwise: remap_block(*otherwise, block_off),
        },
        Terminator::Call {
            func,
            args,
            destination,
            target,
            span,
        } => Terminator::Call {
            func: remap_operand(func, local_off, block_off),
            args: args
                .iter()
                .map(|a| remap_operand(a, local_off, block_off))
                .collect(),
            destination: remap_place(destination, local_off),
            target: remap_block(*target, block_off),
            span: *span,
        },
        Terminator::Unreachable => Terminator::Unreachable,
    }
}

/// Remaps a place's local and projection locals.
fn remap_place(place: &Place, local_off: u32) -> Place {
    Place {
        local: remap_local(place.local, local_off),
        projections: place
            .projections
            .iter()
            .map(|p| remap_projection(p, local_off))
            .collect(),
    }
}

/// Remaps a projection's inner locals.
const fn remap_projection(proj: &Projection, local_off: u32) -> Projection {
    match proj {
        Projection::Field(sym) => Projection::Field(*sym),
        Projection::Index(local) => Projection::Index(remap_local(*local, local_off)),
        Projection::Deref => Projection::Deref,
        Projection::UnionPayload(ty) => Projection::UnionPayload(*ty),
        Projection::EnumField { variant, index } => Projection::EnumField {
            variant: *variant,
            index: *index,
        },
    }
}

/// Remaps an operand's locals and block IDs.
fn remap_operand(op: &Operand, local_off: u32, _block_off: u32) -> Operand {
    match op {
        Operand::Copy(place) => Operand::Copy(remap_place(place, local_off)),
        Operand::Move(place) => Operand::Move(remap_place(place, local_off)),
        Operand::Constant(c) => Operand::Constant(c.clone()),
    }
}

/// Remaps an rvalue's locals and block IDs.
fn remap_rvalue(rvalue: &Rvalue, local_off: u32, block_off: u32) -> Rvalue {
    match rvalue {
        Rvalue::Use(op) => Rvalue::Use(remap_operand(op, local_off, block_off)),
        Rvalue::BinaryOp(op, lhs, rhs) => Rvalue::BinaryOp(
            *op,
            remap_operand(lhs, local_off, block_off),
            remap_operand(rhs, local_off, block_off),
        ),
        Rvalue::UnaryOp(op, operand) => {
            Rvalue::UnaryOp(*op, remap_operand(operand, local_off, block_off))
        }
        Rvalue::Ref { place, mutable } => Rvalue::Ref {
            place: remap_place(place, local_off),
            mutable: *mutable,
        },
        Rvalue::Aggregate(kind, ops) => {
            let remapped_ops: Vec<_> = ops
                .iter()
                .map(|o| remap_operand(o, local_off, block_off))
                .collect();
            Rvalue::Aggregate(remap_aggregate_kind(kind), remapped_ops)
        }
        Rvalue::Discriminant(place) => Rvalue::Discriminant(remap_place(place, local_off)),
        Rvalue::Cast { operand, target_ty } => Rvalue::Cast {
            operand: remap_operand(operand, local_off, block_off),
            target_ty: *target_ty,
        },
        Rvalue::StringInterp(ops) => Rvalue::StringInterp(
            ops.iter()
                .map(|o| remap_operand(o, local_off, block_off))
                .collect(),
        ),
        Rvalue::ListRange {
            start,
            end,
            inclusive,
            step,
        } => Rvalue::ListRange {
            start: remap_operand(start, local_off, block_off),
            end: remap_operand(end, local_off, block_off),
            inclusive: *inclusive,
            step: remap_operand(step, local_off, block_off),
        },
        Rvalue::RangeLen {
            start,
            end,
            inclusive,
            step,
        } => Rvalue::RangeLen {
            start: remap_operand(start, local_off, block_off),
            end: remap_operand(end, local_off, block_off),
            inclusive: *inclusive,
            step: remap_operand(step, local_off, block_off),
        },
        Rvalue::CollectionGetOptional { collection, index } => Rvalue::CollectionGetOptional {
            collection: remap_place(collection, local_off),
            index: remap_place(index, local_off),
        },
    }
}

/// Clones an aggregate kind (`DefId`s and symbols are interned, so clone is cheap).
fn remap_aggregate_kind(kind: &AggregateKind) -> AggregateKind {
    kind.clone()
}

/// Remaps a local by adding an offset.
const fn remap_local(local: Local, offset: u32) -> Local {
    Local(local.0 + offset)
}

/// Remaps a block ID by adding an offset.
const fn remap_block(block: BlockId, offset: u32) -> BlockId {
    BlockId(block.0 + offset)
}

#[cfg(test)]
#[allow(clippy::similar_names)]
mod tests {
    use super::*;
    use spark_ast::BinaryOp;
    use spark_types::TypeId;
    use std::collections::HashMap;

    fn local_decl(idx: u32) -> LocalDecl {
        LocalDecl {
            local: Local(idx),
            ty: TypeId::ERROR,
            name: None,
            is_user_variable: false,
            mutable: false,
            span: Span::DUMMY,
        }
    }

    fn make_func(
        def_id: DefId,
        param_count: u32,
        locals: Vec<LocalDecl>,
        blocks: Vec<BasicBlock>,
    ) -> MirFunction {
        MirFunction {
            def_id,
            param_count,
            return_type: TypeId::ERROR,
            locals,
            blocks,
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        }
    }

    #[test]
    fn inline_simple_identity() {
        // callee: fn id(x: Int): Int { return x }
        // caller: fn main(): Int { return id(42) }
        let callee_id = DefId::new(0);
        let caller_id = DefId::new(1);

        let callee = make_func(
            callee_id,
            1,
            vec![local_decl(0), local_decl(1)],
            vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(1)))),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
        );

        let caller = make_func(
            caller_id,
            0,
            vec![local_decl(0), local_decl(1)],
            vec![
                BasicBlock {
                    statements: vec![],
                    terminator: Terminator::Call {
                        func: Operand::Constant(Constant::FunctionRef(callee_id)),
                        args: vec![Operand::Constant(Constant::Int(42))],
                        destination: Place::from_local(Local(0)),
                        target: BlockId(1),
                        span: Span::DUMMY,
                    },
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
        );

        let mut mir = Mir {
            functions: HashMap::from([(callee_id, callee), (caller_id, caller)]),
        };

        assert!(inline_calls(&mut mir));

        let caller = mir.functions.get(&caller_id).unwrap();
        // The Call terminator should have been replaced with Goto.
        assert!(
            matches!(caller.blocks[0].terminator, Terminator::Goto { .. }),
            "Call should be replaced with Goto, got {:?}",
            caller.blocks[0].terminator,
        );
        // New locals should have been added for the callee's locals.
        assert!(caller.locals.len() > 2);
    }

    #[test]
    fn recursive_function_not_inlined() {
        let func_id = DefId::new(0);
        let func = make_func(
            func_id,
            1,
            vec![local_decl(0), local_decl(1)],
            vec![
                BasicBlock {
                    statements: vec![],
                    terminator: Terminator::Call {
                        func: Operand::Constant(Constant::FunctionRef(func_id)),
                        args: vec![Operand::Copy(Place::from_local(Local(1)))],
                        destination: Place::from_local(Local(0)),
                        target: BlockId(1),
                        span: Span::DUMMY,
                    },
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
        );

        let mut mir = Mir {
            functions: HashMap::from([(func_id, func)]),
        };

        assert!(!inline_calls(&mut mir));
    }

    #[test]
    fn large_function_not_inlined() {
        let callee_id = DefId::new(0);
        let caller_id = DefId::new(1);

        // Create a callee with many blocks to exceed the threshold.
        let mut blocks = Vec::new();
        for i in 0_u32..30 {
            blocks.push(BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::Use(Operand::Constant(Constant::Int(i64::from(i)))),
                    span: Span::DUMMY,
                }],
                terminator: if i < 29 {
                    Terminator::Goto {
                        target: BlockId(i + 1),
                    }
                } else {
                    Terminator::Return
                },
                is_cleanup: false,
            });
        }

        let callee = make_func(callee_id, 0, vec![local_decl(0)], blocks);

        let caller = make_func(
            caller_id,
            0,
            vec![local_decl(0)],
            vec![
                BasicBlock {
                    statements: vec![],
                    terminator: Terminator::Call {
                        func: Operand::Constant(Constant::FunctionRef(callee_id)),
                        args: vec![],
                        destination: Place::from_local(Local(0)),
                        target: BlockId(1),
                        span: Span::DUMMY,
                    },
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
        );

        let mut mir = Mir {
            functions: HashMap::from([(callee_id, callee), (caller_id, caller)]),
        };

        assert!(!inline_calls(&mut mir));
    }

    #[test]
    fn inline_function_with_binary_op() {
        // callee: fn add(a, b): Int { return a + b }
        // caller: fn main(): Int { return add(1, 2) }
        let callee_id = DefId::new(0);
        let caller_id = DefId::new(1);

        let callee = make_func(
            callee_id,
            2,
            vec![local_decl(0), local_decl(1), local_decl(2)],
            vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::BinaryOp(
                        BinaryOp::Add,
                        Operand::Copy(Place::from_local(Local(1))),
                        Operand::Copy(Place::from_local(Local(2))),
                    ),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
        );

        let caller = make_func(
            caller_id,
            0,
            vec![local_decl(0)],
            vec![
                BasicBlock {
                    statements: vec![],
                    terminator: Terminator::Call {
                        func: Operand::Constant(Constant::FunctionRef(callee_id)),
                        args: vec![
                            Operand::Constant(Constant::Int(1)),
                            Operand::Constant(Constant::Int(2)),
                        ],
                        destination: Place::from_local(Local(0)),
                        target: BlockId(1),
                        span: Span::DUMMY,
                    },
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
        );

        let mut mir = Mir {
            functions: HashMap::from([(callee_id, callee), (caller_id, caller)]),
        };

        assert!(inline_calls(&mut mir));

        let caller = mir.functions.get(&caller_id).unwrap();
        // Should have inlined: original 2 blocks + 1 inlined block = 3 blocks.
        assert_eq!(caller.blocks.len(), 3);
        // Original call block should now be Goto.
        assert!(matches!(
            caller.blocks[0].terminator,
            Terminator::Goto { .. }
        ));
    }

    #[test]
    fn async_function_not_inlined() {
        let callee_id = DefId::new(0);
        let caller_id = DefId::new(1);

        let mut callee = make_func(
            callee_id,
            0,
            vec![local_decl(0)],
            vec![BasicBlock {
                statements: vec![],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
        );
        callee.is_async = true;

        let caller = make_func(
            caller_id,
            0,
            vec![local_decl(0)],
            vec![
                BasicBlock {
                    statements: vec![],
                    terminator: Terminator::Call {
                        func: Operand::Constant(Constant::FunctionRef(callee_id)),
                        args: vec![],
                        destination: Place::from_local(Local(0)),
                        target: BlockId(1),
                        span: Span::DUMMY,
                    },
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
        );

        let mut mir = Mir {
            functions: HashMap::from([(callee_id, callee), (caller_id, caller)]),
        };

        assert!(!inline_calls(&mut mir));
    }

    #[test]
    fn no_call_sites_returns_false() {
        let func_id = DefId::new(0);
        let func = make_func(
            func_id,
            0,
            vec![local_decl(0)],
            vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
        );

        let mut mir = Mir {
            functions: HashMap::from([(func_id, func)]),
        };

        // No calls to inline, should return false.
        assert!(!inline_calls(&mut mir));
    }

    #[test]
    fn inline_closure_body() {
        // lambda: fn __lambda(x: Int, captured_y: Int): Int { return x + captured_y }
        // caller: __fn = Closure { func_def_id: lambda_id, captures: [Copy(_1)] }
        //         Call { func: Copy(__fn), args: [const 10], destination: _0, target: bb1 }
        let lambda_id = DefId::new(0);
        let caller_id = DefId::new(1);

        let lambda = MirFunction {
            def_id: lambda_id,
            param_count: 1,
            return_type: TypeId::ERROR,
            locals: vec![
                local_decl(0), // return
                local_decl(1), // param x
                local_decl(2), // capture y
            ],
            blocks: vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::BinaryOp(
                        BinaryOp::Add,
                        Operand::Copy(Place::from_local(Local(1))),
                        Operand::Copy(Place::from_local(Local(2))),
                    ),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![(DefId::new(99), TypeId::ERROR)],
            is_async: false,
            span: Span::DUMMY,
        };

        let caller = MirFunction {
            def_id: caller_id,
            param_count: 1,
            return_type: TypeId::ERROR,
            locals: vec![
                local_decl(0), // return
                local_decl(1), // param y (captured)
                local_decl(2), // __fn (closure fat pointer)
            ],
            blocks: vec![
                BasicBlock {
                    statements: vec![
                        // __fn = Closure { lambda_id, [Copy(_1)] }
                        Statement::Assign {
                            place: Place::from_local(Local(2)),
                            rvalue: Rvalue::Aggregate(
                                AggregateKind::Closure {
                                    func_def_id: lambda_id,
                                },
                                vec![Operand::Copy(Place::from_local(Local(1)))],
                            ),
                            span: Span::DUMMY,
                        },
                    ],
                    terminator: Terminator::Call {
                        func: Operand::Copy(Place::from_local(Local(2))),
                        args: vec![Operand::Constant(Constant::Int(10))],
                        destination: Place::from_local(Local(0)),
                        target: BlockId(1),
                        span: Span::DUMMY,
                    },
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };

        let mut mir = Mir {
            functions: HashMap::from([(lambda_id, lambda), (caller_id, caller)]),
        };

        assert!(inline_calls(&mut mir));

        let caller = mir.functions.get(&caller_id).unwrap();
        // Call should be replaced with Goto.
        assert!(
            matches!(caller.blocks[0].terminator, Terminator::Goto { .. }),
            "Call should be replaced with Goto, got {:?}",
            caller.blocks[0].terminator,
        );
        // New locals should be added for the lambda's locals.
        assert!(
            caller.locals.len() > 3,
            "Expected extra locals from inlined lambda, got {}",
            caller.locals.len(),
        );
    }
}
