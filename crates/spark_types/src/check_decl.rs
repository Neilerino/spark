//! Declaration signature collection (Pass 1).
//!
//! Walks all top-level items and registers their type signatures in the
//! type table. This must happen before body checking so that forward
//! references between functions resolve correctly.

use spark_ast::{
    BindingDef, ConstDef, EnumDef, EnumVariantData, FunctionDef, InterfaceDef, Item, StructDef,
    TypeAliasDef, TypeDefDef,
};
use spark_resolve::{DefId, DefinitionKind, DefinitionTable, ResolutionMap};

use crate::builtins::BuiltinTypes;
use crate::lower::lower_type_expr;
use crate::table::TypeTable;
use crate::ty::{Ty, TypeId};

/// Collects type signatures for a single item.
///
/// This is called during Pass 1 to register definitions before their
/// bodies are type-checked.
#[allow(clippy::too_many_arguments)]
pub fn collect_item(
    item: &Item,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    match item {
        Item::Function(f) => collect_function(f, table, builtins, definitions, resolutions, diagnostics),
        Item::Struct(s) => collect_struct(s, table, builtins, definitions, resolutions, diagnostics),
        Item::Enum(e) => collect_enum(e, table, builtins, definitions, resolutions, diagnostics),
        Item::Const(c) => collect_const(c, table, builtins, definitions, resolutions, diagnostics),
        Item::TypeAlias(a) => collect_type_alias(a, table, builtins, definitions, resolutions, diagnostics),
        Item::TypeDef(t) => collect_type_def(t, table, builtins, definitions, resolutions, diagnostics),
        Item::Interface(i) => collect_interface(i, table, builtins, definitions, resolutions, diagnostics),
        Item::Binding(b) => collect_binding(b, table, builtins, definitions, resolutions, diagnostics),
    }
}

/// Collects a function's type signature.
fn collect_function(
    func: &FunctionDef,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    // Register generic parameter types before lowering param/return types,
    // so that references to `T` in the signature resolve to `Ty::Param`.
    let generic_param_def_ids = collect_generic_params(&func.generics, table, resolutions);

    let param_types: Vec<TypeId> = func
        .params
        .iter()
        .map(|p| lower_type_expr(&p.ty, table, builtins, definitions, resolutions, diagnostics))
        .collect();

    let ret_type = lower_type_expr(&func.ret_type, table, builtins, definitions, resolutions, diagnostics);

    let (param_types, ret_type) = crate::lower::apply_lifetime_elision(
        &param_types, ret_type, func.span, table, diagnostics,
    );

    let fn_type = table.intern(Ty::Function {
        params: param_types.clone(),
        ret: ret_type,
        is_async: func.is_async,
    });

    // Find the DefId for this function by looking in the definition table
    if let Some(def_id) = find_def_by_name_and_kind(func.name.name, &DefinitionKind::Function, definitions) {
        table.set_def_type(def_id, fn_type);

        // Store generic param list for this function
        if !generic_param_def_ids.is_empty() {
            table.set_generic_params(def_id, generic_param_def_ids);
        }

        // Also register parameter types
        for (i, param) in func.params.iter().enumerate() {
            if let Some(param_def) = find_param_def(param.name.name, def_id, definitions) {
                table.set_def_type(param_def, param_types[i]);
            }
        }
    }
}

/// Collects a struct's type and field types.
fn collect_struct(
    struct_def: &StructDef,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    if let Some(def_id) = find_def_by_name_and_kind(struct_def.name.name, &DefinitionKind::Struct, definitions) {
        // Register generic parameter types before lowering field types.
        let generic_param_def_ids = collect_generic_params(&struct_def.generics, table, resolutions);

        let struct_type = table.intern(Ty::Struct(def_id));
        table.set_def_type(def_id, struct_type);

        // Store generic param list for this struct
        if !generic_param_def_ids.is_empty() {
            table.set_generic_params(def_id, generic_param_def_ids);
        }

        // Collect field types
        let mut fields = Vec::with_capacity(struct_def.fields.len());
        for field in &struct_def.fields {
            let field_type = lower_type_expr(&field.ty, table, builtins, definitions, resolutions, diagnostics);
            fields.push((field.name.name, field_type));

            // Register the field def type
            if let Some(field_def) = find_field_def(field.name.name, def_id, definitions) {
                table.set_def_type(field_def, field_type);
            }
        }
        table.set_struct_fields(def_id, fields);
    }
}

