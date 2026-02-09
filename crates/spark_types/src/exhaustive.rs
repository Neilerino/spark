//! Match exhaustiveness checking.
//!
//! Analyzes match arms to verify that all possible values of the scrutinee
//! type are covered by at least one arm. Reports missing variants/types and
//! warns about unreachable arms.

use rustc_hash::FxHashSet;
use spark_ast::{LiteralKind, MatchArm, PatternKind};
use spark_diagnostics::{Diagnostic, Label};
use spark_resolve::{DefId, DefinitionTable, ResolutionMap};
use spark_span::Span;

use crate::builtins::BuiltinTypes;
use crate::check_expr::type_name;
use crate::lower::lower_type_expr;
use crate::table::TypeTable;
use crate::ty::{Ty, TypeId};

/// Tracks what values a set of patterns covers for a given scrutinee type.
enum Coverage {
    /// A wildcard/binding is present — everything is covered.
    Full,
    /// Specific enum variants are covered (by their `DefId`).
    EnumVariants(FxHashSet<DefId>),
    /// Specific union member types are covered.
    UnionMembers(FxHashSet<TypeId>),
    /// Specific boolean values are covered.
    BoolValues { has_true: bool, has_false: bool },
    /// Only specific literals are covered (never exhaustive alone).
    Literals,
    /// Nothing covered yet.
    Empty,
}

impl Coverage {
    /// Returns true if this coverage is exhaustive for the scrutinee type.
    const fn is_exhaustive(&self) -> bool {
        matches!(self, Self::Full)
    }

    /// Merges another pattern's contribution into the current coverage.
    fn merge(&mut self, other: &Self) {
        match (&*self, other) {
            // Full stays full
            (Self::Full, _) => {}
            // Other is full — become full
            (_, Self::Full) => *self = Self::Full,

            // Merge enum variants
            (Self::EnumVariants(existing), Self::EnumVariants(new)) => {
                let mut merged = existing.clone();
                merged.extend(new);
                *self = Self::EnumVariants(merged);
            }
            (Self::Empty, Self::EnumVariants(new)) => {
                *self = Self::EnumVariants(new.clone());
            }

            // Merge union members
            (Self::UnionMembers(existing), Self::UnionMembers(new)) => {
                let mut merged = existing.clone();
                merged.extend(new);
                *self = Self::UnionMembers(merged);
            }
            (Self::Empty, Self::UnionMembers(new)) => {
                *self = Self::UnionMembers(new.clone());
            }

            // Merge booleans
            (Self::BoolValues { has_true: t1, has_false: f1 },
             Self::BoolValues { has_true: t2, has_false: f2 }) => {
                *self = Self::BoolValues {
                    has_true: *t1 || *t2,
                    has_false: *f1 || *f2,
                };
            }
            (Self::Empty, Self::BoolValues { has_true, has_false }) => {
                *self = Self::BoolValues {
                    has_true: *has_true,
                    has_false: *has_false,
                };
            }

            // Literals never make things exhaustive
            (Self::Literals | Self::Empty, Self::Literals) => {
                *self = Self::Literals;
            }

            // Mixed merges: keep the more specific one or stay as-is
            _ => {}
        }
    }

    /// Returns true if the given pattern is already subsumed by this coverage.
    fn subsumes(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Full, _) => true,
            (Self::EnumVariants(covered), Self::EnumVariants(new)) => {
                new.is_subset(covered)
            }
            (Self::UnionMembers(covered), Self::UnionMembers(new)) => {
                new.is_subset(covered)
            }
            (Self::BoolValues { has_true: t, has_false: f },
             Self::BoolValues { has_true: nt, has_false: nf }) => {
                (!nt || *t) && (!nf || *f)
            }
            _ => false,
        }
    }
}

