//! # `spark_opt`
//!
//! MIR optimization passes for the Spark compiler.
//!
//! This crate transforms MIR to improve generated code quality. It sits
//! between borrow checking and code generation, operates on each function
//! independently, and runs a fixed set of passes in two iterations.
//!
//! ## Passes (in order)
//!
//! 1. **Nop elimination** — remove `Statement::Nop` placeholders
//! 2. **Constant folding** — evaluate constant `BinaryOp`/`UnaryOp` at compile time
//! 3. **Signed division strength reduction** — lower division and remainder by positive powers of two
//! 4. **CFG simplification** — merge goto chains, fold constant `SwitchInt`
//! 5. **Dead code elimination** — remove unreachable blocks and dead assignments
//! 6. **Copy propagation** — replace temp copies with source locals
//!
//! ## Quick Start
//!
//! ```ignore
//! use spark_opt::{optimize, OptInput};
//!
//! let input = OptInput {
//!     mir: borrow_output.mir,
//!     table: borrow_output.table,
//!     definitions: resolver_output.definitions,
//! };
//!
//! let output = optimize(input);
//!
//! if output.diagnostics.has_errors() {
//!     // Handle errors (should not happen)
//! } else {
//!     // Use output.mir for code generation
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::cast_possible_truncation)]

mod const_fold;
mod copy_prop;
mod dce;
mod deforest;
mod divrem;
mod inline;
mod nop_elim;
mod simplify_cfg;

use spark_diagnostics::Diagnostics;
use spark_mir::{Mir, MirFunction};
use spark_resolve::DefinitionTable;
use spark_types::TypeTable;

/// Controls whether optimization passes run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptLevel {
    /// Skip all optimization passes (pass MIR through unchanged).
    None,
    /// Run all optimization passes (default).
    #[default]
    Basic,
}

/// Input to the optimization pass.
#[derive(Debug)]
pub struct OptInput {
    /// The MIR to optimize.
    pub mir: Mir,
    /// The type table (passed through unchanged).
    pub table: TypeTable,
    /// Definition table from the resolver.
    pub definitions: DefinitionTable,
    /// Optimization level.
    pub opt_level: OptLevel,
}

/// Output from the optimization pass.
#[derive(Debug)]
pub struct OptOutput {
    /// The optimized MIR.
    pub mir: Mir,
    /// The type table (passed through unchanged).
    pub table: TypeTable,
    /// Diagnostics accumulated during optimization.
    pub diagnostics: Diagnostics,
}

/// Runs all optimization passes on the MIR.
///
/// Each function is optimized independently. The pass pipeline runs twice
/// to catch cascading effects (e.g. constant folding enables dead code
/// elimination which simplifies the CFG).
#[must_use]
pub fn optimize(input: OptInput) -> OptOutput {
    let OptInput {
        mut mir,
        table,
        definitions,
        opt_level,
    } = input;

    let diagnostics = Diagnostics::new();

    if opt_level != OptLevel::None {
        // Deforestation runs first: eliminates intermediate allocations
        // (e.g. ListRange → arithmetic) before per-function passes.
        for func in mir.functions.values_mut() {
            deforest::deforest(func, &definitions);
        }

        // Cross-function inlining runs next (needs the full MIR).
        inline::inline_calls(&mut mir);

        // Per-function optimization passes.
        for func in mir.functions.values_mut() {
            optimize_function(func, &table);
        }
    }

    OptOutput {
        mir,
        table,
        diagnostics,
    }
}

/// Runs all passes on a single function, with two fixed iterations.
fn optimize_function(func: &mut MirFunction, table: &TypeTable) {
    let changed = run_passes(func, table);
    if changed {
        run_passes(func, table);
    }
}