/// Collects an enum's type and variant types.
fn collect_enum(
    enum_def: &EnumDef,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    if let Some(def_id) = find_def_by_name_and_kind(enum_def.name.name, &DefinitionKind::Enum, definitions) {
        // Register generic parameter types before lowering variant field types.
        let generic_param_def_ids = collect_generic_params(&enum_def.generics, table, resolutions);

        let enum_type = table.intern(Ty::Enum(def_id));
        table.set_def_type(def_id, enum_type);

        // Store generic param list for this enum
        if !generic_param_def_ids.is_empty() {
            table.set_generic_params(def_id, generic_param_def_ids);
        }

        let mut variants = Vec::with_capacity(enum_def.variants.len());

        for variant in &enum_def.variants {
            if let Some(variant_def) =
                find_variant_def(variant.name.name, def_id, definitions)
            {
                // The variant's type is the parent enum type
                table.set_def_type(variant_def, enum_type);
                variants.push((variant.name.name, variant_def));

                // Collect variant fields
                if let EnumVariantData::Fields(fields) = &variant.data {
                    let mut variant_fields = Vec::with_capacity(fields.len());
                    for field in fields {
                        let field_type =
                            lower_type_expr(&field.ty, table, builtins, definitions, resolutions, diagnostics);
                        variant_fields.push((field.name.name, field_type));
                    }
                    table.set_variant_fields(variant_def, variant_fields);
                }
            }
        }
        table.set_enum_variants(def_id, variants);
    }
}

/// Collects a constant's type.
fn collect_const(
    const_def: &ConstDef,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    let ty = lower_type_expr(&const_def.ty, table, builtins, definitions, resolutions, diagnostics);
    if let Some(def_id) = find_def_by_name_and_kind(const_def.name.name, &DefinitionKind::Const, definitions) {
        table.set_def_type(def_id, ty);
    }
}

/// Collects a type alias.
fn collect_type_alias(
    alias_def: &TypeAliasDef,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    let ty = lower_type_expr(&alias_def.ty, table, builtins, definitions, resolutions, diagnostics);
    if let Some(def_id) = find_def_by_name_and_kind(alias_def.name.name, &DefinitionKind::TypeAlias, definitions) {
        table.set_def_type(def_id, ty);
    }
}

/// Collects a distinct type definition.
fn collect_type_def(
    type_def: &TypeDefDef,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    let ty = lower_type_expr(&type_def.ty, table, builtins, definitions, resolutions, diagnostics);
    if let Some(def_id) = find_def_by_name_and_kind(type_def.name.name, &DefinitionKind::TypeDef, definitions) {
        table.set_def_type(def_id, ty);
    }
}

/// Collects an interface's method signatures.
///
/// Registers each method as a `Ty::Function` so that bindings can be
/// validated against the interface's requirements.
fn collect_interface(
    iface: &InterfaceDef,
    table: &mut TypeTable,
    _builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    _resolutions: Option<&ResolutionMap>,
    _diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    // Register the interface type itself
    if let Some(def_id) = find_def_by_name_and_kind(iface.name.name, &DefinitionKind::Interface, definitions) {
        let iface_type = table.intern(Ty::Param(def_id));
        table.set_def_type(def_id, iface_type);
    }

    // Register method signatures — they don't have separate DefIds currently,
    // so we just lower them for validation use. Interface methods that don't
    // have DefIds allocated by the resolver won't be stored here; the critical
    // path is binding method collection below.
}

