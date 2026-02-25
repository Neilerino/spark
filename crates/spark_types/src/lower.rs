//! Lowering AST type expressions to internal types.
//!
//! Converts [`TypeExpr`] nodes from `spark_ast` into [`TypeId`]s by
//! looking up builtin names and resolving user-defined type references.

use spark_ast::{TypeExpr, TypeExprKind};
use spark_diagnostics::{Diagnostic, Label};
use spark_resolve::{DefId, DefinitionKind, DefinitionTable, ResolutionMap};

use crate::builtins::BuiltinTypes;
use crate::table::TypeTable;
use crate::ty::{LifetimeId, Ty, TypeId};

/// Lowers a [`TypeExpr`] to a [`TypeId`].
///
/// This function handles all `TypeExprKind` variants:
/// - Named types are resolved via builtins or the resolver's definition table
/// - Compound types (optional, union, mutable) are recursively lowered
/// - Structural and infer types produce `Ty::Error` with a diagnostic
pub fn lower_type_expr(
    type_expr: &TypeExpr,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    let ty = lower_type_expr_inner(type_expr, table, builtins, definitions, resolutions, diagnostics);
    // Record the lowered type at this ExprId so HIR lowering can look it up.
    table.set_expr_type(type_expr.id, ty);
    ty
}

/// Inner implementation of type-expression lowering.
fn lower_type_expr_inner(
    type_expr: &TypeExpr,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    match &type_expr.kind {
        TypeExprKind::Named { path, type_args } => {
            lower_named_type(path, type_args, type_expr, table, builtins, definitions, resolutions, diagnostics)
        }

        TypeExprKind::Optional(inner) => {
            let inner_id = lower_type_expr(inner, table, builtins, definitions, resolutions, diagnostics);
            table.intern(Ty::Optional(inner_id))
        }

        TypeExprKind::Union(types) => {
            let mut members: Vec<TypeId> = types
                .iter()
                .map(|t| lower_type_expr(t, table, builtins, definitions, resolutions, diagnostics))
                .collect();
            members.sort();
            members.dedup();

            if members.len() == 1 {
                return members[0];
            }

            table.intern(Ty::Union(members))
        }

        TypeExprKind::Mutable(inner) => {
            let inner_id = lower_type_expr(inner, table, builtins, definitions, resolutions, diagnostics);
            table.intern(Ty::Mutable(inner_id))
        }

        TypeExprKind::Tuple(elems) => {
            let elem_ids: Vec<TypeId> = elems
                .iter()
                .map(|t| lower_type_expr(t, table, builtins, definitions, resolutions, diagnostics))
                .collect();
            table.intern(Ty::Tuple(elem_ids))
        }

        TypeExprKind::Function { params, ret } => {
            let param_ids: Vec<TypeId> = params
                .iter()
                .map(|t| lower_type_expr(t, table, builtins, definitions, resolutions, diagnostics))
                .collect();
            let ret_id = lower_type_expr(ret, table, builtins, definitions, resolutions, diagnostics);
            table.intern(Ty::Function {
                params: param_ids,
                ret: ret_id,
                is_async: false,
            })
        }

        TypeExprKind::Ref { inner, mutable, lifetime } => {
            let inner_id = lower_type_expr(inner, table, builtins, definitions, resolutions, diagnostics);
            let lifetime_id = table.intern_lifetime(lifetime.name);
            table.intern(Ty::Ref {
                inner: inner_id,
                mutable: *mutable,
                lifetime: lifetime_id,
            })
        }

        TypeExprKind::Structural(_) => {
            diagnostics.push(
                Diagnostic::error("structural types are not yet supported")
                    .with_label(Label::primary(type_expr.span, "structural type")),
            );
            TypeId::ERROR
        }

        TypeExprKind::Infer => {
            diagnostics.push(
                Diagnostic::error("type inference is not supported; explicit types are required")
                    .with_label(Label::primary(type_expr.span, "cannot infer type")),
            );
            TypeId::ERROR
        }

        TypeExprKind::Error => TypeId::ERROR,
    }
}

