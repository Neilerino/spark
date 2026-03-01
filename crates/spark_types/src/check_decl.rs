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

use crate::ctx::TypeCheckCtx;
use crate::lower::lower_type_expr;
use crate::ty::{Constraint, Ty, TypeId};

/// Collects type signatures for a single item.
///
/// This is called during Pass 1 to register definitions before their
/// bodies are type-checked.
pub fn collect_item(
    item: &Item,
    ctx: &mut TypeCheckCtx<'_>,
) {
    match item {
        Item::Function(f) => collect_function(f, ctx),
        Item::Struct(s) => collect_struct(s, ctx),
        Item::Enum(e) => collect_enum(e, ctx),
        Item::Const(c) => collect_const(c, ctx),
        Item::TypeAlias(a) => collect_type_alias(a, ctx),
        Item::TypeDef(t) => collect_type_def(t, ctx),
        Item::Interface(i) => collect_interface(i, ctx),
        Item::Binding(b) => collect_binding(b, ctx),
    }
}

/// Collects a function's type signature.
fn collect_function(
    func: &FunctionDef,
    ctx: &mut TypeCheckCtx<'_>,
) {
    // Register generic parameter types before lowering param/return types,
    // so that references to `T` in the signature resolve to `Ty::Param`.
    let generic_param_def_ids = collect_generic_params(&func.generics, ctx);

    let param_types: Vec<TypeId> = func
        .params
        .iter()
        .map(|p| lower_type_expr(&p.ty, ctx))
        .collect();

    let ret_type = lower_type_expr(&func.ret_type, ctx);

    let (param_types, ret_type) = crate::lower::apply_lifetime_elision(
        &param_types, ret_type, func.span, ctx.table, ctx.diagnostics,
    );

    let fn_type = ctx.table.intern(Ty::Function {
        params: param_types.clone(),
        ret: ret_type,
        is_async: func.is_async,
    });

    if let Some(def_id) = find_item_def(func.name.span, ctx.resolutions) {
        ctx.table.set_def_type(def_id, fn_type);

        // Store generic param list for this function
        if !generic_param_def_ids.is_empty() {
            ctx.table.set_generic_params(def_id, generic_param_def_ids);
        }

        // Also register parameter types
        for (i, param) in func.params.iter().enumerate() {
            if let Some(param_def) = find_param_def(param.name.span, ctx.resolutions) {
                ctx.table.set_def_type(param_def, param_types[i]);
            }
        }
    }
}

/// Collects a struct's type and field types.
fn collect_struct(
    struct_def: &StructDef,
    ctx: &mut TypeCheckCtx<'_>,
) {
    if let Some(def_id) = find_item_def(struct_def.name.span, ctx.resolutions) {
        // Register generic parameter types before lowering field types.
        let generic_param_def_ids = collect_generic_params(&struct_def.generics, ctx);

        let struct_type = ctx.table.intern(Ty::Struct(def_id));
        ctx.table.set_def_type(def_id, struct_type);

        // Store generic param list for this struct
        if !generic_param_def_ids.is_empty() {
            ctx.table.set_generic_params(def_id, generic_param_def_ids);
        }

        // Collect field types
        let mut fields = Vec::with_capacity(struct_def.fields.len());
        for field in &struct_def.fields {
            let field_type = lower_type_expr(&field.ty, ctx);
            fields.push((field.name.name, field_type));

            // Register the field def type
            if let Some(field_def) = find_field_def(field.name.name, def_id, ctx.definitions) {
                ctx.table.set_def_type(field_def, field_type);
            }
        }
        ctx.table.set_struct_fields(def_id, fields);
    }
}