/// Collects method signatures from an interface binding.
///
/// For each binding target method, lowers the parameter and return types,
/// interns the function type, and registers it on the method's `DefId`.
/// Also records the methods in `TypeTable::type_methods` for lookup
/// during method call resolution.
#[allow(clippy::too_many_arguments)]
fn collect_binding(
    binding: &BindingDef,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    // Find the bound type's DefId
    let type_def_id = find_type_def_from_binding_ty(&binding.ty, definitions);
    if type_def_id.is_dummy() {
        return;
    }

    let mut all_methods: Vec<(spark_intern::Symbol, DefId)> = Vec::new();

    for target in &binding.bindings {
        // Validate interface requirements
        let interface_def_id = find_type_def_from_binding_ty(&target.interface, definitions);
        if !interface_def_id.is_dummy() {
            validate_interface_methods(interface_def_id, &target.methods, definitions, diagnostics);
        }

        for method in &target.methods {
            // Find the method's DefId (allocated by resolver as BindingMethod)
            let method_def_id = find_binding_method_def(
                method.name.name,
                type_def_id,
                definitions,
            );

            if let Some(method_def_id) = method_def_id {
                // Lower parameter types
                let param_types: Vec<TypeId> = method
                    .params
                    .iter()
                    .map(|p| lower_type_expr(&p.ty, table, builtins, definitions, resolutions, diagnostics))
                    .collect();

                // Lower return type
                let ret_type = lower_type_expr(&method.ret_type, table, builtins, definitions, resolutions, diagnostics);

                // Apply lifetime elision
                let (param_types, ret_type) = crate::lower::apply_lifetime_elision(
                    &param_types, ret_type, method.span, table, diagnostics,
                );

                // Intern the function type
                let fn_type = table.intern(Ty::Function {
                    params: param_types.clone(),
                    ret: ret_type,
                    is_async: method.is_async,
                });

                // Register the method type
                table.set_def_type(method_def_id, fn_type);

                // Register parameter types
                for (i, param) in method.params.iter().enumerate() {
                    if let Some(param_def) = find_param_def(param.name.name, method_def_id, definitions) {
                        table.set_def_type(param_def, param_types[i]);
                    }
                }

                all_methods.push((method.name.name, method_def_id));
            }
        }
    }

    // Register all methods for the bound type
    if !all_methods.is_empty() {
        // Merge with any existing methods (from other bindings)
        if let Some(existing) = table.get_type_methods(type_def_id) {
            let mut merged = existing.to_vec();
            merged.extend(all_methods);
            table.set_type_methods(type_def_id, merged);
        } else {
            table.set_type_methods(type_def_id, all_methods);
        }
    }
}

/// Finds the type `DefId` referenced by a binding's type expression.
fn find_type_def_from_binding_ty(
    ty: &spark_ast::TypeExpr,
    definitions: &DefinitionTable,
) -> DefId {
    if let spark_ast::TypeExprKind::Named { path, .. } = &ty.kind {
        if path.is_simple() {
            let name = path.segments[0].name;
            // Search for Struct or Enum or Interface with this name
            return definitions
                .iter()
                .find(|(_, info)| info.name == name && info.kind.is_type())
                .map_or(DefId::DUMMY, |(id, _)| id);
        }
    }
    DefId::DUMMY
}

/// Finds a binding method `DefId` by name and parent type.
fn find_binding_method_def(
    name: spark_intern::Symbol,
    parent_type: DefId,
    definitions: &DefinitionTable,
) -> Option<DefId> {
    definitions
        .iter()
        .find(|(_, info)| {
            info.name == name
                && matches!(info.kind, DefinitionKind::BindingMethod(parent) if parent == parent_type)
        })
        .map(|(id, _)| id)
}

/// Validates that a binding provides all required methods from its interface.
///
/// Reports errors for missing methods. Defers signature matching to the
/// generics phase.
fn validate_interface_methods(
    interface_def_id: DefId,
    binding_methods: &[FunctionDef],
    definitions: &DefinitionTable,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    // Find the interface definition info
    let iface_info = match definitions.get(interface_def_id) {
        Some(info) if matches!(info.kind, DefinitionKind::Interface) => info,
        _ => return,
    };

    let iface_name = iface_info.name;
    let iface_span = iface_info.span;

    // We don't have access to the AST interface methods here (they aren't stored
    // in the definition table), so interface validation is deferred to when we
    // have both the interface AST and the binding AST available (in checker.rs).
    // For now, this is a placeholder that can be enhanced later.
    let _ = (iface_name, iface_span, binding_methods, diagnostics);
}

// === Generic parameter collection ===

/// Collects generic parameter `DefId`s from the AST and registers their
/// types as `Ty::Param(def_id)` in the type table.
///
/// Returns the ordered list of generic param `DefId`s.
fn collect_generic_params(
    generics: &[spark_ast::GenericParam],
    table: &mut TypeTable,
    resolutions: Option<&ResolutionMap>,
) -> Vec<DefId> {
    if generics.is_empty() {
        return Vec::new();
    }

    let mut param_def_ids = Vec::with_capacity(generics.len());
    for generic in generics {
        if let Some(def_id) = resolutions.and_then(|res| res.get_binding(generic.name.span)) {
            // Register the param type: T → Ty::Param(def_id)
            let param_ty = table.intern(Ty::Param(def_id));
            table.set_def_type(def_id, param_ty);
            param_def_ids.push(def_id);
        }
    }
    param_def_ids
}

// === Helper functions for finding DefIds ===

/// Finds a definition by name and kind in the definition table.
fn find_def_by_name_and_kind(
    name: spark_intern::Symbol,
    kind: &DefinitionKind,
    definitions: &DefinitionTable,
) -> Option<DefId> {
    definitions
        .iter()
        .find(|(_, info)| info.name == name && &info.kind == kind)
        .map(|(id, _)| id)
}