/// Lowers a named type expression (built-in, resolved, or generic).
#[allow(clippy::too_many_arguments)]
fn lower_named_type(
    path: &spark_ast::Path,
    type_args: &[TypeExpr],
    type_expr: &TypeExpr,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    // Try to resolve as a built-in type first (single-segment paths)
    if path.is_simple() && type_args.is_empty() {
        let name = path.segments[0].name;
        if let Some(id) = builtins.lookup(name) {
            return id;
        }
    }

    // Try to resolve via the resolution map
    if let Some(res) = resolutions {
        if let Some(def_id) = res.get_path(path.span) {
            return lower_named_def(
                def_id, type_args, type_expr, table, builtins, definitions, resolutions, diagnostics,
            );
        }
    }

    // For simple builtin collection types with type args
    if path.is_simple() {
        let name = path.segments[0].name;
        return lower_builtin_generic(
            name, type_args, type_expr, table, builtins, definitions, resolutions, diagnostics,
        );
    }

    // Unresolved type — report error
    diagnostics.push(
        Diagnostic::error(format!("unresolved type `{}`", path.last().map_or("<empty>", |s| s.name.as_str())))
            .with_label(Label::primary(type_expr.span, "not found")),
    );
    TypeId::ERROR
}

/// Lowers a named type that was resolved to a `DefId`.
#[allow(clippy::too_many_arguments)]
fn lower_named_def(
    def_id: DefId,
    type_args: &[TypeExpr],
    type_expr: &TypeExpr,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    let Some(info) = definitions.get(def_id) else {
        return TypeId::ERROR;
    };

    // Check if it's a builtin type name that was also registered as a definition
    if let Some(id) = builtins.lookup(info.name) {
        if type_args.is_empty() {
            return id;
        }
    }

    match &info.kind {
        DefinitionKind::Struct => {
            if !type_args.is_empty() {
                let arg_types: Vec<TypeId> = type_args.iter()
                    .map(|t| lower_type_expr(t, table, builtins, definitions, resolutions, diagnostics))
                    .collect();
                // Validate arity against registered generic params
                if let Some(params) = table.get_generic_params(def_id).map(<[_]>::to_vec) {
                    if arg_types.len() != params.len() {
                        diagnostics.push(
                            Diagnostic::error(format!(
                                "expected {} type argument{}, found {}",
                                params.len(),
                                if params.len() == 1 { "" } else { "s" },
                                arg_types.len()
                            ))
                            .with_label(Label::primary(type_expr.span, "wrong number of type arguments")),
                        );
                        return TypeId::ERROR;
                    }
                    // Validate generic constraints on type arguments
                    let subst = crate::substitute::build_subst_map(&params, &arg_types);
                    crate::constraint::validate_constraints(
                        &params, &subst, table, definitions, type_expr.span, diagnostics,
                    );
                }
                return table.intern(Ty::Applied { base: def_id, args: arg_types });
            }
            // Check if this type requires type args
            if table.get_generic_params(def_id).is_some_and(|p| !p.is_empty()) {
                diagnostics.push(
                    Diagnostic::error(format!("missing type arguments for generic type `{}`", info.name))
                        .with_label(Label::primary(type_expr.span, "requires type arguments")),
                );
                return TypeId::ERROR;
            }
            table.intern(Ty::Struct(def_id))
        }
        DefinitionKind::Enum => {
            if !type_args.is_empty() {
                let arg_types: Vec<TypeId> = type_args.iter()
                    .map(|t| lower_type_expr(t, table, builtins, definitions, resolutions, diagnostics))
                    .collect();
                if let Some(params) = table.get_generic_params(def_id).map(<[_]>::to_vec) {
                    if arg_types.len() != params.len() {
                        diagnostics.push(
                            Diagnostic::error(format!(
                                "expected {} type argument{}, found {}",
                                params.len(),
                                if params.len() == 1 { "" } else { "s" },
                                arg_types.len()
                            ))
                            .with_label(Label::primary(type_expr.span, "wrong number of type arguments")),
                        );
                        return TypeId::ERROR;
                    }
                    // Validate generic constraints on type arguments
                    let subst = crate::substitute::build_subst_map(&params, &arg_types);
                    crate::constraint::validate_constraints(
                        &params, &subst, table, definitions, type_expr.span, diagnostics,
                    );
                }
                return table.intern(Ty::Applied { base: def_id, args: arg_types });
            }
            if table.get_generic_params(def_id).is_some_and(|p| !p.is_empty()) {
                diagnostics.push(
                    Diagnostic::error(format!("missing type arguments for generic type `{}`", info.name))
                        .with_label(Label::primary(type_expr.span, "requires type arguments")),
                );
                return TypeId::ERROR;
            }
            table.intern(Ty::Enum(def_id))
        }
        DefinitionKind::TypeAlias | DefinitionKind::TypeDef => {
            // For now, look up the type that was registered for this def
            table.get_def_type(def_id).unwrap_or(TypeId::ERROR)
        }
        DefinitionKind::Interface => {
            diagnostics.push(
                Diagnostic::error("interfaces cannot be used as types directly")
                    .with_label(Label::primary(type_expr.span, "interface used as type")),
            );
            TypeId::ERROR
        }
        DefinitionKind::GenericParam => {
            table.intern(Ty::Param(def_id))
        }
        _ => {
            diagnostics.push(
                Diagnostic::error(format!("`{}` is not a type", info.name))
                    .with_label(Label::primary(type_expr.span, "expected a type")),
            );
            TypeId::ERROR
        }
    }
}

