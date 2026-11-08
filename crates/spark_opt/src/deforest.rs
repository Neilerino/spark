//! Deforestation pass: eliminates intermediate data structures that are
//! created only to be immediately consumed by iteration.
//!
//! Currently handles:
//! - `ListRange` → sequential loop (`list_len` + indexed access) → arithmetic
//! - `ListRange` → parallel operation → range-aware intrinsic
//!
//! The pass works in three phases:
//! 1. **Find allocation sites** — scan for `Rvalue::ListRange` assignments
//! 2. **Analyze consumers** — classify all uses (len, index, parallel op, escapes)
//! 3. **Rewrite** — if all consumers are "structural", eliminate the allocation

use spark_ast::BinaryOp;
use spark_intern::kw;
use spark_mir::{
    BlockId, Constant, Intrinsic, Local, LocalDecl, MirFunction, Operand, Place, Projection,
    Rvalue, Statement, Terminator,
};
use spark_resolve::{DefinitionKind, DefinitionTable};
use spark_span::Span;
use spark_types::TypeId;

/// An allocation site: a `ListRange` assignment we may be able to eliminate.
#[derive(Debug)]
struct AllocationSite {
    /// The local assigned the list range value.
    local: Local,
    /// Block index of the assignment.
    block_idx: usize,
    /// Statement index within the block.
    stmt_idx: usize,
    /// Range start operand.
    start: Operand,
    /// Range end operand.
    end: Operand,
    /// Whether the range is inclusive.
    inclusive: bool,
    /// Range step operand.
    step: Operand,
}

/// A consumer of a list-range allocation.
#[derive(Debug)]
enum Consumer {
    /// `list_len(local)` call → can be replaced with `RangeLen`.
    LenCall {
        block_idx: usize,
        dest: Local,
        target: BlockId,
    },
    /// `local[idx]` index access → can be replaced with `start + idx * step`.
    IndexAccess {
        block_idx: usize,
        stmt_idx: usize,
        idx_local: Local,
    },
    /// Parallel operation call (`parallel_foreach`, `parallel_map`, `parallel_map_reduce`).
    ParallelOp {
        block_idx: usize,
        kind: ParallelKind,
    },
    /// Any other use — cannot deforest.
    Escapes,
}

#[derive(Debug, Clone, Copy)]
enum ParallelKind {
    ForEach,
    Map,
    MapReduce,
}

/// Runs the deforestation pass on a single function.
///
/// Returns `true` if any rewrites were performed.
pub fn deforest(func: &mut MirFunction, defs: &DefinitionTable) -> bool {
    // Look up DefIds for builtins we need to recognize.
    let len_def = find_builtin_method(defs, kw::LEN);
    let foreach_def = find_builtin_fn(defs, kw::PARALLEL_FOREACH);
    let map_def = find_builtin_fn(defs, kw::PARALLEL_MAP);
    let map_reduce_def = find_builtin_fn(defs, kw::PARALLEL_MAP_REDUCE);

    // Phase 1: Find allocation sites.
    let sites = find_allocation_sites(func);
    if sites.is_empty() {
        return false;
    }

    let mut changed = false;

    for site in &sites {
        // Collect aliases (transparent move/copy chains).
        let aliases = collect_aliases(func, site.local);

        // Phase 2: Analyze consumers.
        let consumers = analyze_consumers(
            func,
            site,
            &aliases,
            len_def,
            foreach_def,
            map_def,
            map_reduce_def,
        );

        // If any consumer escapes, skip this site.
        if consumers.iter().any(|c| matches!(c, Consumer::Escapes)) {
            continue;
        }

        // Phase 3: Rewrite.
        changed |= rewrite_consumers(func, site, &consumers);
    }

    changed
}

/// Finds all `ListRange` assignments in the function.
fn find_allocation_sites(func: &MirFunction) -> Vec<AllocationSite> {
    let mut sites = Vec::new();
    for (block_idx, block) in func.blocks.iter().enumerate() {
        for (stmt_idx, stmt) in block.statements.iter().enumerate() {
            if let Statement::Assign {
                place,
                rvalue:
                    Rvalue::ListRange {
                        start,
                        end,
                        inclusive,
                        step,
                    },
                ..
            } = stmt
            {
                if place.projections.is_empty() {
                    sites.push(AllocationSite {
                        local: place.local,
                        block_idx,
                        stmt_idx,
                        start: start.clone(),
                        end: end.clone(),
                        inclusive: *inclusive,
                        step: step.clone(),
                    });
                }
            }
        }
    }
    sites
}