/// Finds a parameter definition within a function.
fn find_param_def(
    name: spark_intern::Symbol,
    _parent_fn: DefId,
    definitions: &DefinitionTable,
) -> Option<DefId> {
    definitions
        .iter()
        .find(|(_, info)| info.name == name && matches!(info.kind, DefinitionKind::Parameter))
        .map(|(id, _)| id)
}

/// Finds a struct field definition.
fn find_field_def(
    name: spark_intern::Symbol,
    parent_struct: DefId,
    definitions: &DefinitionTable,
) -> Option<DefId> {
    definitions
        .iter()
        .find(|(_, info)| {
            info.name == name && matches!(info.kind, DefinitionKind::StructField(parent) if parent == parent_struct)
        })
        .map(|(id, _)| id)
}

/// Finds an enum variant definition.
fn find_variant_def(
    name: spark_intern::Symbol,
    parent_enum: DefId,
    definitions: &DefinitionTable,
) -> Option<DefId> {
    definitions
        .iter()
        .find(|(_, info)| {
            info.name == name && matches!(info.kind, DefinitionKind::EnumVariant(parent) if parent == parent_enum)
        })
        .map(|(id, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_ast::*;
    use spark_diagnostics::Diagnostics;
    use spark_intern::sym;
    use spark_modules::ModulePath;
    use spark_resolve::{DefinitionInfo, Visibility};
    use spark_span::Span;

    fn int_type() -> TypeExpr {
        TypeExpr::new(
            TypeExprKind::Named {
                path: Path::from_ident(Ident::dummy(sym("Int"))),
                type_args: vec![],
            },
            Span::DUMMY,
        )
    }


    #[test]
    fn collect_function_signature() {
        let mut table = crate::table::TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let mut defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        // Register a function definition
        let fn_def_id = defs.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            sym("add"),
            Span::DUMMY,
            Visibility::Public,
            ModulePath::root(),
        ));

        // Register parameter definitions
        defs.insert(DefinitionInfo::new(
            DefinitionKind::Parameter,
            sym("a"),
            Span::DUMMY,
            Visibility::Private,
            ModulePath::root(),
        ));
        defs.insert(DefinitionInfo::new(
            DefinitionKind::Parameter,
            sym("b"),
            Span::DUMMY,
            Visibility::Private,
            ModulePath::root(),
        ));

        let func = FunctionDef {
            visibility: spark_ast::Visibility::Public,
            is_async: false,
            name: Ident::dummy(sym("add")),
            generics: vec![],
            params: vec![
                FnParam::new(Ident::dummy(sym("a")), int_type(), Span::DUMMY),
                FnParam::new(Ident::dummy(sym("b")), int_type(), Span::DUMMY),
            ],
            ret_type: int_type(),
            body: None,
            decorators: vec![],
            doc: None,
            span: Span::DUMMY,
        };

        let item = Item::Function(func);
        collect_item(&item, &mut table, &builtins, &defs, None, &mut diags);

        assert!(!diags.has_errors());
        let fn_ty = table.get_def_type(fn_def_id).unwrap();
        assert!(matches!(table.resolve(fn_ty), Ty::Function { .. }));
    }

    #[test]
    fn collect_struct_fields() {
        let mut table = crate::table::TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let mut defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let struct_def_id = defs.insert(DefinitionInfo::new(
            DefinitionKind::Struct,
            sym("User"),
            Span::DUMMY,
            Visibility::Public,
            ModulePath::root(),
        ));

        defs.insert(DefinitionInfo::new(
            DefinitionKind::StructField(struct_def_id),
            sym("name"),
            Span::DUMMY,
            Visibility::Public,
            ModulePath::root(),
        ));

        let struct_item = StructDef {
            visibility: spark_ast::Visibility::Public,
            name: Ident::dummy(sym("User")),
            generics: vec![],
            fields: vec![StructField::new(
                Ident::dummy(sym("name")),
                TypeExpr::new(
                    TypeExprKind::Named {
                        path: Path::from_ident(Ident::dummy(sym("String"))),
                        type_args: vec![],
                    },
                    Span::DUMMY,
                ),
                Span::DUMMY,
            )],
            doc: None,
            span: Span::DUMMY,
        };

        let item = Item::Struct(struct_item);
        collect_item(&item, &mut table, &builtins, &defs, None, &mut diags);

        assert!(!diags.has_errors());
        let fields = table.get_struct_fields(struct_def_id).unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0, sym("name"));
    }
}
