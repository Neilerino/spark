//! Lowering AST type expressions to internal types.
//!
//! Converts [`TypeExpr`] nodes from `spark_ast` into [`TypeId`]s by
//! looking up builtin names and resolving user-defined type references.

use spark_ast::{TypeExpr, TypeExprKind};
use spark_diagnostics::{Diagnostic, Label};
use spark_resolve::{DefId, DefinitionKind, DefinitionTable, ResolutionMap};

use crate::builtins::BuiltinTypes;
use crate::table::TypeTable;
use crate::ty::{Ty, TypeId};

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
                // Generic instantiation deferred — just use the base struct type
                let _ = type_args.iter().map(|t| {
                    lower_type_expr(t, table, builtins, definitions, resolutions, diagnostics)
                }).collect::<Vec<_>>();
            }
            table.intern(Ty::Struct(def_id))
        }
        DefinitionKind::Enum => {
            if !type_args.is_empty() {
                let _ = type_args.iter().map(|t| {
                    lower_type_expr(t, table, builtins, definitions, resolutions, diagnostics)
                }).collect::<Vec<_>>();
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

    // Not a known builtin generic — report error
    diagnostics.push(
        Diagnostic::error(format!("unresolved type `{name}`"))
            .with_label(Label::primary(type_expr.span, "not found")),
    );
    TypeId::ERROR
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
}
