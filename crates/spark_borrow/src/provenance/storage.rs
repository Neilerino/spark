//! Owner invalidation and the actual accesses that require an owned payload.

use rustc_hash::FxHashSet;
use spark_diagnostics::{Diagnostic, Diagnostics, Label};
use spark_mir::Place;
use spark_types::{Ty, TypeId, TypeTable};

use super::{value, AccessKind, Accesses, Flow, Origin, Path, Point, Slot, State, Target};

impl Flow<'_> {
    pub(super) fn invalidate(&self, place: &Place, point: Point, state: &mut State) {
        if place
            .projections
            .iter()
            .any(|p| matches!(p, spark_mir::Projection::Deref))
        {
            return;
        }
        let (prefix, ty) = self.projection_path(
            self.function.locals[place.local.index()].ty,
            &place.projections,
        );
        for path in owned_paths(ty, self.table) {
            let mut path = [prefix.clone(), path].concat();
            path = value::normalize(
                self.function.locals[place.local.index()].ty,
                path,
                self.table,
            );
            state
                .invalidated
                .entry((place.local, path))
                .or_default()
                .insert(point);
        }
    }

    pub(super) fn record_accesses(&mut self, point: Point, accesses: Accesses, state: &State) {
        if !accesses.is_empty() {
            self.readers.insert(point.block);
        }
        for access in &accesses {
            let mut expired = FxHashSet::default();
            self.expired_access(
                &access.origin,
                &access.selection,
                access.kind,
                state,
                &mut FxHashSet::default(),
                &mut expired,
            );
            for drop in expired {
                self.invalid_reads
                    .insert((point, drop, access.origin.clone()));
            }
        }
        self.transfers
            .entry(point)
            .or_default()
            .seeds
            .extend(accesses.demands.iter().cloned());
        self.accesses.entry(point).or_default().merge(accesses);
    }

    fn expired_access(
        &self,
        origin: &Origin,
        selection: &[Slot],
        kind: AccessKind,
        state: &State,
        seen: &mut FxHashSet<Origin>,
        expired: &mut FxHashSet<Point>,
    ) {
        if !seen.insert(origin.clone()) {
            return;
        }
        let local_loan = match origin {
            Origin::Loan(point) => Some(*point),
            Origin::Frame { function, point } if *function == self.function.def_id => Some(*point),
            Origin::Input { .. }
            | Origin::InputAddress { .. }
            | Origin::Frame { .. }
            | Origin::Opaque { .. }
            | Origin::Unproven(_) => None,
        };
        if let Some(point) = local_loan {
            for target in self.loans.get(&point).into_iter().flatten() {
                match target {
                    Target::Frame { local, path } => {
                        let selected = [path.as_slice(), selection].concat();
                        for ((owner, path), drops) in &state.invalidated {
                            if owner != local {
                                continue;
                            }
                            let needed = match kind {
                                AccessKind::Value => overlaps(path, &selected),
                                AccessKind::Address => {
                                    path.len() < selected.len() && overlaps_prefix(path, &selected)
                                }
                            };
                            if needed {
                                expired.extend(drops);
                            }
                        }
                    }
                    Target::Through { origin, path, .. } => {
                        self.expired_access(
                            origin,
                            &[path.as_slice(), selection].concat(),
                            kind,
                            state,
                            seen,
                            expired,
                        );
                    }
                }
            }
        } else if let Origin::Unproven(point) = origin {
            for dependency in self.unproven.get(point).into_iter().flatten() {
                self.expired_access(dependency, selection, kind, state, seen, expired);
            }
        }
        seen.remove(origin);
    }

    pub(super) fn report_reads(&self, diagnostics: &mut Diagnostics) {
        let mut reported = FxHashSet::default();
        for (access, invalidation, origin) in &self.invalid_reads {
            if !reported.insert((*access, *invalidation)) {
                continue;
            }
            let mut diagnostic = Diagnostic::error("borrowed value does not live long enough")
                .with_label(Label::primary(
                    crate::errors::span_for_point(*access, self.function),
                    "this read needs the borrowed value",
                ))
                .with_label(Label::secondary(
                    crate::errors::span_for_point(*invalidation, self.function),
                    "its owner was dropped here",
                ));
            if let Origin::Loan(point) = origin {
                diagnostic = diagnostic.with_label(Label::secondary(
                    crate::errors::span_for_point(*point, self.function),
                    "borrow created here",
                ));
            }
            diagnostics.push(diagnostic);
        }
    }
}

