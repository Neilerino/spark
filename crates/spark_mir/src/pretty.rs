//! Pretty printer for MIR, used for debugging and test assertions.

use std::fmt::{self, Write as _};

use crate::mir::{
    AggregateKind, BasicBlock, Constant, Local, MirFunction, Operand, Rvalue, Statement,
    Terminator,
};

/// Pretty-prints a [`MirFunction`] into a human-readable string.
#[must_use]
pub fn pretty_function(func: &MirFunction) -> String {
    let mut out = String::new();
    PrettyPrinter { out: &mut out }.print_function(func);
    out
}

struct PrettyPrinter<'a> {
    out: &'a mut String,
}

impl PrettyPrinter<'_> {
    fn print_function(&mut self, func: &MirFunction) {
        // Header
        let _ = write!(self.out, "fn def{}(", func.def_id.as_u32());
        for (i, local) in func.locals.iter().enumerate().skip(1) {
            if i > func.param_count as usize + 1 {
                break;
            }
            if i > 1 {
                self.out.push_str(", ");
            }
            if let Some(name) = local.name {
                let _ = write!(self.out, "{}: {}", name.as_str(), local.local);
            } else {
                let _ = write!(self.out, "{}", local.local);
            }
        }
        self.out.push_str(") {\n");

        // Locals
        for local in &func.locals {
            let mutability = if local.mutable { "mut " } else { "" };
            let kind = if local.is_user_variable {
                "let"
            } else if local.local == Local::RETURN_PLACE {
                "ret"
            } else {
                "tmp"
            };
            if let Some(name) = local.name {
                let _ = writeln!(
                    self.out,
                    "    {kind} {mutability}{}: {} // {}",
                    local.local,
                    name.as_str(),
                    local.ty.as_u32()
                );
            } else {
                let _ = writeln!(
                    self.out,
                    "    {kind} {mutability}{}: ty{}",
                    local.local,
                    local.ty.as_u32()
                );
            }
        }
        self.out.push('\n');

        // Blocks
        for (i, block) in func.blocks.iter().enumerate() {
            self.print_block(i, block);
        }

        self.out.push_str("}\n");
    }

    fn print_block(&mut self, index: usize, block: &BasicBlock) {
        let _ = writeln!(self.out, "    bb{index}:");
        for stmt in &block.statements {
            self.out.push_str("        ");
            self.print_statement(stmt);
            self.out.push('\n');
        }
        self.out.push_str("        ");
        self.print_terminator(&block.terminator);
        self.out.push('\n');
    }

    fn print_statement(&mut self, stmt: &Statement) {
        match stmt {
            Statement::Assign {
                place, rvalue, ..
            } => {
                let _ = write!(self.out, "{place} = ");
                self.print_rvalue(rvalue);
            }
            Statement::Drop { place, .. } => {
                let _ = write!(self.out, "drop({place})");
            }
            Statement::Nop => {
                self.out.push_str("nop");
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn print_rvalue(&mut self, rvalue: &Rvalue) {
        match rvalue {
            Rvalue::Use(op) => self.print_operand(op),
            Rvalue::BinaryOp(op, lhs, rhs) => {
                self.print_operand(lhs);
                let _ = write!(self.out, " {} ", op.as_str());
                self.print_operand(rhs);
            }
            Rvalue::UnaryOp(op, operand) => {
                self.out.push_str(op.as_str());
                if matches!(op, spark_ast::UnaryOp::Not) {
                    self.out.push(' ');
                }
                self.print_operand(operand);
            }
            Rvalue::Ref { place, mutable } => {
                if *mutable {
                    let _ = write!(self.out, "&mut {place}");
                } else {
                    let _ = write!(self.out, "&{place}");
                }
            }
            Rvalue::Aggregate(kind, ops) => {
                match kind {
                    AggregateKind::Struct { def_id, fields } => {
                        let _ = write!(self.out, "Struct(def{}", def_id.as_u32());
                        for (field, op) in fields.iter().zip(ops) {
                            let _ = write!(self.out, ", {}: ", field.as_str());
                            self.print_operand(op);
                        }
                        self.out.push(')');
                    }
                    AggregateKind::EnumVariant { def_id, variant } => {
                        let _ = write!(
                            self.out,
                            "Variant(def{}, {}",
                            def_id.as_u32(),
                            variant.as_str()
                        );
                        for op in ops {
                            self.out.push_str(", ");
                            self.print_operand(op);
                        }
                        self.out.push(')');
                    }
                    AggregateKind::Tuple => {
                        self.out.push('(');
                        self.print_operand_list(ops);
                        self.out.push(')');
                    }
                    AggregateKind::List => {
                        self.out.push('[');
                        self.print_operand_list(ops);
                        self.out.push(']');
                    }
                    AggregateKind::Set => {
                        self.out.push_str("Set{");
                        self.print_operand_list(ops);
                        self.out.push('}');
                    }
                    AggregateKind::Map => {
                        self.out.push_str("Map{");
                        for (i, pair) in ops.chunks(2).enumerate() {
                            if i > 0 {
                                self.out.push_str(", ");
                            }
                            if pair.len() == 2 {
                                self.print_operand(&pair[0]);
                                self.out.push_str(": ");
                                self.print_operand(&pair[1]);
                            }
                        }
                        self.out.push('}');
                    }
                    AggregateKind::Closure { func_def_id } => {
                        let _ = write!(self.out, "Closure(def{}", func_def_id.as_u32());
                        for op in ops {
                            self.out.push_str(", ");
                            self.print_operand(op);
                        }
                        self.out.push(')');
                    }
                    AggregateKind::Union { union_ty, member_ty } => {
                        let _ = write!(
                            self.out,
                            "Union(ty{}, member ty{}, ",
                            union_ty.as_u32(),
                            member_ty.as_u32()
                        );
                        self.print_operand_list(ops);
                        self.out.push(')');
                    }
                    AggregateKind::Channel => {
                        self.out.push_str("Channel()");
                    }
                }
            }
            Rvalue::Discriminant(place) => {
                let _ = write!(self.out, "discriminant({place})");
            }
            Rvalue::Cast { operand, target_ty } => {
                self.print_operand(operand);
                let _ = write!(self.out, " as ty{}", target_ty.as_u32());
            }
            Rvalue::StringInterp(ops) => {
                self.out.push_str("StringInterp(");
                self.print_operand_list(ops);
                self.out.push(')');
            }
            Rvalue::ListRange { start, end, inclusive, step } => {
                self.out.push_str("ListRange(");
                self.print_operand(start);
                if *inclusive {
                    self.out.push_str("..=");
                } else {
                    self.out.push_str("..");
                }
                self.print_operand(end);
                self.out.push_str(", step=");
                self.print_operand(step);
                self.out.push(')');
            }
        }
    }

    fn print_operand(&mut self, op: &Operand) {
        match op {
            Operand::Copy(place) => {
                let _ = write!(self.out, "copy {place}");
            }
            Operand::Move(place) => {
                let _ = write!(self.out, "move {place}");
            }
            Operand::Constant(c) => self.print_constant(c),
        }
    }

    fn print_operand_list(&mut self, ops: &[Operand]) {
        for (i, op) in ops.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.print_operand(op);
        }
    }

    fn print_constant(&mut self, c: &Constant) {
        match c {
            Constant::Int(v) => {
                let _ = write!(self.out, "const {v}");
            }
            Constant::Float(v) => {
                let _ = write!(self.out, "const {v}");
            }
            Constant::Bool(v) => {
                let _ = write!(self.out, "const {v}");
            }
            Constant::String(s) => {
                let _ = write!(self.out, "const \"{}\"", s.as_str());
            }
            Constant::Char(c) => {
                let _ = write!(self.out, "const '{c}'");
            }
            Constant::Null => self.out.push_str("const null"),
            Constant::FunctionRef(def_id) => {
                let _ = write!(self.out, "fn(def{})", def_id.as_u32());
            }
            Constant::Void => self.out.push_str("const void"),
        }
    }

    fn print_terminator(&mut self, term: &Terminator) {
        match term {
            Terminator::Goto { target } => {
                let _ = write!(self.out, "goto -> {target}");
            }
            Terminator::Return => {
                self.out.push_str("return");
            }
            Terminator::SwitchInt {
                discriminant,
                targets,
                otherwise,
            } => {
                self.out.push_str("switchInt(");
                self.print_operand(discriminant);
                self.out.push_str(") -> [");
                for (i, (val, bb)) in targets.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    let _ = write!(self.out, "{val}: {bb}");
                }
                let _ = write!(self.out, ", otherwise: {otherwise}]");
            }
            Terminator::Call {
                func,
                args,
                destination,
                target,
                ..
            } => {
                let _ = write!(self.out, "{destination} = call ");
                self.print_operand(func);
                self.out.push('(');
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.print_operand(arg);
                }
                let _ = write!(self.out, ") -> {target}");
            }
            Terminator::Unreachable => {
                self.out.push_str("unreachable");
            }
        }
    }
}

/// Implement `Display` for [`MirFunction`] via the pretty printer.
impl fmt::Display for MirFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&pretty_function(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::{BasicBlock, LocalDecl, MirFunction, Place};
    use spark_resolve::DefId;
    use spark_span::Span;
    use spark_types::TypeId;

    #[test]
    fn pretty_empty_function() {
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

        let output = pretty_function(&func);
        assert!(output.contains("fn def0()"));
        assert!(output.contains("bb0:"));
        assert!(output.contains("return"));
    }

    #[test]
    fn pretty_constant_values() {
        let func = MirFunction {
            def_id: DefId::new(1),
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
            blocks: vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(1)),
                    rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };

        let output = pretty_function(&func);
        assert!(output.contains("_1 = const 42"));
    }
}