/// Collects all locals that are transparent move/copy aliases of the given local.
///
/// For example, if we have `_3 = ListRange(...)` and `_4 = Move(_3)`,
/// then `{_3, _4}` are aliases. Follows chains transitively.
fn collect_aliases(func: &MirFunction, base_local: Local) -> Vec<Local> {
    let mut aliases = vec![base_local];
    let mut changed = true;
    while changed {
        changed = false;
        for block in &func.blocks {
            for stmt in &block.statements {
                if let Statement::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(src) | Operand::Move(src)),
                    ..
                } = stmt
                {
                    if src.projections.is_empty()
                        && place.projections.is_empty()
                        && aliases.contains(&src.local)
                        && !aliases.contains(&place.local)
                    {
                        aliases.push(place.local);
                        changed = true;
                    }
                }
            }
        }
    }
    aliases
}

/// Returns true if a local is in the alias set.
fn is_alias(aliases: &[Local], local: Local) -> bool {
    aliases.contains(&local)
}

/// Returns true if an operand refers to an alias local.
fn operand_is_alias(op: &Operand, aliases: &[Local]) -> bool {
    match op {
        Operand::Copy(place) | Operand::Move(place) => {
            place.projections.is_empty() && is_alias(aliases, place.local)
        }
        Operand::Constant(_) => false,
    }
}

/// Returns true if an operand reads from any alias local.
fn operand_reads_alias(op: &Operand, aliases: &[Local]) -> bool {
    match op {
        Operand::Copy(place) | Operand::Move(place) => {
            is_alias(aliases, place.local)
                || place
                    .projections
                    .iter()
                    .any(|p| matches!(p, Projection::Index(l) if is_alias(aliases, *l)))
        }
        Operand::Constant(_) => false,
    }
}

/// Analyzes all consumers of a given allocation site (and its aliases).
#[allow(clippy::too_many_lines)]
fn analyze_consumers(
    func: &MirFunction,
    site: &AllocationSite,
    aliases: &[Local],
    len_def: Option<spark_resolve::DefId>,
    foreach_def: Option<spark_resolve::DefId>,
    map_def: Option<spark_resolve::DefId>,
    map_reduce_def: Option<spark_resolve::DefId>,
) -> Vec<Consumer> {
    let mut consumers = Vec::new();

    for (block_idx, block) in func.blocks.iter().enumerate() {
        // Check statements for uses of any alias.
        for (stmt_idx, stmt) in block.statements.iter().enumerate() {
            // Skip the allocation site itself.
            if block_idx == site.block_idx && stmt_idx == site.stmt_idx {
                continue;
            }

            match stmt {
                Statement::Assign { place, rvalue, .. } => {
                    // Check if rvalue reads an alias via move/copy.
                    if let Rvalue::Use(Operand::Copy(src_place) | Operand::Move(src_place)) = rvalue
                    {
                        if is_alias(aliases, src_place.local) {
                            // Check for index projection: _list[_idx]
                            if src_place.projections.len() == 1 {
                                if let Projection::Index(idx_local) = &src_place.projections[0] {
                                    consumers.push(Consumer::IndexAccess {
                                        block_idx,
                                        stmt_idx,
                                        idx_local: *idx_local,
                                    });
                                    continue;
                                }
                            }
                            // Transparent move/copy to another local with no projections
                            // — this creates an alias, not an escape.
                            if src_place.projections.is_empty() && place.projections.is_empty() {
                                // Already tracked as an alias; skip.
                                continue;
                            }
                            // Non-index, non-transparent use: escape.
                            consumers.push(Consumer::Escapes);
                            continue;
                        }
                    }

                    // Check if the rvalue otherwise reads an alias.
                    if rvalue_reads_any_alias(rvalue, aliases) {
                        consumers.push(Consumer::Escapes);
                        continue;
                    }

                    // Check if place projections read an alias.
                    if projection_reads_alias(place, aliases) {
                        consumers.push(Consumer::Escapes);
                    }
                }
                Statement::Drop { place, .. } => {
                    if is_alias(aliases, place.local) {
                        // Drop of the list (or alias) is fine — becomes dead code.
                    }
                }
                Statement::Nop => {}
            }
        }

        // Check the terminator for uses of any alias.
        match &block.terminator {
            Terminator::Call {
                func: func_op,
                args,
                destination,
                target,
                ..
            } => {
                // Check if an alias is passed as an argument to a known function.
                let callee_def = func.resolve_callee_def_id(func_op);

                if let Some(callee) = callee_def {
                    // Check if this is a len call on our list.
                    if Some(callee) == len_def
                        && args.len() == 1
                        && operand_is_alias(&args[0], aliases)
                    {
                        consumers.push(Consumer::LenCall {
                            block_idx,
                            dest: destination.local,
                            target: *target,
                        });
                        continue;
                    }

                    // Check for parallel operations: list is always args[0].
                    if args.first().is_some_and(|a| operand_is_alias(a, aliases)) {
                        if Some(callee) == foreach_def {
                            consumers.push(Consumer::ParallelOp {
                                block_idx,
                                kind: ParallelKind::ForEach,
                            });
                            continue;
                        }
                        if Some(callee) == map_def {
                            consumers.push(Consumer::ParallelOp {
                                block_idx,
                                kind: ParallelKind::Map,
                            });
                            continue;
                        }
                        if Some(callee) == map_reduce_def {
                            consumers.push(Consumer::ParallelOp {
                                block_idx,
                                kind: ParallelKind::MapReduce,
                            });
                            continue;
                        }
                    }
                }

                // If any alias appears in this call in any other way, it escapes.
                if operand_reads_alias(func_op, aliases)
                    || args.iter().any(|a| operand_reads_alias(a, aliases))
                {
                    consumers.push(Consumer::Escapes);
                }
            }
            Terminator::SwitchInt { discriminant, .. } => {
                if operand_reads_alias(discriminant, aliases) {
                    consumers.push(Consumer::Escapes);
                }
            }
            Terminator::Goto { .. } | Terminator::Return | Terminator::Unreachable => {}
        }
    }

    consumers
}