/// Collects an enum's type and variant types.
fn collect_enum(
    enum_def: &EnumDef,
    ctx: &mut TypeCheckCtx<'_>,
) {
    if let Some(def_id) = find_item_def(enum_def.name.span, ctx.resolutions) {
        // Register generic parameter types before lowering variant field types.
        let generic_param_def_ids = collect_generic_params(&enum_def.generics, ctx);

        let enum_type = ctx.table.intern(Ty::Enum(def_id));
        ctx.table.set_def_type(def_id, enum_type);

        // Store generic param list for this enum
        if !generic_param_def_ids.is_empty() {
            ctx.table.set_generic_params(def_id, generic_param_def_ids);
        }

        let mut variants = Vec::with_capacity(enum_def.variants.len());

        for variant in &enum_def.variants {
            if let Some(variant_def) =
                find_variant_def(variant.name.name, def_id, ctx.definitions)
            {
                // The variant's type is the parent enum type
                ctx.table.set_def_type(variant_def, enum_type);
                variants.push((variant.name.name, variant_def));

                // Collect variant fields
                if let EnumVariantData::Fields(fields) = &variant.data {
                    let mut variant_fields = Vec::with_capacity(fields.len());
                    for field in fields {
                        let field_type = lower_type_expr(&field.ty, ctx);
                        variant_fields.push((field.name.name, field_type));
                    }
                    ctx.table.set_variant_fields(variant_def, variant_fields);
                }
            }
        }
        ctx.table.set_enum_variants(def_id, variants);
    }
}

/// Collects a constant's type.
fn collect_const(
    const_def: &ConstDef,
    ctx: &mut TypeCheckCtx<'_>,
) {
    let ty = lower_type_expr(&const_def.ty, ctx);
    if let Some(def_id) = find_item_def(const_def.name.span, ctx.resolutions) {
        ctx.table.set_def_type(def_id, ty);
    }
}

/// Collects a type alias.
fn collect_type_alias(
    alias_def: &TypeAliasDef,
    ctx: &mut TypeCheckCtx<'_>,
) {
    let ty = lower_type_expr(&alias_def.ty, ctx);
    if let Some(def_id) = find_item_def(alias_def.name.span, ctx.resolutions) {
        ctx.table.set_def_type(def_id, ty);
    }
}

/// Collects a distinct type definition.
fn collect_type_def(
    type_def: &TypeDefDef,
    ctx: &mut TypeCheckCtx<'_>,
) {
    let ty = lower_type_expr(&type_def.ty, ctx);
    if let Some(def_id) = find_item_def(type_def.name.span, ctx.resolutions) {
        ctx.table.set_def_type(def_id, ty);
    }
}