/// Runs the per-function optimization passes in order. Returns `true` if any
/// pass modified the function.
fn run_passes(func: &mut MirFunction, table: &TypeTable) -> bool {
    let mut changed = false;
    changed |= nop_elim::eliminate_nops(func);
    changed |= const_fold::fold_constants(func);
    changed |= divrem::reduce_signed_divrem(func, table);
    changed |= simplify_cfg::simplify_cfg(func);
    changed |= dce::eliminate_dead_code(func, table);
    changed |= copy_prop::propagate_copies(func, table);
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_mir::{
        BasicBlock, BlockId, Constant, Local, LocalDecl, Mir, Operand, Place, Rvalue, Statement,
        Terminator,
    };
    use spark_resolve::DefId;
    use spark_span::Span;
    use spark_types::TypeId;
    use std::collections::HashMap;

    /// Creates a minimal `OptInput` with no functions.
    fn empty_input() -> OptInput {
        OptInput {
            mir: Mir {
                functions: HashMap::new(),
            },
            table: TypeTable::new(),
            definitions: DefinitionTable::new(),
            opt_level: OptLevel::default(),
        }
    }

    #[test]
    fn empty_input_passes() {
        let output = optimize(empty_input());
        assert!(!output.diagnostics.has_errors());
        assert!(output.mir.functions.is_empty());
    }

    #[test]
    fn opt_level_none_skips_passes() {
        // Nops should survive when optimization is disabled.
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
                statements: vec![
                    Statement::Nop,
                    Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
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

        let mut functions = HashMap::new();
        functions.insert(DefId::new(0), func);

        let output = optimize(OptInput {
            mir: Mir { functions },
            table: TypeTable::new(),
            definitions: DefinitionTable::new(),
            opt_level: OptLevel::None,
        });

        assert!(!output.diagnostics.has_errors());
        // Nop should still be present — passes were skipped.
        let func = output.mir.functions.values().next().unwrap();
        assert_eq!(func.blocks[0].statements.len(), 2);
        assert!(matches!(func.blocks[0].statements[0], Statement::Nop));
    }

    #[test]
    fn passthrough_simple_function() {
        // A trivial function: _0 = const 42; return
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
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
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

        let mut functions = HashMap::new();
        functions.insert(DefId::new(0), func);

        let output = optimize(OptInput {
            mir: Mir { functions },
            table: TypeTable::new(),
            definitions: DefinitionTable::new(),
            opt_level: OptLevel::default(),
        });

        assert!(!output.diagnostics.has_errors());
        assert_eq!(output.mir.functions.len(), 1);

        let func = output.mir.functions.values().next().unwrap();
        assert_eq!(func.blocks.len(), 1);
        assert_eq!(func.blocks[0].statements.len(), 1);
    }

    #[test]
    fn nops_and_dead_code_cleaned_up() {
        // bb0: Nop; _0 = const 1; Nop; goto → bb1
        // bb1: _0 = const 2; return
        // bb2: _0 = const 3; return  (unreachable)
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
            blocks: vec![
                BasicBlock {
                    statements: vec![
                        Statement::Nop,
                        Statement::Assign {
                            place: Place::from_local(Local(0)),
                            rvalue: Rvalue::Use(Operand::Constant(Constant::Int(1))),
                            span: Span::DUMMY,
                        },
                        Statement::Nop,
                    ],
                    terminator: Terminator::Goto { target: BlockId(1) },
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(2))),
                        span: Span::DUMMY,
                    }],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(3))),
                        span: Span::DUMMY,
                    }],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };

        let mut functions = HashMap::new();
        functions.insert(DefId::new(0), func);

        let output = optimize(OptInput {
            mir: Mir { functions },
            table: TypeTable::new(),
            definitions: DefinitionTable::new(),
            opt_level: OptLevel::default(),
        });

        assert!(!output.diagnostics.has_errors());
        let func = output.mir.functions.values().next().unwrap();

        // Nops should be gone from bb0
        for stmt in &func.blocks[0].statements {
            assert!(!matches!(stmt, Statement::Nop), "Nop should be removed");
        }

        // bb2 should be cleared (unreachable)
        assert!(
            func.blocks[2].statements.is_empty(),
            "Unreachable block should be cleared"
        );
        assert!(
            matches!(func.blocks[2].terminator, Terminator::Unreachable),
            "Unreachable block terminator should be Unreachable"
        );
    }

    /// Full pipeline helper: lex → parse → resolve → type-check → HIR → MIR → optimize.
    fn optimize_source(source: &str) -> OptOutput {
        optimize_source_with_level(source, OptLevel::default())
    }

    fn optimize_source_with_level(source: &str, opt_level: OptLevel) -> OptOutput {
        use spark_lexer::Lexer;
        use spark_modules::{ModuleGraph, ModuleInfo, ModulePath};
        use spark_parser::parse;
        use spark_resolve::{resolve, ResolverInput};
        use spark_span::FileId;
        use spark_types::{check, TypeCheckerInput};

        fn make_graph(file_id: spark_span::FileId, ast: &spark_ast::Ast) -> ModuleGraph {
            use spark_ast::Item;

            let module_path = ModulePath::from_single(spark_intern::sym("test"));
            let mut graph = ModuleGraph::default();
            let mut module_info = ModuleInfo::new(module_path.clone());
            module_info.files.push(file_id);
            for item in &ast.items {
                let name = match item {
                    Item::Function(f) => Some(f.name.name),
                    Item::Struct(s) => Some(s.name.name),
                    Item::Enum(e) => Some(e.name.name),
                    Item::Interface(i) => Some(i.name.name),
                    Item::TypeAlias(t) => Some(t.name.name),
                    Item::TypeDef(t) => Some(t.name.name),
                    Item::Const(c) => Some(c.name.name),
                    Item::Binding(_) => None,
                };
                if let Some(n) = name {
                    module_info.exports.insert(n);
                }
            }
            graph.modules.insert(module_path.clone(), module_info);
            graph.compilation_order.push(module_path);
            graph
        }

        let file_id = FileId::new(0);
        let tokens = Lexer::new(file_id, source).tokenize();
        let (ast, _) = parse(tokens, file_id);
        let graph = make_graph(file_id, &ast);
        let mut asts = HashMap::new();
        asts.insert(file_id, ast);

        let resolver_output = resolve(ResolverInput { graph, asts });

        let type_output = check(TypeCheckerInput {
            resolved: &resolver_output.resolved,
            definitions: &resolver_output.definitions,
            graph: ModuleGraph::default(),
        });

        let hir_output = spark_hir::lower(spark_hir::HirInput {
            resolved: resolver_output.resolved,
            definitions: resolver_output.definitions,
            table: type_output.table,
            builtins: type_output.builtins,
        });

        let mir_output = spark_mir::lower(spark_mir::MirInput {
            hir: hir_output.hir,
            table: hir_output.table,
            definitions: DefinitionTable::new(),
        });

        optimize(OptInput {
            mir: mir_output.mir,
            table: mir_output.table,
            definitions: DefinitionTable::new(),
            opt_level,
        })
    }

    #[test]
    fn optimize_void_function() {
        let output = optimize_source("fn main(): Void { }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output
                .diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn optimize_arithmetic() {
        let output = optimize_source("fn f(): Int { return 2 + 3 }");
        assert!(!output.diagnostics.has_errors());
    }

    #[test]
    fn signed_divrem_rewrite_respects_opt_level() {
        use spark_ast::BinaryOp::{Div, Mod, Shr};

        let source = "fn f(x: Int, divisor: Int): Int {
            return x / 1 + x % 1 + x / 4 + x % 4 + x / divisor + x % divisor
        }";
        for level in [OptLevel::None, OptLevel::Basic] {
            let output = optimize_source_with_level(source, level);
            let func = output.mir.functions.values().next().expect("function");
            let operators: Vec<_> = func
                .blocks
                .iter()
                .flat_map(|block| &block.statements)
                .filter_map(|statement| match statement {
                    Statement::Assign {
                        rvalue: Rvalue::BinaryOp(op, ..),
                        ..
                    } => Some(*op),
                    _ => None,
                })
                .collect();

            let expected = if level == OptLevel::None { 3 } else { 1 };
            assert_eq!(operators.iter().filter(|&&op| op == Div).count(), expected);
            assert_eq!(operators.iter().filter(|&&op| op == Mod).count(), expected);
            assert_eq!(operators.contains(&Shr), level == OptLevel::Basic);
        }
    }

    #[test]
    fn optimize_if_else() {
        let output = optimize_source("fn f(x: Bool): Int { return if x { 1 } else { 2 } }");
        assert!(!output.diagnostics.has_errors());
    }

    #[test]
    fn optimize_multiple_functions() {
        let output = optimize_source(
            "fn a(): Int { return 1 }\nfn b(): Int { return 2 }\nfn c(): Int { return 3 }",
        );
        assert!(!output.diagnostics.has_errors());
        assert_eq!(output.mir.functions.len(), 3);
    }
}