/// Lowers built-in generic types like `List[T]`, `Map[K, V]`, `Set[T]`.
#[allow(clippy::too_many_arguments)]
fn lower_builtin_generic(
    name: spark_intern::Symbol,
    type_args: &[TypeExpr],
    type_expr: &TypeExpr,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    use spark_intern::kw;

    if name == kw::LIST {
        if type_args.len() != 1 {
            diagnostics.push(
                Diagnostic::error("List requires exactly one type argument")
                    .with_label(Label::primary(type_expr.span, "expected List[T]")),
            );
            return TypeId::ERROR;
        }
        let elem = lower_type_expr(&type_args[0], table, builtins, definitions, resolutions, diagnostics);
        return table.intern(Ty::List(elem));
    }

    if name == kw::MAP {
        if type_args.len() != 2 {
            diagnostics.push(
                Diagnostic::error("Map requires exactly two type arguments")
                    .with_label(Label::primary(type_expr.span, "expected Map[K, V]")),
            );
            return TypeId::ERROR;
        }
        let key = lower_type_expr(&type_args[0], table, builtins, definitions, resolutions, diagnostics);
        let value = lower_type_expr(&type_args[1], table, builtins, definitions, resolutions, diagnostics);
        return table.intern(Ty::Map(key, value));
    }

    if name == kw::SET {
        if type_args.len() != 1 {
            diagnostics.push(
                Diagnostic::error("Set requires exactly one type argument")
                    .with_label(Label::primary(type_expr.span, "expected Set[T]")),
            );
            return TypeId::ERROR;
        }
        let elem = lower_type_expr(&type_args[0], table, builtins, definitions, resolutions, diagnostics);
        return table.intern(Ty::Set(elem));
    }

    if name == kw::FUTURE {
        if type_args.len() != 1 {
            diagnostics.push(
                Diagnostic::error("Future requires exactly one type argument")
                    .with_label(Label::primary(type_expr.span, "expected Future[T]")),
            );
            return TypeId::ERROR;
        }
        let inner = lower_type_expr(&type_args[0], table, builtins, definitions, resolutions, diagnostics);
        return table.intern(Ty::Future { inner });
    }

    if name == kw::CHANNEL {
        if type_args.len() != 1 {
            diagnostics.push(
                Diagnostic::error("Channel requires exactly one type argument")
                    .with_label(Label::primary(type_expr.span, "expected Channel[T]")),
            );
            return TypeId::ERROR;
        }
        let inner = lower_type_expr(&type_args[0], table, builtins, definitions, resolutions, diagnostics);
        return table.intern(Ty::Channel { inner });
    }

    // Not a known builtin generic — report error
    diagnostics.push(
        Diagnostic::error(format!("unresolved type `{name}`"))
            .with_label(Label::primary(type_expr.span, "not found")),
    );
    TypeId::ERROR
}

