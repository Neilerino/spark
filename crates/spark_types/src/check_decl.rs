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

use spark_diagnostics::{Diagnostic, Label};

use crate::ctx::TypeCheckCtx;
use crate::display::display_type as type_name;
use crate::lower::lower_type_expr;
use crate::table::InterfaceMethodInfo;
use crate::ty::{Constraint, Ty, TypeId};

/// Collects type signatures for a single item.
///
/// This is called during Pass 1 to register definitions before their
/// bodies are type-checked.
pub fn collect_item(item: &Item, ctx: &mut TypeCheckCtx<'_>) {
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
fn collect_function(func: &FunctionDef, ctx: &mut TypeCheckCtx<'_>) {
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
        &param_types,
        ret_type,
        func.span,
        ctx.table,
        ctx.diagnostics,
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

        // Store parameter names for partial application resolution
        let param_names: Vec<spark_intern::Symbol> =
            func.params.iter().map(|p| p.name.name).collect();
        ctx.table.set_fn_param_names(def_id, param_names);
    }
}

/// Collects a struct's type and field types.
fn collect_struct(struct_def: &StructDef, ctx: &mut TypeCheckCtx<'_>) {
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
fn collect_enum(enum_def: &EnumDef, ctx: &mut TypeCheckCtx<'_>) {
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
            if let Some(variant_def) = find_variant_def(variant.name.name, def_id, ctx.definitions)
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
fn collect_const(const_def: &ConstDef, ctx: &mut TypeCheckCtx<'_>) {
    let ty = lower_type_expr(&const_def.ty, ctx);
    if let Some(def_id) = find_item_def(const_def.name.span, ctx.resolutions) {
        ctx.table.set_def_type(def_id, ty);
    }
}

/// Collects a type alias.
fn collect_type_alias(alias_def: &TypeAliasDef, ctx: &mut TypeCheckCtx<'_>) {
    let ty = lower_type_expr(&alias_def.ty, ctx);
    if let Some(def_id) = find_item_def(alias_def.name.span, ctx.resolutions) {
        ctx.table.set_def_type(def_id, ty);
    }
}

/// Collects a distinct type definition.
fn collect_type_def(type_def: &TypeDefDef, ctx: &mut TypeCheckCtx<'_>) {
    let ty = lower_type_expr(&type_def.ty, ctx);
    if let Some(def_id) = find_item_def(type_def.name.span, ctx.resolutions) {
        ctx.table.set_def_type(def_id, ty);
    }
}

/// Collects an interface's method signatures.
///
/// Registers each method as a `Ty::Function` so that bindings can be
/// validated against the interface's requirements. Also stores
/// [`InterfaceMethodInfo`] for each method so that `validate_interface_methods`
/// can check binding implementations.
fn collect_interface(iface: &InterfaceDef, ctx: &mut TypeCheckCtx<'_>) {
    // Register the interface type itself
    let Some(def_id) = find_item_def(iface.name.span, ctx.resolutions) else {
        return;
    };
    let iface_type = ctx.table.intern(Ty::Param(def_id));
    ctx.table.set_def_type(def_id, iface_type);

    // Lower each method's signature and store for later validation.
    let mut method_infos = Vec::with_capacity(iface.methods.len());
    for method in &iface.methods {
        // Interface methods may use `_` as a self-type placeholder (parsed as
        // Named("_") or Infer). Skip lowering those to avoid spurious diagnostics.
        let has_placeholder = method.params.iter().any(|p| is_placeholder_type(&p.ty))
            || is_placeholder_type(&method.ret_type);

        let param_types: Vec<TypeId> = method
            .params
            .iter()
            .map(|p| lower_or_placeholder(&p.ty, ctx))
            .collect();
        let ret_type = lower_or_placeholder(&method.ret_type, ctx);
        let has_generics = has_placeholder
            || !method.generics.is_empty()
            || param_types
                .iter()
                .any(|t| type_involves_param(*t, ctx.table))
            || type_involves_param(ret_type, ctx.table);

        method_infos.push(InterfaceMethodInfo {
            name: method.name.name,
            param_types,
            ret_type,
            is_async: method.is_async,
            has_default: method.body.is_some(),
            has_generics,
            span: method.span,
        });
    }
    ctx.table.set_interface_methods(def_id, method_infos);
}

/// Collects method signatures from an interface binding.
///
/// For each binding target method, lowers the parameter and return types,
/// interns the function type, and registers it on the method's `DefId`.
/// Also records the methods in `TypeTable::type_methods` for lookup
/// during method call resolution.
fn collect_binding(binding: &BindingDef, ctx: &mut TypeCheckCtx<'_>) {
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
            ctx.table
                .add_interface_binding(type_def_id, interface_def_id);
            validate_interface_methods(interface_def_id, &target.methods, ctx);
        }

        for method in &target.methods {
            // Find the method's DefId (allocated by resolver as BindingMethod)
            let method_def_id =
                find_binding_method_def(method.name.name, type_def_id, ctx.definitions);

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
                    &param_types,
                    ret_type,
                    method.span,
                    ctx.table,
                    ctx.diagnostics,
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

                // Store parameter names for partial application resolution
                let param_names: Vec<spark_intern::Symbol> =
                    method.params.iter().map(|p| p.name.name).collect();
                ctx.table.set_fn_param_names(method_def_id, param_names);

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
/// Checks for:
/// 1. Missing required methods (interface methods without default bodies)
/// 2. Parameter count mismatch
/// 3. Return type mismatch (non-generic methods only)
/// 4. Async/sync mismatch
/// 5. Parameter type mismatch (non-generic methods only)
fn validate_interface_methods(
    interface_def_id: DefId,
    binding_methods: &[FunctionDef],
    ctx: &mut TypeCheckCtx<'_>,
) {
    // Look up the interface name for diagnostics.
    let iface_name = match ctx.definitions.get(interface_def_id) {
        Some(info) if matches!(info.kind, DefinitionKind::Interface) => info.name,
        _ => return,
    };

    // Clone the method infos to avoid borrowing ctx.table while we mutate ctx.
    let iface_methods = match ctx.table.get_interface_methods(interface_def_id) {
        Some(methods) => methods.to_vec(),
        None => return,
    };

    for iface_method in &iface_methods {
        // Find the corresponding binding method by name.
        let binding_method = binding_methods
            .iter()
            .find(|m| m.name.name == iface_method.name);

        let Some(binding_method) = binding_method else {
            // Missing required method (skip methods with default implementations).
            if !iface_method.has_default {
                ctx.diagnostics.push(
                    Diagnostic::error(format!(
                        "missing required method `{}` from interface `{iface_name}`",
                        iface_method.name
                    ))
                    .with_label(Label::secondary(
                        iface_method.span,
                        "required by this interface method",
                    )),
                );
            }
            continue;
        };

        // Determine parameter offset: binding methods may have an extra receiver
        // parameter at index 0 (e.g., `fn recv(server: Server)`) that the interface
        // omits (e.g., `fn recv()`). If the binding has exactly one more param than
        // the interface, the first param is treated as the receiver and skipped.
        let binding_params = if binding_method.params.len() == iface_method.param_types.len() + 1 {
            &binding_method.params[1..]
        } else {
            binding_method.params.as_slice()
        };

        // Check parameter count (after accounting for receiver).
        if binding_params.len() != iface_method.param_types.len() {
            ctx.diagnostics.push(
                Diagnostic::error(format!(
                    "method `{}` has {} parameter{}, but interface `{iface_name}` expects {}",
                    iface_method.name,
                    binding_params.len(),
                    if binding_params.len() == 1 { "" } else { "s" },
                    iface_method.param_types.len()
                ))
                .with_label(Label::primary(
                    binding_method.span,
                    "wrong number of parameters",
                ))
                .with_label(Label::secondary(
                    iface_method.span,
                    "interface method defined here",
                )),
            );
            continue; // Skip further checks if param count is wrong.
        }

        // Check async mismatch.
        if binding_method.is_async != iface_method.is_async {
            let expected = if iface_method.is_async {
                "async"
            } else {
                "sync"
            };
            let found = if binding_method.is_async {
                "async"
            } else {
                "sync"
            };
            ctx.diagnostics.push(
                Diagnostic::error(format!(
                    "method `{}` is {found}, but interface `{iface_name}` declares it as {expected}",
                    iface_method.name
                ))
                .with_label(Label::primary(binding_method.span, format!("this method is {found}")))
                .with_label(Label::secondary(iface_method.span, format!("interface declares {expected}"))),
            );
        }

        // Skip type-level comparison for generic methods.
        if iface_method.has_generics {
            continue;
        }

        // Check return type mismatch.
        let binding_ret = lower_type_expr(&binding_method.ret_type, ctx);
        if !binding_ret.is_error()
            && !iface_method.ret_type.is_error()
            && binding_ret != iface_method.ret_type
        {
            ctx.diagnostics.push(
                Diagnostic::error(format!(
                    "method `{}` returns `{}`, but interface `{iface_name}` expects `{}`",
                    iface_method.name,
                    type_name(binding_ret, ctx.table, ctx.definitions),
                    type_name(iface_method.ret_type, ctx.table, ctx.definitions)
                ))
                .with_label(Label::primary(
                    binding_method.ret_type.span,
                    "wrong return type",
                ))
                .with_label(Label::secondary(
                    iface_method.span,
                    "interface method defined here",
                )),
            );
        }

        // Check parameter type mismatches.
        for (i, param) in binding_params.iter().enumerate() {
            let binding_param_ty = lower_type_expr(&param.ty, ctx);
            let iface_param_ty = iface_method.param_types[i];
            if !binding_param_ty.is_error()
                && !iface_param_ty.is_error()
                && binding_param_ty != iface_param_ty
            {
                ctx.diagnostics.push(
                    Diagnostic::error(format!(
                        "parameter `{}` of method `{}` has type `{}`, but interface `{iface_name}` expects `{}`",
                        param.name.name,
                        iface_method.name,
                        type_name(binding_param_ty, ctx.table, ctx.definitions),
                        type_name(iface_param_ty, ctx.table, ctx.definitions)
                    ))
                    .with_label(Label::primary(param.ty.span, "wrong parameter type"))
                    .with_label(Label::secondary(iface_method.span, "interface method defined here")),
                );
            }
        }
    }
}

/// Lowers a type expression, returning `TypeId::ERROR` for placeholder (`_`) types
/// without emitting spurious diagnostics.
fn lower_or_placeholder(ty: &spark_ast::TypeExpr, ctx: &mut TypeCheckCtx<'_>) -> TypeId {
    if is_placeholder_type(ty) {
        TypeId::ERROR
    } else {
        lower_type_expr(ty, ctx)
    }
}

/// Returns `true` if a type expression is the `_` placeholder used in interface
/// method signatures for the `self` parameter type.
fn is_placeholder_type(ty: &spark_ast::TypeExpr) -> bool {
    match &ty.kind {
        spark_ast::TypeExprKind::Infer => true,
        spark_ast::TypeExprKind::Named { path, type_args } => {
            type_args.is_empty()
                && path.is_simple()
                && path.segments[0].name == spark_intern::kw::UNDERSCORE
        }
        _ => false,
    }
}

/// Returns `true` if the type contains any `Ty::Param` (generic type parameter).
fn type_involves_param(ty: TypeId, table: &crate::table::TypeTable) -> bool {
    if ty.is_error() {
        return false;
    }
    match table.resolve(ty) {
        Ty::Param(_) => true,
        Ty::Optional(inner)
        | Ty::Mutable(inner)
        | Ty::List(inner)
        | Ty::Set(inner)
        | Ty::Future { inner }
        | Ty::Channel { inner }
        | Ty::Ref { inner, .. } => type_involves_param(*inner, table),
        Ty::Map(k, v) => type_involves_param(*k, table) || type_involves_param(*v, table),
        Ty::Tuple(elems) => elems.iter().any(|e| type_involves_param(*e, table)),
        Ty::Union(members) => members.iter().any(|m| type_involves_param(*m, table)),
        Ty::Function { params, ret, .. } => {
            params.iter().any(|p| type_involves_param(*p, table))
                || type_involves_param(*ret, table)
        }
        Ty::Applied { args, .. } => args.iter().any(|a| type_involves_param(*a, table)),
        _ => false,
    }
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
        if let Some(def_id) = ctx
            .resolutions
            .and_then(|res| res.get_binding(generic.name.span))
        {
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
                let constraint_def_id = ctx.resolutions.and_then(|res| res.get_path(path.span));

                if let Some(cdef_id) = constraint_def_id {
                    let info = ctx.definitions.get(cdef_id)?;
                    match &info.kind {
                        DefinitionKind::Interface => {
                            // Lower type arguments (e.g., [T] in Comparable[T])
                            let args: Vec<TypeId> =
                                type_args.iter().map(|t| lower_type_expr(t, ctx)).collect();
                            Some(Constraint::Interface {
                                interface_def_id: cdef_id,
                                type_args: args,
                            })
                        }
                        DefinitionKind::Struct => {
                            // Named type constraint: T must have all fields of the struct
                            ctx.table.get_struct_fields(cdef_id).map(|fields| {
                                Constraint::Structural {
                                    fields: fields.to_vec(),
                                }
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
            Some(Constraint::Structural {
                fields: lowered_fields,
            })
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
            info.name == name
                && matches!(info.kind, DefinitionKind::EnumVariant(parent) if parent == parent_enum)
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
    #[allow(clippy::similar_names)]
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
        let mut ctx =
            TypeCheckCtx::new(&mut table, &builtins, &defs, Some(&resolutions), &mut diags);
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
        let mut ctx =
            TypeCheckCtx::new(&mut table, &builtins, &defs, Some(&resolutions), &mut diags);
        collect_item(&item, &mut ctx);

        assert!(!ctx.diagnostics.has_errors());
        let fields = ctx.table.get_struct_fields(struct_def_id).unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0, sym("name"));
    }
}