/// Collects an interface's method signatures.
///
/// Registers each method as a `Ty::Function` so that bindings can be
/// validated against the interface's requirements.
fn collect_interface(
    iface: &InterfaceDef,
    ctx: &mut TypeCheckCtx<'_>,
) {
    // Register the interface type itself
    if let Some(def_id) = find_item_def(iface.name.span, ctx.resolutions) {
        let iface_type = ctx.table.intern(Ty::Param(def_id));
        ctx.table.set_def_type(def_id, iface_type);
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
fn collect_binding(
    binding: &BindingDef,
    ctx: &mut TypeCheckCtx<'_>,
) {
    // Find the bound type's DefId
    let type_def_id = find_type_def_from_binding_ty(&binding.ty, ctx.resolutions);
    if type_def_id.is_dummy() {
        return;
    }

    let mut all_methods: Vec<(spark_intern::Symbol, DefId)> = Vec::new();

    for target in &binding.bindings {
        // Validate interface requirements
        let interface_def_id = find_type_def_from_binding_ty(&target.interface, ctx.resolutions);
        if !interface_def_id.is_dummy() {
            ctx.table.add_interface_binding(type_def_id, interface_def_id);
            validate_interface_methods(interface_def_id, &target.methods, ctx.definitions, ctx.diagnostics);
        }

        for method in &target.methods {
            // Find the method's DefId (allocated by resolver as BindingMethod)
            let method_def_id = find_binding_method_def(
                method.name.name,
                type_def_id,
                ctx.definitions,
            );

            if let Some(method_def_id) = method_def_id {
                // Lower parameter types
                let param_types: Vec<TypeId> = method
                    .params
                    .iter()
                    .map(|p| lower_type_expr(&p.ty, ctx))
                    .collect();

                // Lower return type
                let ret_type = lower_type_expr(&method.ret_type, ctx);

                // Apply lifetime elision
                let (param_types, ret_type) = crate::lower::apply_lifetime_elision(
                    &param_types, ret_type, method.span, ctx.table, ctx.diagnostics,
                );

                // Intern the function type
                let fn_type = ctx.table.intern(Ty::Function {
                    params: param_types.clone(),
                    ret: ret_type,
                    is_async: method.is_async,
                });

                // Register the method type
                ctx.table.set_def_type(method_def_id, fn_type);

                // Register parameter types
                for (i, param) in method.params.iter().enumerate() {
                    if let Some(param_def) = find_param_def(param.name.span, ctx.resolutions) {
                        ctx.table.set_def_type(param_def, param_types[i]);
                    }
                }

                all_methods.push((method.name.name, method_def_id));
            }
        }
    }

    // Register all methods for the bound type
    if !all_methods.is_empty() {
        // Merge with any existing methods (from other bindings)
        if let Some(existing) = ctx.table.get_type_methods(type_def_id) {
            let mut merged = existing.to_vec();
            merged.extend(all_methods);
            ctx.table.set_type_methods(type_def_id, merged);
        } else {
            ctx.table.set_type_methods(type_def_id, all_methods);
        }
    }
}

/// Finds the type `DefId` referenced by a binding's type expression.
fn find_type_def_from_binding_ty(
    ty: &spark_ast::TypeExpr,
    resolutions: Option<&ResolutionMap>,
) -> DefId {
    if let spark_ast::TypeExprKind::Named { path, .. } = &ty.kind {
        if let Some(def_id) = resolutions.and_then(|r| r.get_path(path.span)) {
            return def_id;
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

/// Collects generic parameter `DefId`s from the AST, registers their
/// types as `Ty::Param(def_id)` in the type table, and lowers any
/// constraints into the type table's constraint map.
///
/// Returns the ordered list of generic param `DefId`s.
fn collect_generic_params(
    generics: &[spark_ast::GenericParam],
    ctx: &mut TypeCheckCtx<'_>,
) -> Vec<DefId> {
    if generics.is_empty() {
        return Vec::new();
    }

    let mut param_def_ids = Vec::with_capacity(generics.len());
    for generic in generics {
        if let Some(def_id) = ctx.resolutions.and_then(|res| res.get_binding(generic.name.span)) {
            // Register the param type: T -> Ty::Param(def_id)
            let param_ty = ctx.table.intern(Ty::Param(def_id));
            ctx.table.set_def_type(def_id, param_ty);
            param_def_ids.push(def_id);

            // Lower and store constraint, if present
            if let Some(constraint) = &generic.constraint {
                if let Some(lowered) = lower_generic_constraint(constraint, ctx) {
                    ctx.table.set_generic_constraint(def_id, lowered);
                }
            }
        }
    }
    param_def_ids
}

/// Lowers an AST generic constraint into a type-level [`Constraint`].
///
/// For named constraints, resolves the type name to determine whether it is
/// an interface (producing `Constraint::Interface`) or a struct (producing
/// `Constraint::Structural` from that struct's fields).
fn lower_generic_constraint(
    constraint: &spark_ast::GenericConstraint,
    ctx: &mut TypeCheckCtx<'_>,
) -> Option<Constraint> {
    match constraint {
        spark_ast::GenericConstraint::Named(type_expr) => {
            // Resolve the constraint type name to a DefId
            if let spark_ast::TypeExprKind::Named { path, type_args } = &type_expr.kind {
                let constraint_def_id = ctx.resolutions
                    .and_then(|res| res.get_path(path.span));

                if let Some(cdef_id) = constraint_def_id {
                    let info = ctx.definitions.get(cdef_id)?;
                    match &info.kind {
                        DefinitionKind::Interface => {
                            // Lower type arguments (e.g., [T] in Comparable[T])
                            let args: Vec<TypeId> = type_args
                                .iter()
                                .map(|t| lower_type_expr(t, ctx))
                                .collect();
                            Some(Constraint::Interface {
                                interface_def_id: cdef_id,
                                type_args: args,
                            })
                        }
                        DefinitionKind::Struct => {
                            // Named type constraint: T must have all fields of the struct
                            ctx.table.get_struct_fields(cdef_id).map(|fields| Constraint::Structural {
                                fields: fields.to_vec(),
                            })
                        }
                        _ => None,
                    }
                } else {
                    None
                }
            } else {
                None
            }
        }
        spark_ast::GenericConstraint::Structural(fields) => {
            let lowered_fields: Vec<(spark_intern::Symbol, TypeId)> = fields
                .iter()
                .map(|f| {
                    let ty = lower_type_expr(&f.ty, ctx);
                    (f.name.name, ty)
                })
                .collect();
            Some(Constraint::Structural { fields: lowered_fields })
        }
    }
}

// === Helper functions for finding DefIds ===

/// Finds the `DefId` for a top-level item using the resolver's binding map.
fn find_item_def(
    name_span: spark_span::Span,
    resolutions: Option<&ResolutionMap>,
) -> Option<DefId> {
    resolutions.and_then(|r| r.get_binding(name_span))
}

/// Finds a parameter definition using the resolver's binding map.
fn find_param_def(
    name_span: spark_span::Span,
    resolutions: Option<&ResolutionMap>,
) -> Option<DefId> {
    resolutions.and_then(|r| r.get_binding(name_span))
}

/// Finds a struct field definition.
pub(crate) fn find_field_def(
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
    use spark_span::{FileId, Span};

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

        let file = FileId::new(0);
        let fn_span = Span::new(file, 0, 3);
        let param_a_span = Span::new(file, 4, 5);
        let param_b_span = Span::new(file, 7, 8);

        // Register definitions with unique spans
        let fn_def_id = defs.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            sym("add"),
            fn_span,
            Visibility::Public,
            ModulePath::root(),
        ));

        let param_a_def_id = defs.insert(DefinitionInfo::new(
            DefinitionKind::Parameter,
            sym("a"),
            param_a_span,
            Visibility::Private,
            ModulePath::root(),
        ));
        let param_b_def_id = defs.insert(DefinitionInfo::new(
            DefinitionKind::Parameter,
            sym("b"),
            param_b_span,
            Visibility::Private,
            ModulePath::root(),
        ));

        // Build the resolution map
        let mut resolutions = ResolutionMap::new();
        resolutions.record_binding(fn_span, fn_def_id);
        resolutions.record_binding(param_a_span, param_a_def_id);
        resolutions.record_binding(param_b_span, param_b_def_id);

        let func = FunctionDef {
            visibility: spark_ast::Visibility::Public,
            is_async: false,
            is_extern: false,
            name: Ident::new(sym("add"), fn_span),
            generics: vec![],
            params: vec![
                FnParam::new(Ident::new(sym("a"), param_a_span), int_type(), param_a_span),
                FnParam::new(Ident::new(sym("b"), param_b_span), int_type(), param_b_span),
            ],
            ret_type: int_type(),
            body: None,
            decorators: vec![],
            doc: None,
            span: Span::DUMMY,
        };

        let item = Item::Function(func);
        let mut ctx = TypeCheckCtx {
            table: &mut table,
            builtins: &builtins,
            definitions: &defs,
            resolutions: Some(&resolutions),
            diagnostics: &mut diags,
        };
        collect_item(&item, &mut ctx);

        assert!(!ctx.diagnostics.has_errors());
        let fn_ty = ctx.table.get_def_type(fn_def_id).unwrap();
        assert!(matches!(ctx.table.resolve(fn_ty), Ty::Function { .. }));
    }

    #[test]
    fn collect_struct_fields() {
        let mut table = crate::table::TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let mut defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let file = FileId::new(0);
        let struct_span = Span::new(file, 0, 4);

        let struct_def_id = defs.insert(DefinitionInfo::new(
            DefinitionKind::Struct,
            sym("User"),
            struct_span,
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

        // Build the resolution map
        let mut resolutions = ResolutionMap::new();
        resolutions.record_binding(struct_span, struct_def_id);

        let struct_item = StructDef {
            visibility: spark_ast::Visibility::Public,
            name: Ident::new(sym("User"), struct_span),
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
        let mut ctx = TypeCheckCtx {
            table: &mut table,
            builtins: &builtins,
            definitions: &defs,
            resolutions: Some(&resolutions),
            diagnostics: &mut diags,
        };
        collect_item(&item, &mut ctx);

        assert!(!ctx.diagnostics.has_errors());
        let fields = ctx.table.get_struct_fields(struct_def_id).unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0, sym("name"));
    }
}
