//! Pass 1: Nop elimination.
//!
//! Removes all `Statement::Nop` entries from basic blocks. This is the
//! simplest pass and runs first so other passes don't waste time
//! iterating over no-ops.

use spark_mir::{MirFunction, Statement};

/// Removes all `Statement::Nop` entries from every block in `func`.
///
/// Returns `true` if any nops were removed.
pub fn eliminate_nops(func: &mut MirFunction) -> bool {
    let mut changed = false;
    for block in &mut func.blocks {
        let before = block.statements.len();
        block.statements.retain(|s| !matches!(s, Statement::Nop));
        if block.statements.len() < before {
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_mir::{
        BasicBlock, Constant, Local, LocalDecl, MirFunction, Operand, Place, Rvalue, Statement,
        Terminator,
    };
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

    fn assign_int(local: u32, value: i64) -> Statement {
        Statement::Assign {
            place: Place::from_local(Local(local)),
            rvalue: Rvalue::Use(Operand::Constant(Constant::Int(value))),
            span: Span::DUMMY,
        }
    }

    #[test]
    fn removes_nops_from_mixed_block() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![Statement::Nop, assign_int(0, 42), Statement::Nop],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        let changed = eliminate_nops(&mut func);
        assert!(changed);
        assert_eq!(func.blocks[0].statements.len(), 1);
        assert!(matches!(
            &func.blocks[0].statements[0],
            Statement::Assign { .. }
        ));
    }

    #[test]
    fn all_nop_block() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![Statement::Nop, Statement::Nop, Statement::Nop],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        let changed = eliminate_nops(&mut func);
        assert!(changed);
        assert!(func.blocks[0].statements.is_empty());
    }

    #[test]
    fn no_nops_returns_false() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![assign_int(0, 1)],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        let changed = eliminate_nops(&mut func);
        assert!(!changed);
        assert_eq!(func.blocks[0].statements.len(), 1);
    }

    #[test]
    fn empty_block_returns_false() {
        let mut func = make_func(vec![BasicBlock {
            statements: vec![],
            terminator: Terminator::Return,
            is_cleanup: false,
        }]);

        let changed = eliminate_nops(&mut func);
        assert!(!changed);
    }

    #[test]
    fn multiple_blocks() {
        let mut func = make_func(vec![
            BasicBlock {
                statements: vec![Statement::Nop, assign_int(0, 1)],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![assign_int(0, 2), Statement::Nop],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
        ]);

        let changed = eliminate_nops(&mut func);
        assert!(changed);
        assert_eq!(func.blocks[0].statements.len(), 1);
        assert_eq!(func.blocks[1].statements.len(), 1);
    }
}
