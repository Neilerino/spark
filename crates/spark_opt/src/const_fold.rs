//! Pass 2: Constant folding.
//!
//! Evaluates constant `BinaryOp` and `UnaryOp` operations at compile time.
//! When both operands of a binary op (or the single operand of a unary op)
//! are constants, the result is computed and the rvalue is replaced with
//! `Rvalue::Use(Operand::Constant(result))`.
//!
//! Division/modulo by zero and integer overflow are *not* folded — those
//! are left to the runtime to handle.

use spark_ast::{BinaryOp, UnaryOp};
use spark_mir::{Constant, MirFunction, Operand, Rvalue, Statement};

/// Folds constant expressions in all blocks of `func`.
///
/// Returns `true` if any rvalue was folded.
pub fn fold_constants(func: &mut MirFunction) -> bool {
    let mut changed = false;
    for block in &mut func.blocks {
        for stmt in &mut block.statements {
            if let Statement::Assign { rvalue, .. } = stmt {
                let folded = match rvalue {
                    Rvalue::BinaryOp(op, Operand::Constant(a), Operand::Constant(b)) => {
                        fold_binary(*op, a, b)
                    }
                    Rvalue::UnaryOp(op, Operand::Constant(a)) => fold_unary(*op, a),
                    _ => None,
                };
                if let Some(result) = folded {
                    *rvalue = Rvalue::Use(Operand::Constant(result));
                    changed = true;
                }
            }
        }
    }
    changed
}

/// Attempts to fold a binary operation on two constants.
///
/// Returns `None` if the operation cannot be folded (e.g. division by zero,
/// integer overflow, or mismatched types).
fn fold_binary(op: BinaryOp, lhs: &Constant, rhs: &Constant) -> Option<Constant> {
    match (lhs, rhs) {
        // Int × Int → Int or Bool
        (Constant::Int(a), Constant::Int(b)) => fold_int_binary(op, *a, *b),
        // Float × Float → Float or Bool
        (Constant::Float(a), Constant::Float(b)) => fold_float_binary(op, *a, *b),
        // Bool × Bool → Bool
        (Constant::Bool(a), Constant::Bool(b)) => fold_bool_binary(op, *a, *b),
        _ => None,
    }
}

/// Folds integer binary operations. Returns `None` on overflow or division by zero.
fn fold_int_binary(op: BinaryOp, a: i64, b: i64) -> Option<Constant> {
    match op {
        // Arithmetic (checked)
        BinaryOp::Add => a.checked_add(b).map(Constant::Int),
        BinaryOp::Sub => a.checked_sub(b).map(Constant::Int),
        BinaryOp::Mul => a.checked_mul(b).map(Constant::Int),
        BinaryOp::Div => {
            if b == 0 {
                None
            } else {
                a.checked_div(b).map(Constant::Int)
            }
        }
        BinaryOp::Mod => {
            if b == 0 {
                None
            } else {
                a.checked_rem(b).map(Constant::Int)
            }
        }
        // Comparisons → Bool
        BinaryOp::Eq => Some(Constant::Bool(a == b)),
        BinaryOp::Ne => Some(Constant::Bool(a != b)),
        BinaryOp::Lt => Some(Constant::Bool(a < b)),
        BinaryOp::Le => Some(Constant::Bool(a <= b)),
        BinaryOp::Gt => Some(Constant::Bool(a > b)),
        BinaryOp::Ge => Some(Constant::Bool(a >= b)),
        // Bitwise
        BinaryOp::BitAnd => Some(Constant::Int(a & b)),
        BinaryOp::BitOr => Some(Constant::Int(a | b)),
        BinaryOp::BitXor => Some(Constant::Int(a ^ b)),
        BinaryOp::Shl => {
            let shift = u32::try_from(b).ok()?;
            a.checked_shl(shift).map(Constant::Int)
        }
        BinaryOp::Shr => {
            let shift = u32::try_from(b).ok()?;
            a.checked_shr(shift).map(Constant::Int)
        }
        // Logical ops don't apply to Int
        BinaryOp::And | BinaryOp::Or => None,
    }
}

