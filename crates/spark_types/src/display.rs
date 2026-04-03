//! Human-readable and mangling-friendly type name formatting.
//!
//! Provides two free functions for converting [`TypeId`] to strings:
//!
//! - [`display_type`] — full human-readable names for diagnostics and LSP hover
//! - [`mangle_type`] — compact deterministic names for symbol mangling
//!
//! Both live here rather than on [`TypeTable`] because `display_type` requires a
//! [`DefinitionTable`] that `TypeTable` does not own, and free functions in a
//! dedicated module keep the API consistent.

use spark_resolve::DefinitionTable;

use crate::table::TypeTable;
use crate::ty::{Ty, TypeId};

/// Returns a human-readable name for a type, suitable for diagnostics and
/// user-facing messages.
///
/// This formats types with their full Spark syntax names (e.g. `Int64`,
/// `List[String]`, `Int | Null`). Struct/enum/param names are looked up
/// in the [`DefinitionTable`].
///
/// # Examples
///
/// ```ignore
/// use spark_types::display::display_type;
///
/// let name = display_type(some_type_id, &table, &definitions);
/// // e.g. "List[Int64]", "String?", "MyStruct"
/// ```
#[must_use]
pub fn display_type(ty: TypeId, table: &TypeTable, definitions: &DefinitionTable) -> String {
    match table.resolve(ty) {
        Ty::Int(w) => w.to_string(),
        Ty::Float(w) => w.to_string(),
        Ty::Bool => "Bool".to_string(),
        Ty::Char => "Char".to_string(),
        Ty::String => "String".to_string(),
        Ty::Byte => "Byte".to_string(),
        Ty::CPtr => "CPtr".to_string(),
        Ty::Void => "Void".to_string(),
        Ty::Null => "Null".to_string(),
        Ty::Never => "Never".to_string(),
        Ty::List(elem) => format!("List[{}]", display_type(*elem, table, definitions)),
        Ty::Map(k, v) => format!(
            "Map[{}, {}]",
            display_type(*k, table, definitions),
            display_type(*v, table, definitions)
        ),
        Ty::Set(elem) => format!("Set[{}]", display_type(*elem, table, definitions)),
        Ty::Tuple(elems) => {
            let parts: Vec<_> = elems
                .iter()
                .map(|e| display_type(*e, table, definitions))
                .collect();
            format!("Tup[{}]", parts.join(", "))
        }
        Ty::Struct(def_id) => definitions
            .get(*def_id)
            .map_or_else(|| "<struct>".to_string(), |info| info.name.to_string()),
        Ty::Enum(def_id) => definitions
            .get(*def_id)
            .map_or_else(|| "<enum>".to_string(), |info| info.name.to_string()),
        Ty::Union(members) => {
            let parts: Vec<_> = members
                .iter()
                .map(|m| display_type(*m, table, definitions))
                .collect();
            parts.join(" | ")
        }
        Ty::Optional(inner) => format!("{}?", display_type(*inner, table, definitions)),
        Ty::Mutable(inner) => format!("{}!", display_type(*inner, table, definitions)),
        Ty::Ref {
            inner, mutable, ..
        } => {
            if *mutable {
                format!("Ref[{}]!", display_type(*inner, table, definitions))
            } else {
                format!("Ref[{}]", display_type(*inner, table, definitions))
            }
        }
        Ty::Function {
            params,
            ret,
            is_async,
        } => {
            let params_str: Vec<_> = params
                .iter()
                .map(|p| display_type(*p, table, definitions))
                .collect();
            let async_prefix = if *is_async { "async " } else { "" };
            format!(
                "{}({}): {}",
                async_prefix,
                params_str.join(", "),
                display_type(*ret, table, definitions)
            )
        }
        Ty::Param(def_id) => definitions
            .get(*def_id)
            .map_or_else(|| "<param>".to_string(), |info| info.name.to_string()),
        Ty::Applied { base, args } => {
            let base_name = definitions
                .get(*base)
                .map_or_else(|| "<generic>".to_string(), |info| info.name.to_string());
            let arg_names: Vec<_> = args
                .iter()
                .map(|a| display_type(*a, table, definitions))
                .collect();
            format!("{}[{}]", base_name, arg_names.join(", "))
        }
        Ty::Future { inner } => format!("Future[{}]", display_type(*inner, table, definitions)),
        Ty::Channel { inner } => {
            format!("Channel[{}]", display_type(*inner, table, definitions))
        }
        Ty::AsyncState { .. } => "<async-state>".to_string(),
        Ty::Error => "<error>".to_string(),
    }
}

