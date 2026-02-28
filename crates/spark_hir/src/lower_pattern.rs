//! Pattern lowering.
//!
//! Converts AST [`Pattern`] nodes into HIR [`HirPattern`] nodes.
//! Guard expressions are extracted into the [`HirMatchArm`] guard field.

use spark_ast::{LiteralKind, Pattern, PatternKind};
use spark_diagnostics::Diagnostics;
use spark_resolve::{DefinitionTable, ResolutionMap};
use spark_types::{TypeId, TypeTable};

use crate::hir::{HirExpr, HirLiteral, HirPattern};
use crate::lower_expr::lower_expr;

/// Lowers a pattern, extracting any guard expression.
///
/// AST `Guard { pattern, guard }` is flattened: the inner pattern
/// is lowered normally, and the guard expression is returned separately
/// for placement in `HirMatchArm.guard`.
#[allow(clippy::too_many_arguments)]
pub fn lower_pattern_with_guard(
    pattern: &Pattern,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut Diagnostics,
    table: &TypeTable,
    definitions: &mut DefinitionTable,
    current_fn_ret: Option<TypeId>,
) -> (HirPattern, Option<HirExpr>) {
    match &pattern.kind {
        PatternKind::Guard { pattern: inner, guard } => {
            let hir_pattern = lower_pattern(inner, resolutions, table);
            let guard_expr = lower_expr(guard, table, definitions, resolutions, diagnostics, current_fn_ret);
            (hir_pattern, Some(guard_expr))
        }
        _ => (lower_pattern(pattern, resolutions, table), None),
    }
}

/// Lowers a pattern from the AST to HIR.
#[must_use]
pub fn lower_pattern(
    pattern: &Pattern,
    resolutions: Option<&ResolutionMap>,
    table: &TypeTable,
) -> HirPattern {
    match &pattern.kind {
        PatternKind::Wildcard => HirPattern::Wildcard,

        PatternKind::Ident { name, .. } => {
            let def_id = resolutions
                .and_then(|res| res.get_binding(name.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);
            HirPattern::Binding { def_id }
        }

        PatternKind::Literal(lit) => HirPattern::Literal(lower_pattern_literal(&lit.kind)),

        PatternKind::Variant { path, fields } => {
            let def_id = resolutions
                .and_then(|res| res.get_path(path.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);
            HirPattern::Variant {
                def_id,
                fields: fields.iter().map(|f| lower_pattern(f, resolutions, table)).collect(),
            }
        }

        PatternKind::Struct { path, fields, rest } => {
            let def_id = resolutions
                .and_then(|res| res.get_path(path.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);
            HirPattern::Struct {
                def_id,
                fields: fields
                    .iter()
                    .map(|f| {
                        let pat = f
                            .pattern
                            .as_ref()
                            .map_or_else(
                                || {
                                    // Shorthand: `name` binds to same-named local
                                    let def_id = resolutions
                                        .and_then(|res| res.get_binding(f.name.span))
                                        .unwrap_or(spark_resolve::DefId::DUMMY);
                                    HirPattern::Binding { def_id }
                                },
                                |p| lower_pattern(p, resolutions, table),
                            );
                        (f.name.name, pat)
                    })
                    .collect(),
                rest: *rest,
            }
        }

        PatternKind::Tuple(elems) => {
            HirPattern::Tuple(elems.iter().map(|p| lower_pattern(p, resolutions, table)).collect())
        }

        PatternKind::Or(alts) => {
            HirPattern::Or(alts.iter().map(|p| lower_pattern(p, resolutions, table)).collect())
        }

        // Guard is handled in lower_pattern_with_guard
        PatternKind::Guard { pattern: inner, .. } => lower_pattern(inner, resolutions, table),

        PatternKind::TypeNarrowing { name, ty } => {
            let def_id = resolutions
                .and_then(|res| res.get_binding(name.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);
            let narrowed_ty = table.get_expr_type(ty.id).unwrap_or(spark_types::TypeId::ERROR);
            HirPattern::TypeNarrow { def_id, ty: narrowed_ty }
        }

        // Deferred / error
        PatternKind::List { .. } | PatternKind::Error => {
            HirPattern::Error
        }
    }
}

/// Lowers a literal for use in patterns.
fn lower_pattern_literal(kind: &LiteralKind) -> HirLiteral {
    match kind {
        LiteralKind::Int(v) => HirLiteral::Int(*v),
        LiteralKind::Float(v) => HirLiteral::Float(*v),
        LiteralKind::Bool(v) => HirLiteral::Bool(*v),
        LiteralKind::Char(v) => HirLiteral::Char(*v),
        LiteralKind::Null => HirLiteral::Null,
        LiteralKind::String(s) => {
            if let Some(spark_ast::StringPart::Text(sym)) = s.parts.first() {
                HirLiteral::String(*sym)
            } else {
                HirLiteral::String(spark_intern::sym(""))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_ast::{Ident, Literal, Pattern};
    use spark_intern::sym;
    use spark_span::Span;

    fn empty_table() -> TypeTable {
        TypeTable::new()
    }

    #[test]
    fn lower_wildcard() {
        let table = empty_table();
        let pat = Pattern::new(PatternKind::Wildcard, Span::DUMMY);
        let result = lower_pattern(&pat, None, &table);
        assert!(matches!(result, HirPattern::Wildcard));
    }

    #[test]
    fn lower_ident_pattern() {
        let table = empty_table();
        let pat = Pattern::new(
            PatternKind::Ident {
                name: Ident::dummy(sym("x")),
                ty: None,
            },
            Span::DUMMY,
        );
        let result = lower_pattern(&pat, None, &table);
        // Without resolutions, we get DUMMY DefId
        assert!(matches!(result, HirPattern::Binding { .. }));
    }

    #[test]
    fn lower_literal_pattern() {
        let table = empty_table();
        let pat = Pattern::new(
            PatternKind::Literal(Literal::int(42, Span::DUMMY)),
            Span::DUMMY,
        );
        let result = lower_pattern(&pat, None, &table);
        assert!(matches!(result, HirPattern::Literal(HirLiteral::Int(42))));
    }

    #[test]
    fn lower_or_pattern() {
        let table = empty_table();
        let a = Pattern::new(PatternKind::Wildcard, Span::DUMMY);
        let b = Pattern::new(PatternKind::Wildcard, Span::DUMMY);
        let pat = Pattern::new(PatternKind::Or(vec![a, b]), Span::DUMMY);
        let result = lower_pattern(&pat, None, &table);
        if let HirPattern::Or(alts) = result {
            assert_eq!(alts.len(), 2);
        } else {
            panic!("Expected Or pattern");
        }
    }
}
