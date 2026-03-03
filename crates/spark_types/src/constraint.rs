//! Generic constraint validation.
//!
//! After type argument inference, this module validates that concrete types
//! satisfy the constraints declared on generic parameters (e.g., `T: Comparable`,
//! `T: { id: Int }`).

use rustc_hash::FxHashMap;
use spark_diagnostics::{Diagnostic, Label};
use spark_resolve::{DefId, DefinitionTable};
use spark_span::Span;

use crate::check_expr::is_compatible;
use crate::display::display_type as type_name;
use crate::table::TypeTable;
use crate::ty::{Constraint, Ty, TypeId};

/// Validates that concrete type arguments satisfy generic constraints.
///
/// For each generic param with a constraint, checks that the substituted
/// concrete type satisfies the constraint. Reports diagnostics for failures
/// but does not halt — returns `true` if all constraints are satisfied.
#[allow(clippy::implicit_hasher)]
pub fn validate_constraints(
    generic_params: &[DefId],
    subst: &FxHashMap<DefId, TypeId>,
    table: &mut TypeTable,
    definitions: &DefinitionTable,
    span: Span,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> bool {
    let mut all_satisfied = true;

    for &param_def_id in generic_params {
        // Clone the constraint to avoid borrowing table while we mutate it
        let constraint = table.get_generic_constraint(param_def_id).cloned();
        let Some(constraint) = constraint else {
            continue;
        };

        let Some(&concrete_ty) = subst.get(&param_def_id) else {
            continue;
        };

        // Skip error types to avoid cascading diagnostics
        if concrete_ty.is_error() {
            continue;
        }

        match &constraint {
            Constraint::Interface { interface_def_id, .. } => {
                if !check_interface_constraint(
                    concrete_ty, *interface_def_id,
                    table, definitions, span, diagnostics,
                ) {
                    all_satisfied = false;
                }
            }
            Constraint::Structural { fields } => {
                if !check_structural_constraint(
                    concrete_ty, fields,
                    table, definitions, span, diagnostics,
                ) {
                    all_satisfied = false;
                }
            }
        }
    }

    all_satisfied
}

/// Checks that a concrete type binds the required interface.
fn check_interface_constraint(
    concrete_ty: TypeId,
    interface_def_id: DefId,
    table: &mut TypeTable,
    definitions: &DefinitionTable,
    span: Span,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> bool {
    let concrete_def_id = type_def_id(concrete_ty, table);

    let Some(concrete_did) = concrete_def_id else {
        // Primitive or unresolvable type — emit diagnostic
        emit_interface_error(concrete_ty, interface_def_id, table, definitions, span, diagnostics);
        return false;
    };

    // Check if the concrete type has a binding for this interface
    let has_binding = table
        .get_interface_bindings(concrete_did)
        .is_some_and(|bindings| bindings.contains(&interface_def_id));

    if !has_binding {
        emit_interface_error(concrete_ty, interface_def_id, table, definitions, span, diagnostics);
        return false;
    }

    true
}

/// Emits a diagnostic for an unsatisfied interface constraint.
fn emit_interface_error(
    concrete_ty: TypeId,
    interface_def_id: DefId,
    table: &TypeTable,
    definitions: &DefinitionTable,
    span: Span,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    let concrete_name = type_name(concrete_ty, table, definitions);
    let interface_name = definitions
        .get(interface_def_id)
        .map_or_else(|| "<unknown>".to_string(), |info| info.name.as_str().to_string());

    diagnostics.push(
        Diagnostic::error(format!(
            "type `{concrete_name}` does not satisfy constraint `{interface_name}`"
        ))
        .with_label(Label::primary(
            span,
            format!("`{concrete_name}` does not bind `{interface_name}`"),
        )),
    );
}

/// Checks that a concrete type has all required fields from a structural constraint.
fn check_structural_constraint(
    concrete_ty: TypeId,
    required_fields: &[(spark_intern::Symbol, TypeId)],
    table: &TypeTable,
    definitions: &DefinitionTable,
    span: Span,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> bool {
    let concrete_did = type_def_id(concrete_ty, table);

    let Some(did) = concrete_did else {
        let concrete_name = type_name(concrete_ty, table, definitions);
        diagnostics.push(
            Diagnostic::error(format!(
                "type `{concrete_name}` does not satisfy structural constraint"
            ))
            .with_label(Label::primary(span, "type has no fields")),
        );
        return false;
    };

    let actual_fields = table.get_struct_fields(did);
    let mut all_ok = true;

    for &(req_name, req_type) in required_fields {
        let found = actual_fields.and_then(|fields| {
            fields.iter().find(|(name, _)| *name == req_name)
        });

        if let Some(&(_, actual_type)) = found {
            if !is_compatible(actual_type, req_type, table) {
                let concrete_name = type_name(concrete_ty, table, definitions);
                let expected_name = type_name(req_type, table, definitions);
                let found_name = type_name(actual_type, table, definitions);
                diagnostics.push(
                    Diagnostic::error(format!(
                        "type `{concrete_name}` field `{}` has type `{found_name}`, expected `{expected_name}`",
                        req_name.as_str()
                    ))
                    .with_label(Label::primary(span, format!("field type mismatch for `{}`", req_name.as_str()))),
                );
                all_ok = false;
            }
        } else {
            let concrete_name = type_name(concrete_ty, table, definitions);
            diagnostics.push(
                Diagnostic::error(format!(
                    "type `{concrete_name}` is missing required field `{}`",
                    req_name.as_str()
                ))
                .with_label(Label::primary(
                    span,
                    format!("missing field `{}`", req_name.as_str()),
                )),
            );
            all_ok = false;
        }
    }

    all_ok
}

/// Extracts the `DefId` of a concrete type, if it has one.
///
/// Returns `Some(def_id)` for struct, enum, and applied types.
/// Returns `None` for primitives and other types without a `DefId`.
fn type_def_id(ty: TypeId, table: &TypeTable) -> Option<DefId> {
    match table.resolve(ty) {
        Ty::Struct(id) | Ty::Enum(id) => Some(*id),
        Ty::Applied { base, .. } => Some(*base),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ty::IntWidth;
    use spark_intern::sym;
    use spark_modules::ModulePath;

    /// Helper to create a minimal test setup.
    fn setup() -> (TypeTable, DefinitionTable, spark_diagnostics::Diagnostics) {
        (TypeTable::new(), DefinitionTable::new(), spark_diagnostics::Diagnostics::new())
    }

    #[test]
    fn no_constraint_always_passes() {
        let (mut table, definitions, mut diagnostics) = setup();
        let param = DefId::new(0);
        let concrete = table.intern(Ty::Int(IntWidth::I64));
        let subst: FxHashMap<DefId, TypeId> = [(param, concrete)].into_iter().collect();
        let span = Span::DUMMY;

        let result = validate_constraints(&[param], &subst, &mut table, &definitions, span, &mut diagnostics);
        assert!(result);
        assert!(!diagnostics.has_errors());
    }

    #[test]
    fn error_type_skipped() {
        let (mut table, definitions, mut diagnostics) = setup();
        let param = DefId::new(0);
        let interface = DefId::new(1);

        // Set a constraint but provide Ty::Error as the concrete type
        table.set_generic_constraint(param, Constraint::Interface {
            interface_def_id: interface,
            type_args: vec![],
        });
        let subst: FxHashMap<DefId, TypeId> = [(param, TypeId::ERROR)].into_iter().collect();
        let span = Span::DUMMY;

        let result = validate_constraints(&[param], &subst, &mut table, &definitions, span, &mut diagnostics);
        assert!(result);
        assert!(!diagnostics.has_errors());
    }

    #[test]
    fn interface_constraint_satisfied() {
        let (mut table, mut definitions, mut diagnostics) = setup();

        // Register a struct and an interface
        let struct_def_id = DefId::new(0);
        let interface_def_id = DefId::new(1);
        let param_def_id = DefId::new(2);

        definitions.insert(spark_resolve::DefinitionInfo::new(
            spark_resolve::DefinitionKind::Struct,
            sym("MyStruct"),
            Span::DUMMY,
            spark_resolve::Visibility::Public,
            ModulePath::root(),
        ));
        definitions.insert(spark_resolve::DefinitionInfo::new(
            spark_resolve::DefinitionKind::Interface,
            sym("Comparable"),
            Span::DUMMY,
            spark_resolve::Visibility::Public,
            ModulePath::root(),
        ));

        let struct_ty = table.intern(Ty::Struct(struct_def_id));
        table.add_interface_binding(struct_def_id, interface_def_id);
        table.set_generic_constraint(param_def_id, Constraint::Interface {
            interface_def_id,
            type_args: vec![],
        });

        let subst: FxHashMap<DefId, TypeId> = [(param_def_id, struct_ty)].into_iter().collect();
        let span = Span::DUMMY;

        let result = validate_constraints(&[param_def_id], &subst, &mut table, &definitions, span, &mut diagnostics);
        assert!(result);
        assert!(!diagnostics.has_errors());
    }

    #[test]
    fn interface_constraint_violated() {
        let (mut table, mut definitions, mut diagnostics) = setup();

        let struct_def_id = DefId::new(0);
        let interface_def_id = DefId::new(1);
        let param_def_id = DefId::new(2);

        definitions.insert(spark_resolve::DefinitionInfo::new(
            spark_resolve::DefinitionKind::Struct,
            sym("MyStruct"),
            Span::DUMMY,
            spark_resolve::Visibility::Public,
            ModulePath::root(),
        ));
        definitions.insert(spark_resolve::DefinitionInfo::new(
            spark_resolve::DefinitionKind::Interface,
            sym("Comparable"),
            Span::DUMMY,
            spark_resolve::Visibility::Public,
            ModulePath::root(),
        ));

        let struct_ty = table.intern(Ty::Struct(struct_def_id));
        // NOTE: No binding registered — constraint should fail
        table.set_generic_constraint(param_def_id, Constraint::Interface {
            interface_def_id,
            type_args: vec![],
        });

        let subst: FxHashMap<DefId, TypeId> = [(param_def_id, struct_ty)].into_iter().collect();
        let span = Span::DUMMY;

        let result = validate_constraints(&[param_def_id], &subst, &mut table, &definitions, span, &mut diagnostics);
        assert!(!result);
        assert!(diagnostics.has_errors());
    }

    #[test]
    fn structural_constraint_satisfied() {
        let (mut table, mut definitions, mut diagnostics) = setup();

        let struct_def_id = DefId::new(0);
        let param_def_id = DefId::new(1);

        definitions.insert(spark_resolve::DefinitionInfo::new(
            spark_resolve::DefinitionKind::Struct,
            sym("User"),
            Span::DUMMY,
            spark_resolve::Visibility::Public,
            ModulePath::root(),
        ));

        let int_ty = table.intern(Ty::Int(IntWidth::I64));
        let string_ty = table.intern(Ty::String);
        let struct_ty = table.intern(Ty::Struct(struct_def_id));

        // Struct has id: Int and name: String
        table.set_struct_fields(struct_def_id, vec![
            (sym("id"), int_ty),
            (sym("name"), string_ty),
        ]);

        // Constraint requires id: Int
        table.set_generic_constraint(param_def_id, Constraint::Structural {
            fields: vec![(sym("id"), int_ty)],
        });

        let subst: FxHashMap<DefId, TypeId> = [(param_def_id, struct_ty)].into_iter().collect();
        let span = Span::DUMMY;

        let result = validate_constraints(&[param_def_id], &subst, &mut table, &definitions, span, &mut diagnostics);
        assert!(result);
        assert!(!diagnostics.has_errors());
    }

    #[test]
    fn structural_constraint_missing_field() {
        let (mut table, mut definitions, mut diagnostics) = setup();

        let struct_def_id = DefId::new(0);
        let param_def_id = DefId::new(1);

        definitions.insert(spark_resolve::DefinitionInfo::new(
            spark_resolve::DefinitionKind::Struct,
            sym("Point"),
            Span::DUMMY,
            spark_resolve::Visibility::Public,
            ModulePath::root(),
        ));

        let int_ty = table.intern(Ty::Int(IntWidth::I64));
        let struct_ty = table.intern(Ty::Struct(struct_def_id));

        // Struct has x: Int, y: Int but NOT id
        table.set_struct_fields(struct_def_id, vec![
            (sym("x"), int_ty),
            (sym("y"), int_ty),
        ]);

        // Constraint requires id: Int
        table.set_generic_constraint(param_def_id, Constraint::Structural {
            fields: vec![(sym("id"), int_ty)],
        });

        let subst: FxHashMap<DefId, TypeId> = [(param_def_id, struct_ty)].into_iter().collect();
        let span = Span::DUMMY;

        let result = validate_constraints(&[param_def_id], &subst, &mut table, &definitions, span, &mut diagnostics);
        assert!(!result);
        assert!(diagnostics.has_errors());
    }
}
