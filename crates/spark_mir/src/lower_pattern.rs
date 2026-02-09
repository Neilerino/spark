//! Pattern matching compilation: HIR match → MIR basic blocks.
//!
//! Match arms are compiled as sequential test-and-branch chains.
//! Each arm gets a test block, an arm-body block, and falls through
//! to the next arm's test on failure. Decision trees are deferred.

use spark_hir::{HirExpr, HirLiteral, HirMatchArm, HirPattern};
use spark_resolve::DefinitionTable;
use spark_types::TypeTable;

use crate::builder::{FunctionBuilder, MirBuilder};
use crate::lower_expr::lower_expr;
use crate::mir::{
    BlockId, Constant, Local, Operand, Place, Projection, Rvalue, Statement, Terminator,
};

/// Lowers a match expression. Returns the [`Local`] holding the result.
pub fn lower_match(
    scrutinee: &HirExpr,
    arms: &[HirMatchArm],
    expr: &HirExpr,
    fb: &mut FunctionBuilder,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) -> Local {
    let scrut_local = lower_expr(scrutinee, fb, mir, type_table, defs);
    let result = fb.fresh_temp(expr.ty, expr.span);
    let join_bb = fb.new_block();

    // Pre-create test blocks for each arm
    let arm_test_blocks: Vec<BlockId> = arms.iter().map(|_| fb.new_block()).collect();
    // The "otherwise" block after all arms is unreachable
    let otherwise_bb = fb.new_block();

    // Jump from current block to first arm's test
    if let Some(&first) = arm_test_blocks.first() {
        fb.terminate(Terminator::Goto { target: first });
    } else {
        // Empty match — should not happen after type checking
        fb.terminate(Terminator::Unreachable);
        fb.switch_to(join_bb);
        return result;
    }

    for (i, arm) in arms.iter().enumerate() {
        let test_bb = arm_test_blocks[i];
        let next_bb = arm_test_blocks
            .get(i + 1)
            .copied()
            .unwrap_or(otherwise_bb);
        let body_bb = fb.new_block();

        fb.switch_to(test_bb);

        // Compile pattern test + bindings
        compile_pattern(
            &arm.pattern,
            scrut_local,
            body_bb,
            next_bb,
            fb,
            mir,
            type_table,
            defs,
        );

        // Body block
        fb.switch_to(body_bb);

        // Optional guard
        if let Some(guard) = &arm.guard {
            let guard_local = lower_expr(guard, fb, mir, type_table, defs);
            let guarded_body_bb = fb.new_block();
            fb.terminate(Terminator::SwitchInt {
                discriminant: Operand::Copy(Place::from_local(guard_local)),
                targets: vec![(1, guarded_body_bb)],
                otherwise: next_bb,
            });
            fb.switch_to(guarded_body_bb);
        }

        let arm_val = lower_expr(&arm.body, fb, mir, type_table, defs);
        if !fb.is_terminated() {
            fb.push_stmt(Statement::Assign {
                place: Place::from_local(result),
                rvalue: Rvalue::Use(Operand::Copy(Place::from_local(arm_val))),
                span: arm.span,
            });
            fb.terminate(Terminator::Goto { target: join_bb });
        }
    }

    // Otherwise block (should be unreachable if match is exhaustive)
    fb.switch_to(otherwise_bb);
    fb.terminate(Terminator::Unreachable);

    fb.switch_to(join_bb);
    result
}

