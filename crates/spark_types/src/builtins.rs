//! Built-in type registration.
//!
//! This module pre-interns all primitive types (Int, Float, Bool, etc.)
//! and provides a [`BuiltinTypes`] struct for quick access to their `TypeId`s.

use rustc_hash::FxHashMap;
use spark_intern::{kw, Symbol};

use crate::table::TypeTable;
use crate::ty::{FloatWidth, IntWidth, Ty, TypeId};

/// Pre-interned `TypeId`s for all built-in types.
///
/// Created by [`register_builtins`] during type checker initialization.
#[derive(Debug, Clone)]
pub struct BuiltinTypes {
    /// `Int` (alias for `Int64`).
    pub int: TypeId,
    /// `Int8`.
    pub int8: TypeId,
    /// `Int16`.
    pub int16: TypeId,
    /// `Int32`.
    pub int32: TypeId,
    /// `Int64`.
    pub int64: TypeId,
    /// `UInt` (alias for `UInt64`).
    pub uint: TypeId,
    /// `UInt8`.
    pub uint8: TypeId,
    /// `UInt16`.
    pub uint16: TypeId,
    /// `UInt32`.
    pub uint32: TypeId,
    /// `UInt64`.
    pub uint64: TypeId,
    /// `Float` (alias for `Float64`).
    pub float: TypeId,
    /// `Float32`.
    pub float32: TypeId,
    /// `Float64`.
    pub float64: TypeId,
    /// `Bool`.
    pub bool_ty: TypeId,
    /// `Char`.
    pub char_ty: TypeId,
    /// `String`.
    pub string: TypeId,
    /// `Byte`.
    pub byte: TypeId,
    /// `Void`.
    pub void: TypeId,
    /// `Null`.
    pub null: TypeId,
    /// `Never`.
    pub never: TypeId,

    /// Maps built-in type name symbols to their `TypeId`s for quick lookup.
    symbol_map: FxHashMap<Symbol, TypeId>,
}

impl BuiltinTypes {
    /// Looks up a built-in type by its symbol name.
    ///
    /// Returns `None` if the symbol is not a built-in type.
    #[must_use]
    pub fn lookup(&self, sym: Symbol) -> Option<TypeId> {
        self.symbol_map.get(&sym).copied()
    }
}

/// Registers all built-in primitive types in the type table.
///
/// Returns a [`BuiltinTypes`] struct containing `TypeId`s for every
/// built-in type, plus a symbol-to-type lookup map.
pub fn register_builtins(table: &mut TypeTable) -> BuiltinTypes {
    let int8 = table.intern(Ty::Int(IntWidth::I8));
    let int16 = table.intern(Ty::Int(IntWidth::I16));
    let int32 = table.intern(Ty::Int(IntWidth::I32));
    let int64 = table.intern(Ty::Int(IntWidth::I64));
    let uint8 = table.intern(Ty::Int(IntWidth::U8));
    let uint16 = table.intern(Ty::Int(IntWidth::U16));
    let uint32 = table.intern(Ty::Int(IntWidth::U32));
    let uint64 = table.intern(Ty::Int(IntWidth::U64));
    let float32 = table.intern(Ty::Float(FloatWidth::F32));
    let float64 = table.intern(Ty::Float(FloatWidth::F64));
    let bool_ty = table.intern(Ty::Bool);
    let char_ty = table.intern(Ty::Char);
    let string = table.intern(Ty::String);
    let byte = table.intern(Ty::Byte);
    let void = table.intern(Ty::Void);
    let null = table.intern(Ty::Null);
    let never = table.intern(Ty::Never);

    // Int = Int64, UInt = UInt64, Float = Float64
    let int = int64;
    let uint = uint64;
    let float = float64;

    let mut symbol_map = FxHashMap::default();
    symbol_map.insert(kw::INT, int);
    symbol_map.insert(kw::INT8, int8);
    symbol_map.insert(kw::INT16, int16);
    symbol_map.insert(kw::INT32, int32);
    symbol_map.insert(kw::INT64, int64);
    symbol_map.insert(kw::UINT, uint);
    symbol_map.insert(kw::UINT8, uint8);
    symbol_map.insert(kw::UINT16, uint16);
    symbol_map.insert(kw::UINT32, uint32);
    symbol_map.insert(kw::UINT64, uint64);
    symbol_map.insert(kw::FLOAT, float);
    symbol_map.insert(kw::FLOAT32, float32);
    symbol_map.insert(kw::FLOAT64, float64);
    symbol_map.insert(kw::BOOL, bool_ty);
    symbol_map.insert(kw::CHAR, char_ty);
    symbol_map.insert(kw::STRING, string);
    symbol_map.insert(kw::BYTE, byte);
    symbol_map.insert(kw::VOID, void);
    symbol_map.insert(kw::NULL, null);
    symbol_map.insert(kw::NEVER, never);

    BuiltinTypes {
        int,
        int8,
        int16,
        int32,
        int64,
        uint,
        uint8,
        uint16,
        uint32,
        uint64,
        float,
        float32,
        float64,
        bool_ty,
        char_ty,
        string,
        byte,
        void,
        null,
        never,
        symbol_map,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_all_builtins() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);

        // Check that all builtins are non-error
        assert!(!builtins.int.is_error());
        assert!(!builtins.bool_ty.is_error());
        assert!(!builtins.string.is_error());
        assert!(!builtins.void.is_error());
        assert!(!builtins.null.is_error());
        assert!(!builtins.never.is_error());
    }

    #[test]
    fn int_is_int64() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        assert_eq!(builtins.int, builtins.int64);
    }

    #[test]
    fn float_is_float64() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        assert_eq!(builtins.float, builtins.float64);
    }

    #[test]
    fn lookup_by_symbol() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);

        assert_eq!(builtins.lookup(kw::INT), Some(builtins.int));
        assert_eq!(builtins.lookup(kw::BOOL), Some(builtins.bool_ty));
        assert_eq!(builtins.lookup(kw::STRING), Some(builtins.string));
        assert_eq!(builtins.lookup(kw::VOID), Some(builtins.void));
        // Non-builtin should return None
        assert_eq!(builtins.lookup(spark_intern::sym("MyType")), None);
    }

    #[test]
    fn builtins_resolve_correctly() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);

        assert_eq!(table.resolve(builtins.bool_ty), &Ty::Bool);
        assert_eq!(table.resolve(builtins.string), &Ty::String);
        assert_eq!(table.resolve(builtins.void), &Ty::Void);
        assert_eq!(table.resolve(builtins.int), &Ty::Int(IntWidth::I64));
    }
}