/// Returns true if an rvalue reads from any alias local.
fn rvalue_reads_any_alias(rvalue: &Rvalue, aliases: &[Local]) -> bool {
    match rvalue {
        Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Cast { operand: op, .. } => {
            operand_reads_alias(op, aliases)
        }
        Rvalue::BinaryOp(_, a, b) => {
            operand_reads_alias(a, aliases) || operand_reads_alias(b, aliases)
        }
        Rvalue::Ref { place, .. } | Rvalue::Discriminant(place) => {
            place_reads_alias(place, aliases)
        }
        Rvalue::Aggregate(_, ops) | Rvalue::StringInterp(ops) => {
            ops.iter().any(|op| operand_reads_alias(op, aliases))
        }
        Rvalue::ListRange {
            start, end, step, ..
        }
        | Rvalue::RangeLen {
            start, end, step, ..
        } => {
            operand_reads_alias(start, aliases)
                || operand_reads_alias(end, aliases)
                || operand_reads_alias(step, aliases)
        }
        Rvalue::CollectionGetOptional { collection, index } => {
            place_reads_alias(collection, aliases) || place_reads_alias(index, aliases)
        }
    }
}

fn place_reads_alias(place: &Place, aliases: &[Local]) -> bool {
    is_alias(aliases, place.local) || projection_reads_alias(place, aliases)
}

fn projection_reads_alias(place: &Place, aliases: &[Local]) -> bool {
    place
        .projections
        .iter()
        .any(|p| matches!(p, Projection::Index(l) if is_alias(aliases, *l)))
}