/// Compiles a single pattern into test-and-branch MIR.
///
/// On match, jumps to `match_bb` (bindings are set up before the jump).
/// On failure, jumps to `fail_bb`.
#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::only_used_in_recursion)]
fn compile_pattern(
    pattern: &HirPattern,
    scrut: Local,
    match_bb: BlockId,
    fail_bb: BlockId,
    fb: &mut FunctionBuilder,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) {
    match pattern {
        HirPattern::Wildcard => {
            // Always matches
            fb.terminate(Terminator::Goto { target: match_bb });
        }

        HirPattern::Binding { def_id } => {
            // Bind scrutinee to a local
            let name = defs.get(*def_id).map(|info| info.name);
            let ty = type_table
                .get_def_type(*def_id)
                .unwrap_or(spark_types::TypeId::ERROR);
            let local = match name {
                Some(sym) => fb.fresh_local(ty, sym, false, spark_span::Span::DUMMY),
                None => fb.fresh_temp(ty, spark_span::Span::DUMMY),
            };
            fb.register_def(*def_id, local);
            fb.push_stmt(Statement::Assign {
                place: Place::from_local(local),
                rvalue: Rvalue::Use(Operand::Copy(Place::from_local(scrut))),
                span: spark_span::Span::DUMMY,
            });
            fb.terminate(Terminator::Goto { target: match_bb });
        }

        HirPattern::Literal(lit) => {
            let constant = literal_to_constant(lit);
            let lit_temp = fb.fresh_temp(spark_types::TypeId::ERROR, spark_span::Span::DUMMY);
            fb.push_assign(
                Place::from_local(lit_temp),
                Rvalue::Use(Operand::Constant(constant.clone())),
                spark_span::Span::DUMMY,
            );

            // Compare scrutinee to literal
            match &constant {
                Constant::Int(v) => {
                    // Use SwitchInt for integer comparison
                    let val = i128::from(*v);
                    fb.terminate(Terminator::SwitchInt {
                        discriminant: Operand::Copy(Place::from_local(scrut)),
                        targets: vec![(val, match_bb)],
                        otherwise: fail_bb,
                    });
                }
                Constant::Bool(v) => {
                    let val = i128::from(*v);
                    fb.terminate(Terminator::SwitchInt {
                        discriminant: Operand::Copy(Place::from_local(scrut)),
                        targets: vec![(val, match_bb)],
                        otherwise: fail_bb,
                    });
                }
                Constant::Char(c) => {
                    let val = i128::from(*c as u32);
                    fb.terminate(Terminator::SwitchInt {
                        discriminant: Operand::Copy(Place::from_local(scrut)),
                        targets: vec![(val, match_bb)],
                        otherwise: fail_bb,
                    });
                }
                _ => {
                    // For String/Float/Null, fall through to match_bb
                    // (a real compiler would need equality checks; we simplify here)
                    fb.terminate(Terminator::Goto { target: match_bb });
                }
            }
        }

        HirPattern::Variant { def_id, fields } => {
            // Test discriminant, then bind fields
            let disc_temp =
                fb.fresh_temp(spark_types::TypeId::ERROR, spark_span::Span::DUMMY);
            fb.push_assign(
                Place::from_local(disc_temp),
                Rvalue::Discriminant(Place::from_local(scrut)),
                spark_span::Span::DUMMY,
            );

            // Use the variant DefId's raw u32 as the discriminant value
            let disc_val = i128::from(def_id.as_u32());
            let bind_bb = fb.new_block();
            fb.terminate(Terminator::SwitchInt {
                discriminant: Operand::Copy(Place::from_local(disc_temp)),
                targets: vec![(disc_val, bind_bb)],
                otherwise: fail_bb,
            });

            fb.switch_to(bind_bb);

            // Bind sub-patterns to projected fields
            for (idx, sub_pat) in fields.iter().enumerate() {
                let field_temp =
                    fb.fresh_temp(spark_types::TypeId::ERROR, spark_span::Span::DUMMY);
                let mut place = Place::from_local(scrut);
                place.projections.push(Projection::Field(
                    spark_intern::sym(&idx.to_string()),
                ));
                fb.push_assign(
                    Place::from_local(field_temp),
                    Rvalue::Use(Operand::Copy(place)),
                    spark_span::Span::DUMMY,
                );

                // Recursively compile sub-pattern
                let sub_match_bb = fb.new_block();
                compile_pattern(sub_pat, field_temp, sub_match_bb, fail_bb, fb, mir, type_table, defs);
                fb.switch_to(sub_match_bb);
            }

            fb.terminate(Terminator::Goto { target: match_bb });
        }

        HirPattern::Struct { def_id: _, fields, rest: _ } => {
            // For struct patterns, project each field and match sub-patterns
            for (field_name, sub_pat) in fields {
                let field_temp =
                    fb.fresh_temp(spark_types::TypeId::ERROR, spark_span::Span::DUMMY);
                let mut place = Place::from_local(scrut);
                place.projections.push(Projection::Field(*field_name));
                fb.push_assign(
                    Place::from_local(field_temp),
                    Rvalue::Use(Operand::Copy(place)),
                    spark_span::Span::DUMMY,
                );

                let sub_match_bb = fb.new_block();
                compile_pattern(sub_pat, field_temp, sub_match_bb, fail_bb, fb, mir, type_table, defs);
                fb.switch_to(sub_match_bb);
            }
            fb.terminate(Terminator::Goto { target: match_bb });
        }

        HirPattern::Tuple(elements) => {
            for (idx, sub_pat) in elements.iter().enumerate() {
                let field_temp =
                    fb.fresh_temp(spark_types::TypeId::ERROR, spark_span::Span::DUMMY);
                let mut place = Place::from_local(scrut);
                place.projections.push(Projection::Field(
                    spark_intern::sym(&idx.to_string()),
                ));
                fb.push_assign(
                    Place::from_local(field_temp),
                    Rvalue::Use(Operand::Copy(place)),
                    spark_span::Span::DUMMY,
                );

                let sub_match_bb = fb.new_block();
                compile_pattern(sub_pat, field_temp, sub_match_bb, fail_bb, fb, mir, type_table, defs);
                fb.switch_to(sub_match_bb);
            }
            fb.terminate(Terminator::Goto { target: match_bb });
        }

        HirPattern::Or(alternatives) => {
            // Try each alternative in sequence; if any matches, jump to match_bb
            for (i, alt) in alternatives.iter().enumerate() {
                let alt_match_bb = fb.new_block();
                let alt_fail_bb = if i + 1 < alternatives.len() {
                    fb.new_block()
                } else {
                    fail_bb
                };
                compile_pattern(alt, scrut, alt_match_bb, alt_fail_bb, fb, mir, type_table, defs);

                // If this alternative matched, go to the outer match_bb
                fb.switch_to(alt_match_bb);
                fb.terminate(Terminator::Goto { target: match_bb });

                // If it failed and there are more alternatives, try the next one
                if i + 1 < alternatives.len() {
                    fb.switch_to(alt_fail_bb);
                }
            }
        }

        HirPattern::TypeNarrow { def_id, ty } => {
            // Type narrowing: bind the scrutinee with the narrowed type.
            // Full runtime type testing is deferred — for now, we bind and proceed.
            let name = defs.get(*def_id).map(|info| info.name);
            let local = match name {
                Some(sym) => fb.fresh_local(*ty, sym, false, spark_span::Span::DUMMY),
                None => fb.fresh_temp(*ty, spark_span::Span::DUMMY),
            };
            fb.register_def(*def_id, local);
            fb.push_stmt(Statement::Assign {
                place: Place::from_local(local),
                rvalue: Rvalue::Use(Operand::Copy(Place::from_local(scrut))),
                span: spark_span::Span::DUMMY,
            });
            fb.terminate(Terminator::Goto { target: match_bb });
        }

        HirPattern::Error => {
            fb.terminate(Terminator::Goto { target: match_bb });
        }
    }
}

/// Converts an HIR literal to a MIR constant.
const fn literal_to_constant(lit: &HirLiteral) -> Constant {
    match lit {
        HirLiteral::Int(v) => Constant::Int(*v),
        HirLiteral::Float(v) => Constant::Float(*v),
        HirLiteral::Bool(v) => Constant::Bool(*v),
        HirLiteral::String(s) => Constant::String(*s),
        HirLiteral::Char(c) => Constant::Char(*c),
        HirLiteral::Null => Constant::Null,
    }
}