/// Folds floating-point binary operations. IEEE 754 rules apply (inf/NaN are valid).
#[allow(clippy::float_cmp)]
fn fold_float_binary(op: BinaryOp, a: f64, b: f64) -> Option<Constant> {
    match op {
        BinaryOp::Add => Some(Constant::Float(a + b)),
        BinaryOp::Sub => Some(Constant::Float(a - b)),
        BinaryOp::Mul => Some(Constant::Float(a * b)),
        BinaryOp::Div => Some(Constant::Float(a / b)),
        BinaryOp::Mod => Some(Constant::Float(a % b)),
        // Exact comparison is intentional — we're implementing the language's
        // `==`/`!=` semantics, not checking for approximate equality.
        BinaryOp::Eq => Some(Constant::Bool(a == b)),
        BinaryOp::Ne => Some(Constant::Bool(a != b)),
        BinaryOp::Lt => Some(Constant::Bool(a < b)),
        BinaryOp::Le => Some(Constant::Bool(a <= b)),
        BinaryOp::Gt => Some(Constant::Bool(a > b)),
        BinaryOp::Ge => Some(Constant::Bool(a >= b)),
        // Bitwise / logical don't apply to Float
        BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::Shl
        | BinaryOp::Shr => None,
    }
}

/// Folds boolean binary operations.
const fn fold_bool_binary(op: BinaryOp, a: bool, b: bool) -> Option<Constant> {
    match op {
        BinaryOp::And => Some(Constant::Bool(a && b)),
        BinaryOp::Or => Some(Constant::Bool(a || b)),
        BinaryOp::Eq => Some(Constant::Bool(a == b)),
        BinaryOp::Ne => Some(Constant::Bool(a != b)),
        _ => None,
    }
}