/// Rewrites consumers to eliminate the allocation.
#[allow(clippy::too_many_lines)]
fn rewrite_consumers(
    func: &mut MirFunction,
    site: &AllocationSite,
    consumers: &[Consumer],
) -> bool {
    if consumers.is_empty() {
        // No consumers at all — just nop the allocation, DCE handles the rest.
        func.blocks[site.block_idx].statements[site.stmt_idx] = Statement::Nop;
        return true;
    }

    // Track whether we need a fresh local for the step multiplication result.
    let mut changed = false;

    for consumer in consumers {
        match consumer {
            Consumer::LenCall {
                block_idx,
                dest,
                target,
            } => {
                // Replace the call terminator with:
                //   _dest = RangeLen { start, end, inclusive, step }
                //   goto target
                let range_len_stmt = Statement::Assign {
                    place: Place::from_local(*dest),
                    rvalue: Rvalue::RangeLen {
                        start: site.start.clone(),
                        end: site.end.clone(),
                        inclusive: site.inclusive,
                        step: site.step.clone(),
                    },
                    span: Span::DUMMY,
                };

                let block = &mut func.blocks[*block_idx];
                block.statements.push(range_len_stmt);
                block.terminator = Terminator::Goto { target: *target };
                changed = true;
            }

            Consumer::IndexAccess {
                block_idx,
                stmt_idx,
                idx_local,
            } => {
                // Replace `_dest = Copy(_list[_idx])` with:
                //   _mul_tmp = BinaryOp(Mul, _idx, step)
                //   _dest = BinaryOp(Add, start, _mul_tmp)
                let dest_local = match &func.blocks[*block_idx].statements[*stmt_idx] {
                    Statement::Assign { place, .. } => place.local,
                    _ => continue,
                };

                // Use the same type as the destination (element type, typically Int).
                let dest_ty = func
                    .locals
                    .iter()
                    .find(|l| l.local == dest_local)
                    .map_or(TypeId::ERROR, |l| l.ty);

                // Allocate a temporary for the mul result.
                let mul_tmp = Local(func.locals.len() as u32);
                func.locals.push(LocalDecl {
                    local: mul_tmp,
                    ty: dest_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                });

                let mul_stmt = Statement::Assign {
                    place: Place::from_local(mul_tmp),
                    rvalue: Rvalue::BinaryOp(
                        BinaryOp::Mul,
                        Operand::Copy(Place::from_local(*idx_local)),
                        site.step.clone(),
                    ),
                    span: Span::DUMMY,
                };

                let add_stmt = Statement::Assign {
                    place: Place::from_local(dest_local),
                    rvalue: Rvalue::BinaryOp(
                        BinaryOp::Add,
                        site.start.clone(),
                        Operand::Copy(Place::from_local(mul_tmp)),
                    ),
                    span: Span::DUMMY,
                };

                // Replace the original statement with the mul, and insert add after it.
                func.blocks[*block_idx].statements[*stmt_idx] = mul_stmt;
                func.blocks[*block_idx]
                    .statements
                    .insert(*stmt_idx + 1, add_stmt);
                changed = true;
            }

            Consumer::ParallelOp { block_idx, kind } => {
                // Replace the call: swap FunctionRef with IntrinsicRef and
                // replace the list arg with (start, end, inclusive_const, step).
                let intrinsic = match kind {
                    ParallelKind::ForEach => Intrinsic::ParallelRangeForEach,
                    ParallelKind::Map => Intrinsic::ParallelRangeMap,
                    ParallelKind::MapReduce => Intrinsic::ParallelRangeMapReduce,
                };

                let block = &mut func.blocks[*block_idx];
                if let Terminator::Call {
                    func: ref mut func_op,
                    ref mut args,
                    ..
                } = block.terminator
                {
                    // Replace the function operand with IntrinsicRef.
                    *func_op = Operand::Constant(Constant::IntrinsicRef(intrinsic));

                    // Replace args[0] (the list) with range parameters.
                    // Original args for parallel_map_reduce: [list, mapper, combiner, initial]
                    // New args: [start, end, inclusive_i64, step, mapper, combiner, initial]
                    let rest_args: Vec<Operand> = args.drain(1..).collect();
                    args.clear();
                    args.push(site.start.clone());
                    args.push(site.end.clone());
                    args.push(Operand::Constant(Constant::Int(i64::from(u8::from(
                        site.inclusive,
                    )))));
                    args.push(site.step.clone());
                    args.extend(rest_args);

                    changed = true;
                }
            }

            Consumer::Escapes => {
                // Should not reach here — we filter these out above.
            }
        }
    }

    if changed {
        // Nop the original ListRange allocation — DCE will clean it up.
        func.blocks[site.block_idx].statements[site.stmt_idx] = Statement::Nop;
    }

    changed
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Finds a `BuiltinMethod` by name in the definition table.
fn find_builtin_method(
    defs: &DefinitionTable,
    name: spark_intern::Symbol,
) -> Option<spark_resolve::DefId> {
    defs.iter()
        .find(|(_, info)| matches!(info.kind, DefinitionKind::BuiltinMethod) && info.name == name)
        .map(|(id, _)| id)
}

/// Finds a `BuiltinFunction` by name in the definition table.
fn find_builtin_fn(
    defs: &DefinitionTable,
    name: spark_intern::Symbol,
) -> Option<spark_resolve::DefId> {
    defs.iter()
        .find(|(_, info)| matches!(info.kind, DefinitionKind::BuiltinFunction) && info.name == name)
        .map(|(id, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_mir::{BasicBlock, MirFunction, Statement, Terminator};
    use spark_resolve::DefId;

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

    #[test]
    fn no_list_range_returns_false() {
        let mut func = make_func(
            vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            vec![local_decl(0)],
        );

        let defs = DefinitionTable::new();
        assert!(!deforest(&mut func, &defs));
    }

    #[test]
    fn list_range_with_no_consumers_is_noped() {
        // _1 = ListRange(0..10, step=1)
        // return
        let mut func = make_func(
            vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(0))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::ListRange {
                            start: Operand::Constant(Constant::Int(0)),
                            end: Operand::Constant(Constant::Int(10)),
                            inclusive: false,
                            step: Operand::Constant(Constant::Int(1)),
                        },
                        span: Span::DUMMY,
                    },
                ],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            vec![local_decl(0), local_decl(1)],
        );

        let defs = DefinitionTable::new();
        assert!(deforest(&mut func, &defs));
        assert!(matches!(func.blocks[0].statements[1], Statement::Nop));
    }
}
