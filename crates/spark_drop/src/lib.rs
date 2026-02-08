//! Drop elaboration and RC type queries for the Spark compiler.
//!
//! Provides helpers to determine which types require reference-counting
//! operations (heap-allocated types like String, List, Map, Set).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use spark_types::{Ty, TypeId, TypeTable};

/// Returns `true` if values of this type are reference-counted (heap-allocated).
///
/// RC types: `String`, `List(_)`, `Map(_, _)`, `Set(_)`, and `Mutable(inner)`
/// where `inner` is RC'd.
///
/// Non-RC types: all primitives, `Void`, `Null`, `Never`, `Error`, `Ref`,
/// `Function`, `Param`, `Struct`, `Enum`, `Tuple`, `Optional`, `Union`.
#[must_use]
pub fn needs_rc(ty_id: TypeId, table: &TypeTable) -> bool {
    if ty_id.is_error() {
        return false;
    }
    match table.resolve(ty_id) {
        Ty::String | Ty::List(_) | Ty::Map(_, _) | Ty::Set(_) => true,
        Ty::Mutable(inner) => needs_rc(*inner, table),
        _ => false,
    }
}

/// Returns `true` if a collection's elements themselves need RC operations.
///
/// For `List(elem)` / `Set(elem)`: returns `needs_rc(elem)`.
/// For `Map(k, v)`: returns `needs_rc(k) || needs_rc(v)`.
/// For all other types: returns `false`.
#[must_use]
pub fn element_needs_rc(ty_id: TypeId, table: &TypeTable) -> bool {
    if ty_id.is_error() {
        return false;
    }
    let ty = table.resolve(ty_id);
    match ty {
        Ty::List(elem) | Ty::Set(elem) => needs_rc(*elem, table),
        Ty::Map(k, v) => needs_rc(*k, table) || needs_rc(*v, table),
        Ty::Mutable(inner) => element_needs_rc(*inner, table),
        _ => false,
    }
}

/// For `Map(k, v)`, returns `(needs_rc(k), needs_rc(v))`.
/// For other types, returns `(false, false)`.
#[must_use]
pub fn map_key_val_needs_rc(ty_id: TypeId, table: &TypeTable) -> (bool, bool) {
    if ty_id.is_error() {
        return (false, false);
    }
    let ty = table.resolve(ty_id);
    match ty {
        Ty::Map(k, v) => (needs_rc(*k, table), needs_rc(*v, table)),
        Ty::Mutable(inner) => map_key_val_needs_rc(*inner, table),
        _ => (false, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_types::{FloatWidth, IntWidth, Ty};

    #[test]
    fn primitives_not_rc() {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let float = table.intern(Ty::Float(FloatWidth::F64));
        let bool_ty = table.intern(Ty::Bool);
        let char_ty = table.intern(Ty::Char);
        let byte_ty = table.intern(Ty::Byte);
        let void_ty = table.intern(Ty::Void);
        let null_ty = table.intern(Ty::Null);

        assert!(!needs_rc(int, &table));
        assert!(!needs_rc(float, &table));
        assert!(!needs_rc(bool_ty, &table));
        assert!(!needs_rc(char_ty, &table));
        assert!(!needs_rc(byte_ty, &table));
        assert!(!needs_rc(void_ty, &table));
        assert!(!needs_rc(null_ty, &table));
    }

    #[test]
    fn string_is_rc() {
        let mut table = TypeTable::new();
        let string = table.intern(Ty::String);
        assert!(needs_rc(string, &table));
    }

    #[test]
    fn collections_are_rc() {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let string = table.intern(Ty::String);

        let list = table.intern(Ty::List(int));
        let map = table.intern(Ty::Map(string, int));
        let set = table.intern(Ty::Set(int));

        assert!(needs_rc(list, &table));
        assert!(needs_rc(map, &table));
        assert!(needs_rc(set, &table));
    }

    #[test]
    fn mutable_wrapping_rc() {
        let mut table = TypeTable::new();
        let string = table.intern(Ty::String);
        let mut_string = table.intern(Ty::Mutable(string));
        let int = table.intern(Ty::Int(IntWidth::I64));
        let mut_int = table.intern(Ty::Mutable(int));

        assert!(needs_rc(mut_string, &table));
        assert!(!needs_rc(mut_int, &table));
    }

    #[test]
    fn error_type_not_rc() {
        let table = TypeTable::new();
        assert!(!needs_rc(TypeId::ERROR, &table));
    }

    #[test]
    fn element_needs_rc_list_of_string() {
        let mut table = TypeTable::new();
        let string = table.intern(Ty::String);
        let list = table.intern(Ty::List(string));

        assert!(element_needs_rc(list, &table));
    }

    #[test]
    fn element_needs_rc_list_of_int() {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let list = table.intern(Ty::List(int));

        assert!(!element_needs_rc(list, &table));
    }

    #[test]
    fn element_needs_rc_map_with_string_key() {
        let mut table = TypeTable::new();
        let string = table.intern(Ty::String);
        let int = table.intern(Ty::Int(IntWidth::I64));
        let map = table.intern(Ty::Map(string, int));

        assert!(element_needs_rc(map, &table));
    }

    #[test]
    fn map_key_val_rc_flags() {
        let mut table = TypeTable::new();
        let string = table.intern(Ty::String);
        let int = table.intern(Ty::Int(IntWidth::I64));
        let map = table.intern(Ty::Map(string, int));

        let (key_rc, val_rc) = map_key_val_needs_rc(map, &table);
        assert!(key_rc);
        assert!(!val_rc);
    }

    #[test]
    fn non_collection_element_rc_false() {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        assert!(!element_needs_rc(int, &table));
    }

    #[test]
    fn mutable_collection_element_rc() {
        let mut table = TypeTable::new();
        let string = table.intern(Ty::String);
        let list = table.intern(Ty::List(string));
        let mut_list = table.intern(Ty::Mutable(list));

        assert!(element_needs_rc(mut_list, &table));
    }
}