/// Applies lifetime elision rules to a function signature.
///
/// If the return type contains anonymous-lifetime references, this function
/// infers which input lifetime they should share, following these rules:
///
/// 1. **No output refs**: No-op, returns unchanged types.
/// 2. **Single input ref**: Output refs share that input ref's lifetime.
/// 3. **Self parameter** (param 0 is a ref): Output refs share self's lifetime.
/// 4. **Multiple input refs, no self**: Error — ambiguous output lifetime.
/// 5. **No input refs, output has ref**: Error — cannot infer output lifetime.
///
/// Returns the (possibly rewritten) parameter types and return type.
pub fn apply_lifetime_elision(
    param_types: &[TypeId],
    ret_type: TypeId,
    fn_span: spark_span::Span,
    table: &mut TypeTable,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> (Vec<TypeId>, TypeId) {
    // If the return type has no anonymous refs, nothing to do
    if !has_anonymous_ref(ret_type, table) {
        return (param_types.to_vec(), ret_type);
    }

    // Collect input ref indices
    let input_ref_indices: Vec<usize> = param_types
        .iter()
        .enumerate()
        .filter(|(_, ty)| has_anonymous_ref(**ty, table))
        .map(|(i, _)| i)
        .collect();

    // Rule 5: No input refs but output has ref
    if input_ref_indices.is_empty() {
        diagnostics.push(
            Diagnostic::error("cannot infer output lifetime: no input references")
                .with_label(Label::primary(fn_span, "no input references to share lifetime with")),
        );
        return (param_types.to_vec(), ret_type);
    }

    // Rule 2: Single input ref
    if input_ref_indices.len() == 1 {
        let fresh_lt = table.intern_lifetime(spark_intern::sym("'elided"));
        let mut new_params = param_types.to_vec();
        new_params[input_ref_indices[0]] = rewrite_ref_lifetime(param_types[input_ref_indices[0]], fresh_lt, table);
        let new_ret = rewrite_ref_lifetime(ret_type, fresh_lt, table);
        return (new_params, new_ret);
    }

    // Rule 3: Self parameter (param 0 has a ref)
    if input_ref_indices.contains(&0) {
        let fresh_lt = table.intern_lifetime(spark_intern::sym("'self"));
        let mut new_params = param_types.to_vec();
        new_params[0] = rewrite_ref_lifetime(param_types[0], fresh_lt, table);
        let new_ret = rewrite_ref_lifetime(ret_type, fresh_lt, table);
        return (new_params, new_ret);
    }

    // Rule 4: Multiple input refs, no self
    diagnostics.push(
        Diagnostic::error("ambiguous output lifetime: multiple input references")
            .with_label(Label::primary(fn_span, "cannot determine which input lifetime to use")),
    );
    (param_types.to_vec(), ret_type)
}

/// Returns true if a type contains any anonymous-lifetime references.
fn has_anonymous_ref(ty: TypeId, table: &TypeTable) -> bool {
    if ty.is_error() {
        return false;
    }
    match table.resolve(ty).clone() {
        Ty::Ref { lifetime, .. } => lifetime.is_anonymous(),
        Ty::Optional(inner) | Ty::Mutable(inner) => has_anonymous_ref(inner, table),
        Ty::Tuple(elems) => elems.iter().any(|e| has_anonymous_ref(*e, table)),
        Ty::Applied { args, .. } => args.iter().any(|a| has_anonymous_ref(*a, table)),
        _ => false,
    }
}

/// Rewrites all anonymous-lifetime references in a type to use the given lifetime.
///
/// Recurses into compound types (Optional, Tuple, Mutable). Non-anonymous
/// lifetimes are left untouched.
fn rewrite_ref_lifetime(ty: TypeId, new_lt: LifetimeId, table: &mut TypeTable) -> TypeId {
    if ty.is_error() {
        return ty;
    }
    let resolved = table.resolve(ty).clone();
    match resolved {
        Ty::Ref { inner, mutable, lifetime } => {
            let new_inner = rewrite_ref_lifetime(inner, new_lt, table);
            if lifetime.is_anonymous() {
                table.intern(Ty::Ref {
                    inner: new_inner,
                    mutable,
                    lifetime: new_lt,
                })
            } else if new_inner == inner {
                ty
            } else {
                table.intern(Ty::Ref {
                    inner: new_inner,
                    mutable,
                    lifetime,
                })
            }
        }
        Ty::Optional(inner) => {
            let new_inner = rewrite_ref_lifetime(inner, new_lt, table);
            if new_inner == inner { ty } else { table.intern(Ty::Optional(new_inner)) }
        }
        Ty::Tuple(elems) => {
            let new_elems: Vec<_> = elems.iter().map(|e| rewrite_ref_lifetime(*e, new_lt, table)).collect();
            if new_elems == elems { ty } else { table.intern(Ty::Tuple(new_elems)) }
        }
        Ty::Mutable(inner) => {
            let new_inner = rewrite_ref_lifetime(inner, new_lt, table);
            if new_inner == inner { ty } else { table.intern(Ty::Mutable(new_inner)) }
        }
        _ => ty,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_ast::{Ident, Path};
    use spark_diagnostics::Diagnostics;
    use spark_intern::sym;
    use spark_span::Span;

    fn make_named(name: &str) -> TypeExpr {
        TypeExpr::new(
            TypeExprKind::Named {
                path: Path::from_ident(Ident::dummy(sym(name))),
                type_args: vec![],
            },
            Span::DUMMY,
        )
    }

    fn make_named_generic(name: &str, args: Vec<TypeExpr>) -> TypeExpr {
        TypeExpr::new(
            TypeExprKind::Named {
                path: Path::from_ident(Ident::dummy(sym(name))),
                type_args: args,
            },
            Span::DUMMY,
        )
    }

    #[test]
    fn lower_builtin_int() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let te = make_named("Int");
        let id = lower_type_expr(&te, &mut table, &builtins, &defs, None, &mut diags);

        assert!(!diags.has_errors());
        assert_eq!(id, builtins.int);
    }

    #[test]
    fn lower_builtin_bool() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let te = make_named("Bool");
        let id = lower_type_expr(&te, &mut table, &builtins, &defs, None, &mut diags);

        assert!(!diags.has_errors());
        assert_eq!(id, builtins.bool_ty);
    }

    #[test]
    fn lower_optional() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let te = TypeExpr::new(
            TypeExprKind::Optional(Box::new(make_named("Int"))),
            Span::DUMMY,
        );
        let id = lower_type_expr(&te, &mut table, &builtins, &defs, None, &mut diags);

        assert!(!diags.has_errors());
        assert!(matches!(table.resolve(id), Ty::Optional(_)));
    }

    #[test]
    fn lower_mutable() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let te = TypeExpr::new(
            TypeExprKind::Mutable(Box::new(make_named("Int"))),
            Span::DUMMY,
        );
        let id = lower_type_expr(&te, &mut table, &builtins, &defs, None, &mut diags);

        assert!(!diags.has_errors());
        assert!(matches!(table.resolve(id), Ty::Mutable(_)));
    }

    #[test]
    fn lower_union() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let te = TypeExpr::new(
            TypeExprKind::Union(vec![make_named("Int"), make_named("String")]),
            Span::DUMMY,
        );
        let id = lower_type_expr(&te, &mut table, &builtins, &defs, None, &mut diags);

        assert!(!diags.has_errors());
        assert!(matches!(table.resolve(id), Ty::Union(_)));
    }

    #[test]
    fn lower_tuple() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let te = TypeExpr::new(
            TypeExprKind::Tuple(vec![make_named("Int"), make_named("Bool")]),
            Span::DUMMY,
        );
        let id = lower_type_expr(&te, &mut table, &builtins, &defs, None, &mut diags);

        assert!(!diags.has_errors());
        assert!(matches!(table.resolve(id), Ty::Tuple(_)));
    }

    #[test]
    fn lower_function_type() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let te = TypeExpr::new(
            TypeExprKind::Function {
                params: vec![make_named("Int")],
                ret: Box::new(make_named("Bool")),
            },
            Span::DUMMY,
        );
        let id = lower_type_expr(&te, &mut table, &builtins, &defs, None, &mut diags);

        assert!(!diags.has_errors());
        assert!(matches!(table.resolve(id), Ty::Function { .. }));
    }

    #[test]
    fn lower_list_generic() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let te = make_named_generic("List", vec![make_named("Int")]);
        let id = lower_type_expr(&te, &mut table, &builtins, &defs, None, &mut diags);

        assert!(!diags.has_errors());
        assert!(matches!(table.resolve(id), Ty::List(_)));
    }

    #[test]
    fn lower_map_generic() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let te = make_named_generic("Map", vec![make_named("String"), make_named("Int")]);
        let id = lower_type_expr(&te, &mut table, &builtins, &defs, None, &mut diags);

        assert!(!diags.has_errors());
        assert!(matches!(table.resolve(id), Ty::Map(_, _)));
    }

    #[test]
    fn lower_error_type() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let te = TypeExpr::error(Span::DUMMY);
        let id = lower_type_expr(&te, &mut table, &builtins, &defs, None, &mut diags);

        assert!(!diags.has_errors());
        assert!(id.is_error());
    }

    #[test]
    fn lower_structural_produces_error() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let te = TypeExpr::new(TypeExprKind::Structural(vec![]), Span::DUMMY);
        let id = lower_type_expr(&te, &mut table, &builtins, &defs, None, &mut diags);

        assert!(diags.has_errors());
        assert!(id.is_error());
    }

    #[test]
    fn lower_unresolved_produces_error() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let te = make_named("NonExistent");
        let id = lower_type_expr(&te, &mut table, &builtins, &defs, None, &mut diags);

        assert!(diags.has_errors());
        assert!(id.is_error());
    }

    // === Lifetime elision tests ===

    fn make_ref_ty(inner: TypeId, table: &mut TypeTable) -> TypeId {
        table.intern(Ty::Ref {
            inner,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        })
    }

    #[test]
    fn elision_single_input_ref() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let mut diags = Diagnostics::new();

        let ref_int = make_ref_ty(builtins.int, &mut table);
        let ref_string = make_ref_ty(builtins.string, &mut table);

        let (new_params, new_ret) = apply_lifetime_elision(
            &[ref_int], ref_string, Span::DUMMY, &mut table, &mut diags,
        );
        assert!(!diags.has_errors());

        // Both should have the same non-anonymous lifetime
        let param_lt = match table.resolve(new_params[0]) {
            Ty::Ref { lifetime, .. } => *lifetime,
            _ => panic!("expected Ref"),
        };
        let ret_lt = match table.resolve(new_ret) {
            Ty::Ref { lifetime, .. } => *lifetime,
            _ => panic!("expected Ref"),
        };
        assert!(!param_lt.is_anonymous());
        assert_eq!(param_lt, ret_lt);
    }

    #[test]
    fn elision_no_input_refs_error() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let mut diags = Diagnostics::new();

        let ref_int = make_ref_ty(builtins.int, &mut table);

        let (_new_params, _new_ret) = apply_lifetime_elision(
            &[builtins.int], ref_int, Span::DUMMY, &mut table, &mut diags,
        );
        assert!(diags.has_errors());
    }

    #[test]
    fn elision_multiple_input_refs_error() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let mut diags = Diagnostics::new();

        let ref_int = make_ref_ty(builtins.int, &mut table);
        let ref_string = make_ref_ty(builtins.string, &mut table);
        let ref_bool = make_ref_ty(builtins.bool_ty, &mut table);

        // Two non-self refs with output ref → ambiguous
        let (_new_params, _new_ret) = apply_lifetime_elision(
            &[builtins.int, ref_int, ref_string], ref_bool, Span::DUMMY, &mut table, &mut diags,
        );
        assert!(diags.has_errors());
    }

    #[test]
    fn elision_self_parameter() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let mut diags = Diagnostics::new();

        let ref_int = make_ref_ty(builtins.int, &mut table);
        let ref_string = make_ref_ty(builtins.string, &mut table);
        let ref_bool = make_ref_ty(builtins.bool_ty, &mut table);

        // param 0 is ref → self rule applies
        let (new_params, new_ret) = apply_lifetime_elision(
            &[ref_int, ref_string], ref_bool, Span::DUMMY, &mut table, &mut diags,
        );
        assert!(!diags.has_errors(), "Errors: {:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>());

        // Self param and return should share lifetime
        let self_lt = match table.resolve(new_params[0]) {
            Ty::Ref { lifetime, .. } => *lifetime,
            _ => panic!("expected Ref"),
        };
        let ret_lt = match table.resolve(new_ret) {
            Ty::Ref { lifetime, .. } => *lifetime,
            _ => panic!("expected Ref"),
        };
        assert!(!self_lt.is_anonymous());
        assert_eq!(self_lt, ret_lt);

        // Second param should remain anonymous
        let other_lt = match table.resolve(new_params[1]) {
            Ty::Ref { lifetime, .. } => *lifetime,
            _ => panic!("expected Ref"),
        };
        assert!(other_lt.is_anonymous());
    }

    #[test]
    fn elision_no_output_refs_noop() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let mut diags = Diagnostics::new();

        let ref_int = make_ref_ty(builtins.int, &mut table);

        let (new_params, new_ret) = apply_lifetime_elision(
            &[ref_int], builtins.int, Span::DUMMY, &mut table, &mut diags,
        );
        assert!(!diags.has_errors());
        assert_eq!(new_params[0], ref_int); // unchanged
        assert_eq!(new_ret, builtins.int);  // unchanged
    }

    #[test]
    fn elision_explicit_lifetime_preserved() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let mut diags = Diagnostics::new();

        // Create a ref with an explicit (non-anonymous) lifetime
        let explicit_lt = table.intern_lifetime(spark_intern::sym("'a"));
        let ref_with_explicit = table.intern(Ty::Ref {
            inner: builtins.int,
            mutable: false,
            lifetime: explicit_lt,
        });

        let (new_params, new_ret) = apply_lifetime_elision(
            &[ref_with_explicit], builtins.int, Span::DUMMY, &mut table, &mut diags,
        );
        assert!(!diags.has_errors());
        // Should be completely unchanged — no anonymous refs in output
        assert_eq!(new_params[0], ref_with_explicit);
        assert_eq!(new_ret, builtins.int);
    }

    #[test]
    fn elision_nested_ref_in_tuple() {
        let mut table = TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let mut diags = Diagnostics::new();

        let ref_int = make_ref_ty(builtins.int, &mut table);
        let ret_tuple = table.intern(Ty::Tuple(vec![ref_int, builtins.int]));

        let (new_params, new_ret) = apply_lifetime_elision(
            &[ref_int], ret_tuple, Span::DUMMY, &mut table, &mut diags,
        );
        assert!(!diags.has_errors());

        // The ref inside the tuple should have a non-anonymous lifetime
        let new_tuple_elems = match table.resolve(new_ret) {
            Ty::Tuple(elems) => elems.clone(),
            _ => panic!("expected Tuple"),
        };
        let inner_lt = match table.resolve(new_tuple_elems[0]) {
            Ty::Ref { lifetime, .. } => *lifetime,
            _ => panic!("expected Ref in tuple"),
        };
        assert!(!inner_lt.is_anonymous());

        // The param ref should share the same lifetime
        let param_lt = match table.resolve(new_params[0]) {
            Ty::Ref { lifetime, .. } => *lifetime,
            _ => panic!("expected Ref"),
        };
        assert_eq!(param_lt, inner_lt);
    }
}