/// Checks whether a match expression is exhaustive.
///
/// Emits an error diagnostic if not all possible values of the scrutinee
/// type are covered, and a warning for unreachable arms.
#[allow(clippy::too_many_arguments)]
pub fn check_exhaustiveness(
    scrutinee_ty: TypeId,
    arms: &[MatchArm],
    match_span: Span,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    // Don't check error types
    if scrutinee_ty.is_error() {
        return;
    }

    let scrutinee_resolved = table.resolve(scrutinee_ty).clone();

    // Never type is always exhausted (even with zero arms)
    if matches!(scrutinee_resolved, Ty::Never) {
        return;
    }

    let mut coverage = Coverage::Empty;

    for arm in arms {
        let arm_coverage = pattern_coverage(
            &arm.pattern.kind,
            scrutinee_ty,
            &scrutinee_resolved,
            table,
            builtins,
            definitions,
            resolutions,
            diagnostics,
        );

        // Check if this arm is unreachable
        if coverage.subsumes(&arm_coverage) {
            diagnostics.push(
                Diagnostic::warning("unreachable match arm")
                    .with_label(Label::primary(arm.span, "this arm is unreachable")),
            );
        }

        coverage.merge(&arm_coverage);
    }

    // Check exhaustiveness against the scrutinee type
    if coverage.is_exhaustive() {
        return;
    }

    match &scrutinee_resolved {
        Ty::Enum(def_id) => check_enum_exhaustiveness(
            *def_id, &coverage, match_span, table, definitions, diagnostics,
        ),
        Ty::Union(members) => check_union_exhaustiveness(
            members, &coverage, match_span, table, definitions, diagnostics,
        ),
        Ty::Optional(inner) => {
            let null_ty = builtins.null;
            check_optional_exhaustiveness(
                *inner, null_ty, &coverage, match_span, table, definitions, diagnostics,
            );
        }
        Ty::Bool => check_bool_exhaustiveness(
            &coverage, match_span, diagnostics,
        ),
        _ => {
            // Int, String, Float, Char, Struct, Tuple, etc. require wildcard
            diagnostics.push(
                Diagnostic::error("non-exhaustive match")
                    .with_label(Label::primary(match_span, "match is not exhaustive"))
                    .with_note("add a wildcard arm `_ ->` to cover remaining values"),
            );
        }
    }
}

