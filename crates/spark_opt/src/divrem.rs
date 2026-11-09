//! Strength reduction for signed division and remainder by positive powers of two.

use rustc_hash::FxHashMap;
use spark_ast::BinaryOp;
use spark_mir::{Constant, Local, LocalDecl, MirFunction, Operand, Place, Rvalue, Statement};
use spark_span::Span;
use spark_types::{IntWidth, Ty, TypeId, TypeTable};

pub fn reduce_signed_divrem(func: &mut MirFunction, table: &TypeTable) -> bool {
    let borrowed = func.address_taken_locals();

    let mut changed = false;
    for block in &mut func.blocks {
        let mut constants = FxHashMap::default();
        let mut statements = Vec::with_capacity(block.statements.len());
        for mut statement in std::mem::take(&mut block.statements) {
            if let Statement::Assign {
                place,
                rvalue,
                span,
            } = &mut statement
            {
                if let Some((ty, bits, divisor, op, lhs)) =
                    candidate(place, rvalue, &func.locals, &constants, table)
                {
                    let mut builder = Rewrite {
                        locals: &mut func.locals,
                        statements: &mut statements,
                        span: *span,
                        ty,
                        bits,
                    };
                    *rvalue = builder.reduce(op, lhs, divisor);
                    changed = true;
                }
                let constant = match rvalue {
                    Rvalue::Use(operand) => constant_int(operand, &constants),
                    Rvalue::Cast { operand, target_ty } => {
                        signed_bits(*target_ty, table).and_then(|bits| {
                            constant_int(operand, &constants).filter(|&value| {
                                bits == 64 || {
                                    let limit = 1_i64 << (bits - 1);
                                    (-limit..limit).contains(&value)
                                }
                            })
                        })
                    }
                    _ => None,
                };
                if let Some(value) = constant
                    .filter(|_| place.projections.is_empty() && !borrowed.contains(&place.local))
                {
                    constants.insert(place.local, value);
                } else {
                    constants.remove(&place.local);
                }
            }
            statements.push(statement);
        }
        block.statements = statements;
    }
    changed
}

fn candidate(
    place: &Place,
    rvalue: &Rvalue,
    locals: &[LocalDecl],
    constants: &FxHashMap<Local, i64>,
    table: &TypeTable,
) -> Option<(TypeId, u32, i64, BinaryOp, Operand)> {
    let Rvalue::BinaryOp(op @ (BinaryOp::Div | BinaryOp::Mod), lhs, rhs) = rvalue else {
        return None;
    };
    if !place.projections.is_empty() {
        return None;
    }
    let ty = locals[place.local.index()].ty;
    let bits = signed_bits(ty, table)?;
    let operand_bits = |operand: &Operand| match operand {
        Operand::Constant(Constant::Int(_)) => Some(64),
        Operand::Copy(place) | Operand::Move(place) if place.projections.is_empty() => {
            signed_bits(locals[place.local.index()].ty, table)
        }
        _ => None,
    };
    if operand_bits(lhs)? != bits || operand_bits(rhs)? != bits {
        return None;
    }
    let divisor = constant_int(rhs, constants)?;
    let magnitude = u64::try_from(divisor).ok()?;
    (magnitude.is_power_of_two() && magnitude.trailing_zeros() < bits - 1)
        .then(|| (ty, bits, divisor, *op, lhs.clone()))
}

fn constant_int(operand: &Operand, constants: &FxHashMap<Local, i64>) -> Option<i64> {
    match operand {
        Operand::Constant(Constant::Int(value)) => Some(*value),
        Operand::Copy(place) | Operand::Move(place) if place.projections.is_empty() => {
            constants.get(&place.local).copied()
        }
        _ => None,
    }
}

fn signed_bits(ty: TypeId, table: &TypeTable) -> Option<u32> {
    let resolved = match table.resolve(ty) {
        Ty::Mutable(inner) => table.resolve(*inner),
        ty => ty,
    };
    match resolved {
        Ty::Int(IntWidth::I8) => Some(8),
        Ty::Int(IntWidth::I16) => Some(16),
        Ty::Int(IntWidth::I32) => Some(32),
        Ty::Int(IntWidth::I64) => Some(64),
        _ => None,
    }
}

struct Rewrite<'a> {
    locals: &'a mut Vec<LocalDecl>,
    statements: &'a mut Vec<Statement>,
    span: Span,
    ty: TypeId,
    bits: u32,
}

impl Rewrite<'_> {
    fn reduce(&mut self, op: BinaryOp, lhs: Operand, divisor: i64) -> Rvalue {
        if divisor == 1 {
            return if op == BinaryOp::Div {
                Rvalue::Use(lhs)
            } else {
                // Keep the dividend live so MIR DCE preserves any traps while computing it.
                Rvalue::BinaryOp(BinaryOp::BitAnd, lhs, self.immediate(0))
            };
        }

        let shift = self.immediate(i64::from(divisor.trailing_zeros()));
        let sign_shift = self.immediate(i64::from(self.bits - 1));
        let mask = self.immediate(divisor - 1);
        let sign = self.temp(Rvalue::BinaryOp(BinaryOp::Shr, lhs.clone(), sign_shift));
        let bias = self.temp(Rvalue::BinaryOp(BinaryOp::BitAnd, sign, mask));
        let adjusted = self.temp(Rvalue::BinaryOp(BinaryOp::Add, lhs.clone(), bias));
        let quotient = Rvalue::BinaryOp(BinaryOp::Shr, adjusted, shift.clone());
        if op == BinaryOp::Div {
            quotient
        } else {
            let quotient = self.temp(quotient);
            let product = self.temp(Rvalue::BinaryOp(BinaryOp::Shl, quotient, shift));
            Rvalue::BinaryOp(BinaryOp::Sub, lhs, product)
        }
    }

    fn immediate(&mut self, value: i64) -> Operand {
        let operand = Operand::Constant(Constant::Int(value));
        if self.bits == 64 {
            operand
        } else {
            self.temp(Rvalue::Cast {
                operand,
                target_ty: self.ty,
            })
        }
    }

    fn temp(&mut self, rvalue: Rvalue) -> Operand {
        let local = Local(u32::try_from(self.locals.len()).expect("MIR local count fits"));
        self.locals.push(LocalDecl {
            local,
            ty: self.ty,
            name: None,
            is_user_variable: false,
            mutable: false,
            span: self.span,
        });
        let place = Place::from_local(local);
        self.statements.push(Statement::Assign {
            place: place.clone(),
            rvalue,
            span: self.span,
        });
        Operand::Copy(place)
    }
}