pub(super) fn prefix(left: &[Slot], right: &[Slot]) -> bool {
    !left.contains(&Slot::Tail) && right.starts_with(left)
}

fn overlaps_prefix(left: &[Slot], right: &[Slot]) -> bool {
    for (index, slot) in left.iter().enumerate() {
        if *slot == Slot::Tail || right.get(index) == Some(&Slot::Tail) {
            return true;
        }
        if right.get(index) != Some(slot) {
            return false;
        }
    }
    true
}

fn overlaps(left: &[Slot], right: &[Slot]) -> bool {
    overlaps_prefix(left, right) || overlaps_prefix(right, left)
}

fn owned_paths(ty: TypeId, table: &TypeTable) -> Vec<Path> {
    fn visit(
        ty: TypeId,
        path: &mut Path,
        seen: &mut FxHashSet<TypeId>,
        table: &TypeTable,
        paths: &mut Vec<Path>,
    ) {
        let ty = value::strip_mutable(ty, table);
        if !seen.insert(ty) {
            let mut tail = path.clone();
            tail.push(Slot::Tail);
            paths.push(tail);
            return;
        }
        if spark_drop::needs_rc(ty, table)
            || matches!(table.resolve(ty), Ty::Function { .. } | Ty::Param(_))
        {
            paths.push(path.clone());
        } else {
            for (slot, child) in value::inline_slots(ty, table) {
                path.push(slot);
                visit(child, path, seen, table, paths);
                path.pop();
            }
        }
        seen.remove(&ty);
    }
    let mut paths = Vec::new();
    visit(
        ty,
        &mut Vec::new(),
        &mut FxHashSet::default(),
        table,
        &mut paths,
    );
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_mir::{
        AggregateKind, BasicBlock, Constant, Local, LocalDecl, Mir, MirFunction, Operand,
        Projection, Rvalue, Statement, Terminator,
    };
    use spark_resolve::DefId;
    use spark_span::Span;
    use spark_types::{IntWidth, LifetimeId};

    #[test]
    fn cleanup_preserves_raw_reference_fields_in_addressable_cells() {
        for local_source in [false, true] {
            let mut table = TypeTable::new();
            let int = table.intern(Ty::Int(IntWidth::I64));
            let string = table.intern(Ty::String);
            let reference = table.intern(Ty::Ref {
                inner: int,
                mutable: false,
                lifetime: LifetimeId::ANONYMOUS,
            });
            let tuple = table.intern(Ty::Tuple(vec![reference, string]));
            let tuple_ref = table.intern(Ty::Ref {
                inner: tuple,
                mutable: false,
                lifetime: LifetimeId::ANONYMOUS,
            });
            let map = table.intern(Ty::Map(string, reference));
            let span = Span::DUMMY;
            let place = |index| Place::from_local(Local(index));
            let copy = |index| Operand::Copy(place(index));
            let assign = |index, rvalue| Statement::Assign {
                place: place(index),
                rvalue,
                span,
            };
            let function = MirFunction {
                def_id: DefId::new(0),
                param_count: u32::from(!local_source),
                return_type: map,
                locals: [
                    map,
                    if local_source { int } else { reference },
                    reference,
                    tuple,
                    tuple_ref,
                    reference,
                ]
                .into_iter()
                .enumerate()
                .map(|(index, ty)| LocalDecl {
                    local: Local(index as u32),
                    ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span,
                })
                .collect(),
                blocks: vec![BasicBlock {
                    statements: vec![
                        if local_source {
                            assign(1, Rvalue::Use(Operand::Constant(Constant::Int(7))))
                        } else {
                            Statement::Nop
                        },
                        assign(
                            2,
                            if local_source {
                                Rvalue::Ref {
                                    place: place(1),
                                    mutable: false,
                                }
                            } else {
                                Rvalue::Use(copy(1))
                            },
                        ),
                        assign(
                            3,
                            Rvalue::Aggregate(
                                AggregateKind::Tuple,
                                vec![
                                    copy(2),
                                    Operand::Constant(Constant::String(spark_intern::sym("owner"))),
                                ],
                            ),
                        ),
                        assign(
                            4,
                            Rvalue::Ref {
                                place: place(3),
                                mutable: false,
                            },
                        ),
                        Statement::Drop {
                            place: place(3),
                            span,
                        },
                        assign(
                            5,
                            Rvalue::Use(Operand::Copy(Place {
                                local: Local(4),
                                projections: vec![
                                    Projection::Deref,
                                    Projection::Field(spark_intern::sym("0")),
                                ],
                            })),
                        ),
                        assign(
                            0,
                            Rvalue::Aggregate(
                                AggregateKind::Map,
                                vec![
                                    Operand::Constant(Constant::String(spark_intern::sym("one"))),
                                    copy(5),
                                ],
                            ),
                        ),
                    ],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                }],
                captures: vec![],
                is_async: false,
                span,
            };
            let mir = Mir {
                functions: std::collections::HashMap::from([(function.def_id, function)]),
            };
            let mut diagnostics = Diagnostics::new();
            super::super::analyze_program(&mir, &table, &mut diagnostics);
            assert_eq!(diagnostics.has_errors(), local_source);
            if local_source {
                assert!(diagnostics.iter().any(|diagnostic| diagnostic.message
                    == "returned value contains a reference to local storage"));
            }
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| diagnostic.message
                        != "borrowed value does not live long enough"),
                "reading a raw reference field must not read its dropped String sibling"
            );
        }
    }
    #[test]
    fn branch_conditions_observe_projected_reference_reads() {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let boolean = table.intern(Ty::Bool);
        let list = table.intern(Ty::List(boolean));
        let reference = table.intern(Ty::Ref {
            inner: boolean,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let span = Span::DUMMY;
        let place = |index| Place::from_local(Local(index));
        let assign = |index, rvalue| Statement::Assign {
            place: place(index),
            rvalue,
            span,
        };
        let function = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: int,
            locals: [int, list, int, reference]
                .into_iter()
                .enumerate()
                .map(|(index, ty)| LocalDecl {
                    local: Local(index as u32),
                    ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span,
                })
                .collect(),
            blocks: vec![
                BasicBlock {
                    statements: vec![
                        assign(
                            1,
                            Rvalue::Aggregate(
                                AggregateKind::List,
                                vec![Operand::Constant(Constant::Bool(true))],
                            ),
                        ),
                        assign(2, Rvalue::Use(Operand::Constant(Constant::Int(0)))),
                        assign(
                            3,
                            Rvalue::Ref {
                                place: Place {
                                    local: Local(1),
                                    projections: vec![Projection::Index(Local(2))],
                                },
                                mutable: false,
                            },
                        ),
                        Statement::Drop {
                            place: place(1),
                            span,
                        },
                    ],
                    terminator: Terminator::SwitchInt {
                        discriminant: Operand::Copy(Place {
                            local: Local(3),
                            projections: vec![Projection::Deref],
                        }),
                        targets: vec![(1, spark_mir::BlockId(1))],
                        otherwise: spark_mir::BlockId(1),
                    },
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![assign(0, Rvalue::Use(Operand::Constant(Constant::Int(0))))],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
            captures: vec![],
            is_async: false,
            span,
        };
        let mir = Mir {
            functions: std::collections::HashMap::from([(function.def_id, function)]),
        };
        let mut diagnostics = Diagnostics::new();
        super::super::analyze_program(&mir, &table, &mut diagnostics);
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "borrowed value does not live long enough"));
    }
}