/// Returns a compact, deterministic name for a type, suitable for symbol
/// mangling in monomorphized function names.
///
/// Unlike [`display_type`], this does not require a [`DefinitionTable`] — it
/// uses raw `DefId` indices for user-defined types. The output is designed to
/// be unambiguous and stable across compilations of the same source.
///
/// # Examples
///
/// ```ignore
/// use spark_types::display::mangle_type;
///
/// let mangled = mangle_type(some_type_id, &table);
/// // e.g. "Int64", "List_Bool", "D42_Int64"
/// ```
#[must_use]
pub fn mangle_type(ty_id: TypeId, table: &TypeTable) -> String {
    match table.resolve(ty_id) {
        Ty::Int(w) => format!("{w:?}"),
        Ty::Float(w) => format!("{w:?}"),
        Ty::Bool => "Bool".to_owned(),
        Ty::Char => "Char".to_owned(),
        Ty::String => "String".to_owned(),
        Ty::Byte => "Byte".to_owned(),
        Ty::CPtr => "CPtr".to_owned(),
        Ty::Void => "Void".to_owned(),
        Ty::Null => "Null".to_owned(),
        Ty::Never => "Never".to_owned(),
        Ty::List(inner) => format!("List_{}", mangle_type(*inner, table)),
        Ty::Map(k, v) => format!("Map_{}{}", mangle_type(*k, table), mangle_type(*v, table)),
        Ty::Set(elem) => format!("Set_{}", mangle_type(*elem, table)),
        Ty::Tuple(elems) => {
            let mut s = "Tup".to_owned();
            for e in elems {
                s.push('_');
                s.push_str(&mangle_type(*e, table));
            }
            s
        }
        Ty::Struct(d) | Ty::Enum(d) => format!("D{}", d.as_u32()),
        Ty::Union(members) => {
            let mut s = "U".to_owned();
            for m in members {
                s.push('_');
                s.push_str(&mangle_type(*m, table));
            }
            s
        }
        Ty::Optional(inner) => format!("Opt_{}", mangle_type(*inner, table)),
        Ty::Mutable(inner) => format!("Mut_{}", mangle_type(*inner, table)),
        Ty::Ref { inner, mutable, .. } => {
            if *mutable {
                format!("RefMut_{}", mangle_type(*inner, table))
            } else {
                format!("Ref_{}", mangle_type(*inner, table))
            }
        }
        Ty::Function { params, ret, .. } => {
            let mut s = "Fn".to_owned();
            for p in params {
                s.push('_');
                s.push_str(&mangle_type(*p, table));
            }
            s.push_str("_R");
            s.push_str(&mangle_type(*ret, table));
            s
        }
        Ty::Param(d) => format!("P{}", d.as_u32()),
        Ty::Applied { base, args } => {
            let mut s = format!("D{}", base.as_u32());
            for a in args {
                s.push('_');
                s.push_str(&mangle_type(*a, table));
            }
            s
        }
        Ty::Future { inner } => format!("Fut_{}", mangle_type(*inner, table)),
        Ty::Channel { inner } => format!("Chan_{}", mangle_type(*inner, table)),
        Ty::AsyncState { func_def_id, .. } => format!("AS{}", func_def_id.as_u32()),
        Ty::Error => "Err".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::register_builtins;
    use crate::ty::LifetimeId;
    use spark_resolve::DefinitionTable;

    // ── display_type ──────────────────────────────────────────────

    #[test]
    fn display_primitives() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();

        assert_eq!(display_type(builtins.int, &table, &defs), "Int64");
        assert_eq!(display_type(builtins.bool_ty, &table, &defs), "Bool");
        assert_eq!(display_type(builtins.string, &table, &defs), "String");
        assert_eq!(display_type(builtins.void, &table, &defs), "Void");
    }

    #[test]
    fn display_optional() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let opt_int = table.intern(Ty::Optional(builtins.int));
        assert_eq!(display_type(opt_int, &table, &defs), "Int64?");
    }

    #[test]
    fn display_list() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let list_int = table.intern(Ty::List(builtins.int));
        assert_eq!(display_type(list_int, &table, &defs), "List[Int64]");
    }

    #[test]
    fn display_map() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let map_ty = table.intern(Ty::Map(builtins.string, builtins.int));
        assert_eq!(display_type(map_ty, &table, &defs), "Map[String, Int64]");
    }

    #[test]
    fn display_union() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let union_ty = table.intern(Ty::Union(vec![builtins.int, builtins.string]));
        assert_eq!(
            display_type(union_ty, &table, &defs),
            "Int64 | String"
        );
    }

    #[test]
    fn display_function() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let fn_ty = table.intern(Ty::Function {
            params: vec![builtins.int, builtins.string],
            ret: builtins.bool_ty,
            is_async: false,
        });
        assert_eq!(
            display_type(fn_ty, &table, &defs),
            "(Int64, String): Bool"
        );
    }

    #[test]
    fn display_async_function() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let fn_ty = table.intern(Ty::Function {
            params: vec![builtins.int],
            ret: builtins.void,
            is_async: true,
        });
        assert_eq!(display_type(fn_ty, &table, &defs), "async (Int64): Void");
    }

    #[test]
    fn display_ref() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let ref_ty = table.intern(Ty::Ref {
            inner: builtins.int,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        assert_eq!(display_type(ref_ty, &table, &defs), "Ref[Int64]");
        let ref_mut = table.intern(Ty::Ref {
            inner: builtins.int,
            mutable: true,
            lifetime: LifetimeId::ANONYMOUS,
        });
        assert_eq!(display_type(ref_mut, &table, &defs), "Ref[Int64]!");
    }

    #[test]
    fn display_mutable() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut_ty = table.intern(Ty::Mutable(builtins.int));
        assert_eq!(display_type(mut_ty, &table, &defs), "Int64!");
    }

    #[test]
    fn display_error() {
        let table = TypeTable::new();
        let defs = DefinitionTable::new();
        assert_eq!(display_type(TypeId::ERROR, &table, &defs), "<error>");
    }

    #[test]
    fn display_never() {
        let mut table = TypeTable::new();
        let defs = DefinitionTable::new();
        let never = table.intern(Ty::Never);
        assert_eq!(display_type(never, &table, &defs), "Never");
    }

    #[test]
    fn display_null() {
        let mut table = TypeTable::new();
        let defs = DefinitionTable::new();
        let null = table.intern(Ty::Null);
        assert_eq!(display_type(null, &table, &defs), "Null");
    }

    #[test]
    fn display_tuple() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let tup = table.intern(Ty::Tuple(vec![builtins.int, builtins.string, builtins.bool_ty]));
        assert_eq!(
            display_type(tup, &table, &defs),
            "Tup[Int64, String, Bool]"
        );
    }

    #[test]
    fn display_set() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let set_ty = table.intern(Ty::Set(builtins.int));
        assert_eq!(display_type(set_ty, &table, &defs), "Set[Int64]");
    }

    #[test]
    fn display_future() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let fut = table.intern(Ty::Future { inner: builtins.int });
        assert_eq!(display_type(fut, &table, &defs), "Future[Int64]");
    }

    #[test]
    fn display_channel() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let ch = table.intern(Ty::Channel { inner: builtins.string });
        assert_eq!(display_type(ch, &table, &defs), "Channel[String]");
    }

    // ── mangle_type ───────────────────────────────────────────────

    #[test]
    fn mangle_primitives() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);

        assert_eq!(mangle_type(builtins.int, &table), "I64");
        assert_eq!(mangle_type(builtins.bool_ty, &table), "Bool");
        assert_eq!(mangle_type(builtins.string, &table), "String");
        assert_eq!(mangle_type(builtins.void, &table), "Void");
    }

    #[test]
    fn mangle_list() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let list_int = table.intern(Ty::List(builtins.int));
        assert_eq!(mangle_type(list_int, &table), "List_I64");
    }

    #[test]
    fn mangle_optional() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let opt = table.intern(Ty::Optional(builtins.int));
        assert_eq!(mangle_type(opt, &table), "Opt_I64");
    }

    #[test]
    fn mangle_error() {
        let table = TypeTable::new();
        assert_eq!(mangle_type(TypeId::ERROR, &table), "Err");
    }

    #[test]
    fn mangle_union() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let union_ty = table.intern(Ty::Union(vec![builtins.int, builtins.string]));
        assert_eq!(mangle_type(union_ty, &table), "U_I64_String");
    }

    #[test]
    fn mangle_ref() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let ref_ty = table.intern(Ty::Ref {
            inner: builtins.int,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        assert_eq!(mangle_type(ref_ty, &table), "Ref_I64");
        let ref_mut = table.intern(Ty::Ref {
            inner: builtins.int,
            mutable: true,
            lifetime: LifetimeId::ANONYMOUS,
        });
        assert_eq!(mangle_type(ref_mut, &table), "RefMut_I64");
    }

    #[test]
    fn mangle_function() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let fn_ty = table.intern(Ty::Function {
            params: vec![builtins.int],
            ret: builtins.bool_ty,
            is_async: false,
        });
        assert_eq!(mangle_type(fn_ty, &table), "Fn_I64_RBool");
    }

    #[test]
    fn mangle_future() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let fut = table.intern(Ty::Future { inner: builtins.int });
        assert_eq!(mangle_type(fut, &table), "Fut_I64");
    }

    #[test]
    fn mangle_channel() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let ch = table.intern(Ty::Channel { inner: builtins.string });
        assert_eq!(mangle_type(ch, &table), "Chan_String");
    }
}