/// Attempts to fold a unary operation on a constant.
///
/// Returns `None` if the operation cannot be folded (e.g. negation of `i64::MIN`).
fn fold_unary(op: UnaryOp, operand: &Constant) -> Option<Constant> {
    match (op, operand) {
        (UnaryOp::Neg, Constant::Int(n)) => n.checked_neg().map(Constant::Int),
        (UnaryOp::Neg, Constant::Float(n)) => Some(Constant::Float(-n)),
        (UnaryOp::Not, Constant::Bool(b)) => Some(Constant::Bool(!b)),
        (UnaryOp::BitNot, Constant::Int(n)) => Some(Constant::Int(!n)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_mir::{BasicBlock, Local, LocalDecl, MirFunction, Place, Statement, Terminator};
    use spark_resolve::DefId;
    use spark_span::Span;
    use spark_types::TypeId;

    fn make_func(blocks: Vec<BasicBlock>) -> MirFunction {
        MirFunction {
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
            blocks,
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        }
    }

    fn binary_assign(local: u32, op: BinaryOp, a: Constant, b: Constant) -> Statement {
        Statement::Assign {
            place: Place::from_local(Local(local)),
            rvalue: Rvalue::BinaryOp(op, Operand::Constant(a), Operand::Constant(b)),
            span: Span::DUMMY,
        }
    }

    fn unary_assign(local: u32, op: UnaryOp, a: Constant) -> Statement {
        Statement::Assign {
            place: Place::from_local(Local(local)),
            rvalue: Rvalue::UnaryOp(op, Operand::Constant(a)),
            span: Span::DUMMY,
        }
    }

    fn expect_int(func: &MirFunction, block: usize, stmt: usize) -> i64 {
        match &func.blocks[block].statements[stmt] {
            Statement::Assign {
                rvalue: Rvalue::Use(Operand::Constant(Constant::Int(n))),
                ..
            } => *n,
            other => panic!("expected folded int constant, got {other:?}"),
        }
    }

    fn expect_bool(func: &MirFunction, block: usize, stmt: usize) -> bool {
        match &func.blocks[block].statements[stmt] {
            Statement::Assign {
                rvalue: Rvalue::Use(Operand::Constant(Constant::Bool(b))),
                ..
            } => *b,
            other => panic!("expected folded bool constant, got {other:?}"),
        }
    }

    fn expect_float(func: &MirFunction, block: usize, stmt: usize) -> f64 {
        match &func.blocks[block].statements[stmt] {
            Statement::Assign {
                rvalue: Rvalue::Use(Operand::Constant(Constant::Float(f))),
                ..
            } => *f,
            other => panic!("expected folded float constant, got {other:?}"),
        }
    }

    // ── Integer arithmetic ──────────────────────────────────────────

    #[test]
    fn fold_int_add() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Add,
                Constant::Int(2),
                Constant::Int(3),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert_eq!(expect_int(&func, 0, 0), 5);
    }

    #[test]
    fn fold_int_sub() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Sub,
                Constant::Int(10),
                Constant::Int(3),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert_eq!(expect_int(&func, 0, 0), 7);
    }

    #[test]
    fn fold_int_mul() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Mul,
                Constant::Int(4),
                Constant::Int(5),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert_eq!(expect_int(&func, 0, 0), 20);
    }

    #[test]
    fn fold_int_div() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Div,
                Constant::Int(15),
                Constant::Int(3),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert_eq!(expect_int(&func, 0, 0), 5);
    }

    #[test]
    fn fold_int_div_by_zero_not_folded() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Div,
                Constant::Int(10),
                Constant::Int(0),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(!fold_constants(&mut func));
        // Should still be BinaryOp, not folded
        assert!(matches!(
            &func.blocks[0].statements[0],
            Statement::Assign {
                rvalue: Rvalue::BinaryOp(BinaryOp::Div, _, _),
                ..
            }
        ));
    }

    #[test]
    fn fold_int_mod() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Mod,
                Constant::Int(10),
                Constant::Int(3),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert_eq!(expect_int(&func, 0, 0), 1);
    }

    #[test]
    fn fold_int_mod_by_zero_not_folded() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Mod,
                Constant::Int(10),
                Constant::Int(0),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(!fold_constants(&mut func));
    }

    #[test]
    fn fold_int_overflow_not_folded() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Add,
                Constant::Int(i64::MAX),
                Constant::Int(1),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(!fold_constants(&mut func));
    }

    // ── Integer comparisons ──────────────────────────────────────────

    #[test]
    fn fold_int_eq() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Eq,
                Constant::Int(5),
                Constant::Int(5),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert!(expect_bool(&func, 0, 0));
    }

    #[test]
    fn fold_int_ne() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Ne,
                Constant::Int(5),
                Constant::Int(3),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert!(expect_bool(&func, 0, 0));
    }

    #[test]
    fn fold_int_lt() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Lt,
                Constant::Int(3),
                Constant::Int(5),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert!(expect_bool(&func, 0, 0));
    }

    #[test]
    fn fold_int_le() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Le,
                Constant::Int(5),
                Constant::Int(5),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert!(expect_bool(&func, 0, 0));
    }

    #[test]
    fn fold_int_gt() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Gt,
                Constant::Int(5),
                Constant::Int(3),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert!(expect_bool(&func, 0, 0));
    }

    #[test]
    fn fold_int_ge() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Ge,
                Constant::Int(5),
                Constant::Int(5),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert!(expect_bool(&func, 0, 0));
    }

    // ── Integer bitwise ──────────────────────────────────────────

    #[test]
    fn fold_int_bitand() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::BitAnd,
                Constant::Int(0b1010),
                Constant::Int(0b1100),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert_eq!(expect_int(&func, 0, 0), 0b1000);
    }

    #[test]
    fn fold_int_bitor() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::BitOr,
                Constant::Int(0b1010),
                Constant::Int(0b0101),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert_eq!(expect_int(&func, 0, 0), 0b1111);
    }

    #[test]
    fn fold_int_bitxor() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::BitXor,
                Constant::Int(0b1010),
                Constant::Int(0b1100),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert_eq!(expect_int(&func, 0, 0), 0b0110);
    }

    #[test]
    fn fold_int_shl() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Shl,
                Constant::Int(1),
                Constant::Int(4),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert_eq!(expect_int(&func, 0, 0), 16);
    }

    #[test]
    fn fold_int_shr() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Shr,
                Constant::Int(16),
                Constant::Int(2),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert_eq!(expect_int(&func, 0, 0), 4);
    }

    #[test]
    fn fold_int_shl_negative_shift_not_folded() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Shl,
                Constant::Int(1),
                Constant::Int(-1),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(!fold_constants(&mut func));
    }

    // ── Float operations ──────────────────────────────────────────

    #[test]
    fn fold_float_add() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Add,
                Constant::Float(1.5),
                Constant::Float(2.5),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        let result = expect_float(&func, 0, 0);
        assert!((result - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn fold_float_div() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Div,
                Constant::Float(10.0),
                Constant::Float(3.0),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        let result = expect_float(&func, 0, 0);
        assert!((result - 10.0 / 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn fold_float_div_by_zero_gives_inf() {
        // IEEE 754: 1.0 / 0.0 = inf, which is a valid folding
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Div,
                Constant::Float(1.0),
                Constant::Float(0.0),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        let result = expect_float(&func, 0, 0);
        assert!(result.is_infinite());
    }

    #[test]
    fn fold_float_comparison() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Lt,
                Constant::Float(1.0),
                Constant::Float(2.0),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert!(expect_bool(&func, 0, 0));
    }

    // ── Bool operations ──────────────────────────────────────────

    #[test]
    fn fold_bool_and() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::And,
                Constant::Bool(true),
                Constant::Bool(false),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert!(!expect_bool(&func, 0, 0));
    }

    #[test]
    fn fold_bool_or() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Or,
                Constant::Bool(false),
                Constant::Bool(true),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert!(expect_bool(&func, 0, 0));
    }

    #[test]
    fn fold_bool_eq() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Eq,
                Constant::Bool(true),
                Constant::Bool(true),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert!(expect_bool(&func, 0, 0));
    }

    #[test]
    fn fold_bool_ne() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Ne,
                Constant::Bool(true),
                Constant::Bool(false),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert!(expect_bool(&func, 0, 0));
    }

    // ── Unary operations ──────────────────────────────────────────

    #[test]
    fn fold_unary_neg_int() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![unary_assign(0, UnaryOp::Neg, Constant::Int(42))],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert_eq!(expect_int(&func, 0, 0), -42);
    }

    #[test]
    fn fold_unary_neg_float() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![unary_assign(0, UnaryOp::Neg, Constant::Float(2.5))],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        let result = expect_float(&func, 0, 0);
        assert!((result - (-2.5)).abs() < f64::EPSILON);
    }

    #[test]
    fn fold_unary_not_bool() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![unary_assign(0, UnaryOp::Not, Constant::Bool(true))],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert!(!expect_bool(&func, 0, 0));
    }

    #[test]
    fn fold_unary_bitnot_int() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![unary_assign(0, UnaryOp::BitNot, Constant::Int(0))],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(fold_constants(&mut func));
        assert_eq!(expect_int(&func, 0, 0), -1);
    }

    #[test]
    fn fold_neg_i64_min_not_folded() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![unary_assign(0, UnaryOp::Neg, Constant::Int(i64::MIN))],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(!fold_constants(&mut func));
    }

    // ── Mixed types not folded ──────────────────────────────────

    #[test]
    fn mixed_types_not_folded() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![binary_assign(
                0,
                BinaryOp::Add,
                Constant::Int(1),
                Constant::Float(2.0),
            )],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(!fold_constants(&mut func));
    }

    #[test]
    fn non_constant_operands_not_folded() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![Statement::Assign {
                place: Place::from_local(Local(0)),
                rvalue: Rvalue::BinaryOp(
                    BinaryOp::Add,
                    Operand::Copy(Place::from_local(Local(1))),
                    Operand::Constant(Constant::Int(1)),
                ),
                span: Span::DUMMY,
            }],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(!fold_constants(&mut func));
    }

    #[test]
    fn no_foldable_returns_false() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![Statement::Assign {
                place: Place::from_local(Local(0)),
                rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                span: Span::DUMMY,
            }],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        assert!(!fold_constants(&mut func));
    }
}
