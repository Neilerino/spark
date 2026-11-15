// TAG [SPARK-154]
//! Normalize persistent referents to explicit frame places.

use rustc_hash::FxHashMap;
use spark_diagnostics::{Diagnostic, Diagnostics};
use spark_intern::Symbol;
use spark_types::{TypeId, TypeTable};

use crate::{
    BasicBlock, BlockId, Local, LocalDecl, MirFunction, Operand, Place, Projection, Rvalue,
    Statement, Terminator,
};

struct Promotion<'a> {
    roots: &'a FxHashMap<Local, Symbol>,
    locals: &'a mut Vec<LocalDecl>,
    table: &'a TypeTable,
    diagnostics: &'a mut Diagnostics,
    span: spark_span::Span,
}

impl Promotion<'_> {
    fn frame_place(&self, local: Local) -> Place {
        self.roots.get(&local).map_or_else(
            || Place::from_local(local),
            |field| Place {
                local: Local(1),
                projections: vec![Projection::Deref, Projection::Field(*field)],
            },
        )
    }

    fn temporary(&mut self, ty: TypeId) -> Local {
        super::poll_local(self.locals, ty, self.span)
    }

    const fn assign(&self, place: Place, rvalue: Rvalue) -> Statement {
        Statement::Assign {
            place,
            rvalue,
            span: self.span,
        }
    }

    fn snapshot(
        &mut self,
        local: &mut Local,
        consume: bool,
        before: &mut Vec<Statement>,
        after: &mut Vec<Statement>,
    ) {
        if !self.roots.contains_key(local) {
            return;
        }
        let ty = self.locals[local.index()].ty;
        let source = self.frame_place(*local);
        let temporary = self.temporary(ty);
        let operand = if consume {
            Operand::Move(source)
        } else {
            Operand::Copy(source)
        };
        before.push(self.assign(Place::from_local(temporary), Rvalue::Use(operand)));
        if !consume && spark_drop::needs_drop(ty, self.table) {
            after.push(Statement::Drop {
                place: Place::from_local(temporary),
                span: self.span,
            });
        }
        *local = temporary;
    }

    fn place(
        &mut self,
        place: &mut Place,
        before: &mut Vec<Statement>,
        after: &mut Vec<Statement>,
    ) {
        let mut base = self.frame_place(place.local);
        for projection in &mut place.projections {
            if let Projection::Index(index) = projection {
                self.snapshot(index, false, before, after);
            }
        }
        base.projections.append(&mut place.projections);
        *place = base;
    }

    fn operand(
        &mut self,
        operand: &mut Operand,
        before: &mut Vec<Statement>,
        after: &mut Vec<Statement>,
    ) {
        match operand {
            Operand::Copy(place) | Operand::Move(place) => self.place(place, before, after),
            Operand::Constant(_) => {}
        }
    }

    fn rvalue(
        &mut self,
        rvalue: &mut Rvalue,
        before: &mut Vec<Statement>,
        after: &mut Vec<Statement>,
    ) {
        match rvalue {
            Rvalue::Use(operand) | Rvalue::UnaryOp(_, operand) | Rvalue::Cast { operand, .. } => {
                self.operand(operand, before, after);
            }
            Rvalue::BinaryOp(_, left, right) => {
                self.operand(left, before, after);
                self.operand(right, before, after);
            }
            Rvalue::Ref { place, .. } | Rvalue::Discriminant(place) => {
                self.place(place, before, after);
            }
            Rvalue::Aggregate(_, operands) | Rvalue::StringInterp(operands) => {
                for operand in operands {
                    self.operand(operand, before, after);
                }
            }
            Rvalue::ListRange {
                start, end, step, ..
            }
            | Rvalue::RangeLen {
                start, end, step, ..
            } => {
                for operand in [start, end, step] {
                    self.operand(operand, before, after);
                }
            }
            Rvalue::CollectionGetOptional { collection, index } => {
                self.place(collection, before, after);
                self.place(index, before, after);
            }
            Rvalue::ReadFutureResult { pointer } => self.snapshot(pointer, false, before, after),
        }
    }

    fn destination_type(&mut self, place: &Place) -> TypeId {
        crate::mir::place_type(self.locals, place, self.table).unwrap_or_else(|| {
            self.diagnostics.push(Diagnostic::error(
                "invalid destination type during async frame promotion",
            ));
            TypeId::ERROR
        })
    }

    fn statement(&mut self, statement: Statement, output: &mut Vec<Statement>) {
        let mut before = Vec::new();
        let mut after = Vec::new();
        match statement {
            Statement::Assign {
                mut place,
                mut rvalue,
                span,
            } => {
                self.rvalue(&mut rvalue, &mut before, &mut after);
                let mut destination_before = Vec::new();
                let mut destination_after = Vec::new();
                self.place(&mut place, &mut destination_before, &mut destination_after);
                if place.projections.is_empty() {
                    before.push(Statement::Assign {
                        place,
                        rvalue,
                        span,
                    });
                } else {
                    let ty = self.destination_type(&place);
                    let temporary = self.temporary(ty);
                    before.push(Statement::Assign {
                        place: Place::from_local(temporary),
                        rvalue,
                        span,
                    });
                    before.append(&mut after);
                    before.append(&mut destination_before);
                    before.push(Statement::Assign {
                        place,
                        rvalue: Rvalue::Use(Operand::Move(Place::from_local(temporary))),
                        span,
                    });
                }
                before.append(&mut after);
                before.append(&mut destination_after);
            }
            Statement::Drop { mut place, span } => {
                self.place(&mut place, &mut before, &mut after);
                before.push(Statement::Drop { place, span });
                before.append(&mut after);
            }
            Statement::IndexedAssign {
                mut root,
                mut indices,
                mut value,
                span,
            } => {
                self.place(&mut root, &mut before, &mut after);
                for index in &mut indices {
                    self.snapshot(index, true, &mut before, &mut after);
                }
                self.snapshot(&mut value, true, &mut before, &mut after);
                before.push(Statement::IndexedAssign {
                    root,
                    indices,
                    value,
                    span,
                });
                before.append(&mut after);
            }
            Statement::Nop => before.push(Statement::Nop),
        }
        output.extend(before);
    }

    fn edge(target: BlockId, statements: Vec<Statement>, blocks: &mut Vec<BasicBlock>) -> BlockId {
        if statements.is_empty() {
            return target;
        }
        let block = BlockId(blocks.len() as u32);
        blocks.push(BasicBlock {
            statements,
            terminator: Terminator::Goto { target },
            is_cleanup: false,
        });
        block
    }

    fn terminator(
        &mut self,
        mut terminator: Terminator,
        before: &mut Vec<Statement>,
        blocks: &mut Vec<BasicBlock>,
    ) -> Terminator {
        let mut after = Vec::new();
        match &mut terminator {
            Terminator::Call {
                func,
                args,
                destination,
                target,
                ..
            } => {
                self.operand(func, before, &mut after);
                for argument in args {
                    self.operand(argument, before, &mut after);
                }
                let mut stores = Vec::new();
                let mut cleanup = Vec::new();
                self.place(destination, &mut stores, &mut cleanup);
                if !destination.projections.is_empty() {
                    let ty = self.destination_type(destination);
                    let temporary = self.temporary(ty);
                    stores.push(self.assign(
                        destination.clone(),
                        Rvalue::Use(Operand::Move(Place::from_local(temporary))),
                    ));
                    *destination = Place::from_local(temporary);
                }
                stores.append(&mut cleanup);
                stores.append(&mut after);
                *target = Self::edge(*target, stores, blocks);
            }
            Terminator::SwitchInt {
                discriminant,
                targets,
                otherwise,
            } => {
                self.operand(discriminant, before, &mut after);
                for (_, target) in targets {
                    *target = Self::edge(*target, after.clone(), blocks);
                }
                *otherwise = Self::edge(*otherwise, after, blocks);
            }
            Terminator::Goto { .. } | Terminator::Return | Terminator::Unreachable => {}
        }
        terminator
    }
}

pub(super) fn promote(
    function: &mut MirFunction,
    roots: &FxHashMap<Local, Symbol>,
    table: &TypeTable,
    diagnostics: &mut Diagnostics,
) {
    if roots.is_empty() {
        return;
    }
    let original = std::mem::take(&mut function.blocks);
    function.blocks.resize_with(original.len(), || BasicBlock {
        statements: vec![],
        terminator: Terminator::Unreachable,
        is_cleanup: false,
    });
    let mut promotion = Promotion {
        roots,
        locals: &mut function.locals,
        table,
        diagnostics,
        span: function.span,
    };
    for (index, block) in original.into_iter().enumerate() {
        let mut statements = Vec::new();
        for statement in block.statements {
            promotion.statement(statement, &mut statements);
        }
        let terminator =
            promotion.terminator(block.terminator, &mut statements, &mut function.blocks);
        function.blocks[index] = BasicBlock {
            statements,
            terminator,
            is_cleanup: block.is_cleanup,
        };
    }
}