/// Computes the coverage contribution of a single pattern.
#[allow(clippy::too_many_arguments)]
fn pattern_coverage(
    kind: &PatternKind,
    scrutinee_ty: TypeId,
    scrutinee_resolved: &Ty,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> Coverage {
    match kind {
        // Wildcard, plain bindings, and error patterns cover everything
        PatternKind::Wildcard | PatternKind::Ident { ty: None, .. } | PatternKind::Error => Coverage::Full,

        // Typed binding — covers that type in a union
        PatternKind::Ident { ty: Some(ty_expr), .. } => {
            let lowered = lower_type_expr(ty_expr, table, builtins, definitions, resolutions, diagnostics);
            union_member_coverage(lowered, scrutinee_ty, scrutinee_resolved, table)
        }

        // Type narrowing — covers that type in a union
        PatternKind::TypeNarrowing { ty, .. } => {
            let lowered = lower_type_expr(ty, table, builtins, definitions, resolutions, diagnostics);
            union_member_coverage(lowered, scrutinee_ty, scrutinee_resolved, table)
        }

        // Literal patterns
        PatternKind::Literal(lit) => {
            match &lit.kind {
                LiteralKind::Bool(v) => Coverage::BoolValues {
                    has_true: *v,
                    has_false: !*v,
                },
                LiteralKind::Null => {
                    // Covers Null in Optional or union
                    match scrutinee_resolved {
                        Ty::Optional(_) | Ty::Union(_) => {
                            let null_ty = table.intern(Ty::Null);
                            let mut set = FxHashSet::default();
                            set.insert(null_ty);
                            Coverage::UnionMembers(set)
                        }
                        _ => Coverage::Literals,
                    }
                }
                _ => Coverage::Literals,
            }
        }

        // Variant pattern — covers that variant
        PatternKind::Variant { path, .. } => {
            if let Some(res) = resolutions {
                if let Some(def_id) = res.get_path(path.span) {
                    let mut set = FxHashSet::default();
                    set.insert(def_id);
                    return Coverage::EnumVariants(set);
                }
            }
            Coverage::Literals // can't resolve — treat as non-exhaustive
        }

        // Or pattern — union of sub-patterns
        PatternKind::Or(alts) => {
            let mut combined = Coverage::Empty;
            for alt in alts {
                let sub = pattern_coverage(
                    &alt.kind, scrutinee_ty, scrutinee_resolved,
                    table, builtins, definitions, resolutions, diagnostics,
                );
                combined.merge(&sub);
            }
            combined
        }

        // Guard patterns do NOT count toward exhaustiveness
        PatternKind::Guard { .. } => Coverage::Empty,

        // Struct / Tuple / List patterns — treat as one value
        PatternKind::Struct { .. } | PatternKind::Tuple(_) | PatternKind::List { .. } => {
            Coverage::Literals
        }
    }
}

/// Computes coverage for a type-narrowing pattern against a union/optional scrutinee.
fn union_member_coverage(
    member_ty: TypeId,
    scrutinee_ty: TypeId,
    scrutinee_resolved: &Ty,
    table: &TypeTable,
) -> Coverage {
    match scrutinee_resolved {
        Ty::Union(members) => {
            if members.contains(&member_ty) {
                let mut set = FxHashSet::default();
                set.insert(member_ty);
                Coverage::UnionMembers(set)
            } else {
                Coverage::Literals
            }
        }
        Ty::Optional(inner) => {
            let null_ty = table.resolve(member_ty);
            if matches!(null_ty, Ty::Null) || member_ty == *inner {
                let mut set = FxHashSet::default();
                set.insert(member_ty);
                Coverage::UnionMembers(set)
            } else {
                Coverage::Literals
            }
        }
        _ => {
            // Not a union/optional — typed pattern on non-union is like wildcard if types match
            if member_ty == scrutinee_ty {
                Coverage::Full
            } else {
                Coverage::Literals
            }
        }
    }
}

/// Checks enum exhaustiveness and reports missing variants.
fn check_enum_exhaustiveness(
    enum_def_id: DefId,
    coverage: &Coverage,
    match_span: Span,
    table: &TypeTable,
    _definitions: &DefinitionTable,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    let Some(variants) = table.get_enum_variants(enum_def_id) else {
        return;
    };

    let Coverage::EnumVariants(covered) = coverage else {
        let names: Vec<_> = variants.iter()
            .map(|(name, _)| name.as_str().to_string())
            .collect();
        diagnostics.push(
            Diagnostic::error("non-exhaustive match on enum")
                .with_label(Label::primary(match_span, "match is not exhaustive"))
                .with_note(format!("missing variants: {}", names.join(", "))),
        );
        return;
    };

    let missing: Vec<_> = variants.iter()
        .filter(|(_, vid)| !covered.contains(vid))
        .map(|(name, _)| name.as_str().to_string())
        .collect();

    if !missing.is_empty() {
        diagnostics.push(
            Diagnostic::error("non-exhaustive match on enum")
                .with_label(Label::primary(match_span, "match is not exhaustive"))
                .with_note(format!("missing variants: {}", missing.join(", "))),
        );
    }
}

/// Checks union exhaustiveness and reports missing member types.
fn check_union_exhaustiveness(
    members: &[TypeId],
    coverage: &Coverage,
    match_span: Span,
    table: &TypeTable,
    definitions: &DefinitionTable,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    let Coverage::UnionMembers(covered) = coverage else {
        let names: Vec<_> = members.iter()
            .map(|m| type_name(*m, table, definitions))
            .collect();
        diagnostics.push(
            Diagnostic::error("non-exhaustive match on union")
                .with_label(Label::primary(match_span, "match is not exhaustive"))
                .with_note(format!("missing types: {}", names.join(", "))),
        );
        return;
    };

    let missing: Vec<_> = members.iter()
        .filter(|m| !covered.contains(m))
        .map(|m| type_name(*m, table, definitions))
        .collect();

    if !missing.is_empty() {
        diagnostics.push(
            Diagnostic::error("non-exhaustive match on union")
                .with_label(Label::primary(match_span, "match is not exhaustive"))
                .with_note(format!("missing types: {}", missing.join(", "))),
        );
    }
}

/// Checks optional exhaustiveness (both value and null must be covered).
fn check_optional_exhaustiveness(
    inner_ty: TypeId,
    null_ty: TypeId,
    coverage: &Coverage,
    match_span: Span,
    table: &TypeTable,
    definitions: &DefinitionTable,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    let Coverage::UnionMembers(covered) = coverage else {
        diagnostics.push(
            Diagnostic::error("non-exhaustive match on optional")
                .with_label(Label::primary(match_span, "match is not exhaustive"))
                .with_note(format!(
                    "missing: {}, Null",
                    type_name(inner_ty, table, definitions)
                )),
        );
        return;
    };

    let has_value = covered.contains(&inner_ty);
    let has_null = covered.contains(&null_ty);

    let mut missing = Vec::new();
    if !has_value {
        missing.push(type_name(inner_ty, table, definitions));
    }
    if !has_null {
        missing.push("Null".to_string());
    }

    if !missing.is_empty() {
        diagnostics.push(
            Diagnostic::error("non-exhaustive match on optional")
                .with_label(Label::primary(match_span, "match is not exhaustive"))
                .with_note(format!("missing: {}", missing.join(", "))),
        );
    }
}

/// Checks boolean exhaustiveness (both `true` and `false` must be covered).
fn check_bool_exhaustiveness(
    coverage: &Coverage,
    match_span: Span,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    if let Coverage::BoolValues { has_true, has_false } = coverage {
        let mut missing = Vec::new();
        if !has_true { missing.push("true"); }
        if !has_false { missing.push("false"); }

        if !missing.is_empty() {
            diagnostics.push(
                Diagnostic::error("non-exhaustive match on Bool")
                    .with_label(Label::primary(match_span, "match is not exhaustive"))
                    .with_note(format!("missing: {}", missing.join(", "))),
            );
        }
    } else {
        diagnostics.push(
            Diagnostic::error("non-exhaustive match on Bool")
                .with_label(Label::primary(match_span, "match is not exhaustive"))
                .with_note("missing: true, false"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::register_builtins;
    use spark_ast::{Expr, ExprKind, Ident, Literal, Pattern};
    use spark_diagnostics::Diagnostics;
    use spark_intern::sym;
    use spark_span::Span;

    fn make_arm(pattern: Pattern) -> MatchArm {
        MatchArm::new(
            pattern,
            Expr::new(ExprKind::Error, Span::DUMMY),
            Span::DUMMY,
        )
    }

    #[test]
    fn wildcard_is_exhaustive() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let arms = vec![make_arm(Pattern::wildcard(Span::DUMMY))];
        check_exhaustiveness(
            builtins.int, &arms, Span::DUMMY,
            &mut table, &builtins, &defs, None, &mut diags,
        );
        assert!(!diags.has_errors(), "Wildcard should be exhaustive: {:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn binding_is_exhaustive() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let arms = vec![make_arm(Pattern::ident(Ident::dummy(sym("x"))))];
        check_exhaustiveness(
            builtins.int, &arms, Span::DUMMY,
            &mut table, &builtins, &defs, None, &mut diags,
        );
        assert!(!diags.has_errors());
    }

    #[test]
    fn int_without_wildcard_is_error() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let arms = vec![make_arm(Pattern::new(
            PatternKind::Literal(Literal::int(42, Span::DUMMY)),
            Span::DUMMY,
        ))];
        check_exhaustiveness(
            builtins.int, &arms, Span::DUMMY,
            &mut table, &builtins, &defs, None, &mut diags,
        );
        assert!(diags.has_errors());
    }

    #[test]
    fn bool_both_values_exhaustive() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let arms = vec![
            make_arm(Pattern::new(
                PatternKind::Literal(Literal::bool(true, Span::DUMMY)),
                Span::DUMMY,
            )),
            make_arm(Pattern::new(
                PatternKind::Literal(Literal::bool(false, Span::DUMMY)),
                Span::DUMMY,
            )),
        ];
        check_exhaustiveness(
            builtins.bool_ty, &arms, Span::DUMMY,
            &mut table, &builtins, &defs, None, &mut diags,
        );
        assert!(!diags.has_errors(), "Both bools should be exhaustive: {:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn bool_missing_one_is_error() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let arms = vec![make_arm(Pattern::new(
            PatternKind::Literal(Literal::bool(true, Span::DUMMY)),
            Span::DUMMY,
        ))];
        check_exhaustiveness(
            builtins.bool_ty, &arms, Span::DUMMY,
            &mut table, &builtins, &defs, None, &mut diags,
        );
        assert!(diags.has_errors());
    }

    #[test]
    fn unreachable_after_wildcard() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let arms = vec![
            make_arm(Pattern::wildcard(Span::DUMMY)),
            make_arm(Pattern::new(
                PatternKind::Literal(Literal::int(42, Span::DUMMY)),
                Span::DUMMY,
            )),
        ];
        check_exhaustiveness(
            builtins.int, &arms, Span::DUMMY,
            &mut table, &builtins, &defs, None, &mut diags,
        );
        // Should warn about unreachable arm but no errors
        assert!(!diags.has_errors());
        let warnings: Vec<_> = diags.iter()
            .filter(|d| d.message.contains("unreachable"))
            .collect();
        assert!(!warnings.is_empty(), "Should have unreachable warning");
    }

    #[test]
    fn empty_match_on_int_is_error() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let arms = vec![];
        check_exhaustiveness(
            builtins.int, &arms, Span::DUMMY,
            &mut table, &builtins, &defs, None, &mut diags,
        );
        assert!(diags.has_errors());
    }

    #[test]
    fn error_type_no_check() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let arms = vec![];
        check_exhaustiveness(
            TypeId::ERROR, &arms, Span::DUMMY,
            &mut table, &builtins, &defs, None, &mut diags,
        );
        assert!(!diags.has_errors());
    }

    #[test]
    fn error_pattern_no_cascade() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let arms = vec![make_arm(Pattern::new(PatternKind::Error, Span::DUMMY))];
        check_exhaustiveness(
            builtins.int, &arms, Span::DUMMY,
            &mut table, &builtins, &defs, None, &mut diags,
        );
        assert!(!diags.has_errors());
    }

    #[test]
    fn guard_does_not_count() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let arms = vec![make_arm(Pattern::new(
            PatternKind::Guard {
                pattern: Box::new(Pattern::wildcard(Span::DUMMY)),
                guard: Box::new(Expr::new(ExprKind::Error, Span::DUMMY)),
            },
            Span::DUMMY,
        ))];
        check_exhaustiveness(
            builtins.int, &arms, Span::DUMMY,
            &mut table, &builtins, &defs, None, &mut diags,
        );
        assert!(diags.has_errors(), "Guard should not count toward exhaustiveness");
    }

    #[test]
    fn never_type_always_exhausted() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let arms = vec![];
        check_exhaustiveness(
            builtins.never, &arms, Span::DUMMY,
            &mut table, &builtins, &defs, None, &mut diags,
        );
        assert!(!diags.has_errors());
    }

    #[test]
    fn or_pattern_covers_both_bools() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        // `true | false` in a single or-pattern arm should be exhaustive for Bool
        let arms = vec![make_arm(Pattern::new(
            PatternKind::Or(vec![
                Pattern::new(
                    PatternKind::Literal(Literal::bool(true, Span::DUMMY)),
                    Span::DUMMY,
                ),
                Pattern::new(
                    PatternKind::Literal(Literal::bool(false, Span::DUMMY)),
                    Span::DUMMY,
                ),
            ]),
            Span::DUMMY,
        ))];
        check_exhaustiveness(
            builtins.bool_ty, &arms, Span::DUMMY,
            &mut table, &builtins, &defs, None, &mut diags,
        );
        assert!(!diags.has_errors(), "Or-pattern `true | false` should be exhaustive: {:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn optional_value_and_null_exhaustive() {
        use spark_ast::TypeExpr;
        use spark_ast::TypeExprKind;
        use spark_span::FileId;

        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let optional_ty = table.intern(Ty::Optional(builtins.int));

        // Arm 1: typed binding `n: Int` — use a unique span so lower_type_expr
        // can look up the type. We pre-record it in the table.
        let int_span = Span::new(FileId::new(99), 0, 3);
        table.set_expr_type(int_span, builtins.int);
        let int_type_expr = TypeExpr::new(
            TypeExprKind::Named {
                path: spark_ast::Path::from_ident(Ident::new(sym("Int"), int_span)),
                type_args: vec![],
            },
            int_span,
        );
        let value_arm = make_arm(Pattern::ident_typed(
            Ident::dummy(sym("n")),
            int_type_expr,
            Span::DUMMY,
        ));

        // Arm 2: null literal
        let null_arm = make_arm(Pattern::new(
            PatternKind::Literal(Literal::null(Span::DUMMY)),
            Span::DUMMY,
        ));

        let arms = vec![value_arm, null_arm];
        check_exhaustiveness(
            optional_ty, &arms, Span::DUMMY,
            &mut table, &builtins, &defs, None, &mut diags,
        );
        assert!(!diags.has_errors(), "Optional with value + Null should be exhaustive: {:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn union_all_members_exhaustive() {
        use spark_ast::TypeExpr;
        use spark_ast::TypeExprKind;
        use spark_span::FileId;

        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let union_ty = table.intern(Ty::Union(vec![builtins.int, builtins.string]));

        // Arm 1: typed binding `n: Int`
        let int_span = Span::new(FileId::new(99), 0, 3);
        table.set_expr_type(int_span, builtins.int);
        let int_type_expr = TypeExpr::new(
            TypeExprKind::Named {
                path: spark_ast::Path::from_ident(Ident::new(sym("Int"), int_span)),
                type_args: vec![],
            },
            int_span,
        );
        let int_arm = make_arm(Pattern::ident_typed(
            Ident::dummy(sym("n")),
            int_type_expr,
            Span::DUMMY,
        ));

        // Arm 2: typed binding `s: String`
        let str_span = Span::new(FileId::new(99), 10, 16);
        table.set_expr_type(str_span, builtins.string);
        let str_type_expr = TypeExpr::new(
            TypeExprKind::Named {
                path: spark_ast::Path::from_ident(Ident::new(sym("String"), str_span)),
                type_args: vec![],
            },
            str_span,
        );
        let str_arm = make_arm(Pattern::ident_typed(
            Ident::dummy(sym("s")),
            str_type_expr,
            Span::DUMMY,
        ));

        let arms = vec![int_arm, str_arm];
        check_exhaustiveness(
            union_ty, &arms, Span::DUMMY,
            &mut table, &builtins, &defs, None, &mut diags,
        );
        assert!(!diags.has_errors(), "Union with all members covered should be exhaustive: {:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn enum_all_variants_exhaustive() {
        use spark_resolve::ResolutionMap;
        use spark_span::FileId;

        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        // Create an enum with two variants
        let enum_def = DefId::new(100);
        let variant_a = DefId::new(101);
        let variant_b = DefId::new(102);
        let enum_ty = table.intern(Ty::Enum(enum_def));
        table.set_enum_variants(enum_def, vec![
            (sym("A"), variant_a),
            (sym("B"), variant_b),
        ]);

        // Create a ResolutionMap that maps each variant pattern's span to its DefId
        let mut res = ResolutionMap::new();
        let span_a = Span::new(FileId::new(99), 0, 5);
        let span_b = Span::new(FileId::new(99), 10, 15);
        res.record_path(span_a, variant_a);
        res.record_path(span_b, variant_b);

        // Arm 1: Enum.A
        let arm_a = make_arm(Pattern::new(
            PatternKind::Variant {
                path: spark_ast::Path::new(vec![
                    Ident::new(sym("E"), span_a),
                    Ident::new(sym("A"), span_a),
                ], span_a),
                fields: vec![],
            },
            Span::DUMMY,
        ));

        // Arm 2: Enum.B
        let arm_b = make_arm(Pattern::new(
            PatternKind::Variant {
                path: spark_ast::Path::new(vec![
                    Ident::new(sym("E"), span_b),
                    Ident::new(sym("B"), span_b),
                ], span_b),
                fields: vec![],
            },
            Span::DUMMY,
        ));

        let arms = vec![arm_a, arm_b];
        check_exhaustiveness(
            enum_ty, &arms, Span::DUMMY,
            &mut table, &builtins, &defs, Some(&res), &mut diags,
        );
        assert!(!diags.has_errors(), "Enum with all variants should be exhaustive: {:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn enum_missing_variant_error() {
        use spark_resolve::ResolutionMap;
        use spark_span::FileId;

        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        // Create an enum with three variants
        let enum_def = DefId::new(200);
        let variant_a = DefId::new(201);
        let variant_b = DefId::new(202);
        let variant_c = DefId::new(203);
        let enum_ty = table.intern(Ty::Enum(enum_def));
        table.set_enum_variants(enum_def, vec![
            (sym("A"), variant_a),
            (sym("B"), variant_b),
            (sym("C"), variant_c),
        ]);

        // Only cover variant A
        let mut res = ResolutionMap::new();
        let span_a = Span::new(FileId::new(99), 0, 5);
        res.record_path(span_a, variant_a);

        let arm_a = make_arm(Pattern::new(
            PatternKind::Variant {
                path: spark_ast::Path::new(vec![
                    Ident::new(sym("E"), span_a),
                    Ident::new(sym("A"), span_a),
                ], span_a),
                fields: vec![],
            },
            Span::DUMMY,
        ));

        let arms = vec![arm_a];
        check_exhaustiveness(
            enum_ty, &arms, Span::DUMMY,
            &mut table, &builtins, &defs, Some(&res), &mut diags,
        );
        assert!(diags.has_errors());
        let msgs: Vec<_> = diags.iter().map(|d| &d.message).collect();
        assert!(
            msgs.iter().any(|m| m.contains("non-exhaustive")),
            "Expected non-exhaustive error, got: {msgs:?}"
        );
    }
}
