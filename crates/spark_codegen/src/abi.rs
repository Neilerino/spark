//! ABI mapping from Spark types to Cranelift IR types.
//!
//! Maps each Spark type to its Cranelift representation for function signatures
//! and value operations.

use cranelift_codegen::ir::types;
use cranelift_codegen::ir::Type as ClifType;
use spark_types::{FloatWidth, IntWidth, Ty, TypeId, TypeTable};

/// Maps a Spark [`TypeId`] to its Cranelift IR type.
///
/// Returns `None` for zero-sized types (`Void`, `Null`, `Never`, `Error`)
/// that have no runtime representation.
#[must_use]
pub fn spark_type_to_clif(ty_id: TypeId, table: &TypeTable, ptr_type: ClifType) -> Option<ClifType> {
    let ty = table.resolve(ty_id);
    match ty {
        // Integer types
        Ty::Int(IntWidth::I8 | IntWidth::U8) | Ty::Bool | Ty::Byte => Some(types::I8),
        Ty::Int(IntWidth::I16 | IntWidth::U16) => Some(types::I16),
        Ty::Int(IntWidth::I32 | IntWidth::U32) | Ty::Char => Some(types::I32),
        Ty::Int(IntWidth::I64 | IntWidth::U64) => Some(types::I64),

        // Float types
        Ty::Float(FloatWidth::F32) => Some(types::F32),
        Ty::Float(FloatWidth::F64) => Some(types::F64),

        // Zero-sized types — no runtime value
        Ty::Void | Ty::Null | Ty::Never | Ty::Error => None,

        // Pointer-sized: heap objects, references, function pointers, aggregates
        // (aggregates are passed by pointer), and generic parameters
        Ty::String
        | Ty::List(_)
        | Ty::Map(_, _)
        | Ty::Set(_)
        | Ty::Ref { .. }
        | Ty::Function { .. }
        | Ty::Struct(_)
        | Ty::Enum(_)
        | Ty::Tuple(_)
        | Ty::Optional(_)
        | Ty::Union(_)
        | Ty::Param(_)
        | Ty::Applied { .. } => Some(ptr_type),

        // Future[T] is an opaque heap pointer (SparkFuture*)
        Ty::Future { .. } => Some(ptr_type),

        // Async state struct — always an aggregate, passed by pointer
        Ty::AsyncState { .. } => Some(ptr_type),

        // Mutable wrapping is transparent at runtime
        Ty::Mutable(inner) => spark_type_to_clif(*inner, table, ptr_type),
    }
}

/// Returns true if a Spark type is zero-sized at runtime.
#[must_use]
pub fn is_zero_sized(ty_id: TypeId, table: &TypeTable) -> bool {
    matches!(
        table.resolve(ty_id),
        Ty::Void | Ty::Null | Ty::Never | Ty::Error
    )
}

/// Returns true if a Spark type is a scalar (fits in a register, not an aggregate).
#[must_use]
pub fn is_scalar(ty_id: TypeId, table: &TypeTable) -> bool {
    let ty = table.resolve(ty_id);
    matches!(
        ty,
        Ty::Int(_)
            | Ty::Float(_)
            | Ty::Bool
            | Ty::Char
            | Ty::Byte
            | Ty::String
            | Ty::List(_)
            | Ty::Map(_, _)
            | Ty::Set(_)
            | Ty::Ref { .. }
    ) || matches!(ty, Ty::Future { .. })
      || matches!(ty, Ty::Mutable(inner) if is_scalar(*inner, table))
}

/// Returns true if this type is an aggregate that needs sret (struct return pointer)
/// when returned from a function.
///
/// Aggregate return types cannot be returned in a register — the caller must
/// pass a pointer to a buffer where the callee writes the return value.
#[must_use]
pub fn needs_sret(ty_id: TypeId, table: &TypeTable) -> bool {
    !is_zero_sized(ty_id, table) && !is_scalar(ty_id, table)
}

/// Returns true if a type is signed for integer comparisons/division.
#[must_use]
pub const fn int_is_signed(w: IntWidth) -> bool {
    matches!(w, IntWidth::I8 | IntWidth::I16 | IntWidth::I32 | IntWidth::I64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::types;
    use spark_types::Ty;

    #[test]
    fn integer_mappings() {
        let mut table = TypeTable::new();
        let ptr = types::I64;

        let i8_ty = table.intern(Ty::Int(IntWidth::I8));
        let i16_ty = table.intern(Ty::Int(IntWidth::I16));
        let i32_ty = table.intern(Ty::Int(IntWidth::I32));
        let i64_ty = table.intern(Ty::Int(IntWidth::I64));
        let u8_ty = table.intern(Ty::Int(IntWidth::U8));

        assert_eq!(spark_type_to_clif(i8_ty, &table, ptr), Some(types::I8));
        assert_eq!(spark_type_to_clif(i16_ty, &table, ptr), Some(types::I16));
        assert_eq!(spark_type_to_clif(i32_ty, &table, ptr), Some(types::I32));
        assert_eq!(spark_type_to_clif(i64_ty, &table, ptr), Some(types::I64));
        assert_eq!(spark_type_to_clif(u8_ty, &table, ptr), Some(types::I8));
    }

    #[test]
    fn float_mappings() {
        let mut table = TypeTable::new();
        let ptr = types::I64;

        let f32_ty = table.intern(Ty::Float(FloatWidth::F32));
        let f64_ty = table.intern(Ty::Float(FloatWidth::F64));

        assert_eq!(spark_type_to_clif(f32_ty, &table, ptr), Some(types::F32));
        assert_eq!(spark_type_to_clif(f64_ty, &table, ptr), Some(types::F64));
    }

    #[test]
    fn zero_sized_returns_none() {
        let mut table = TypeTable::new();
        let ptr = types::I64;

        let void_ty = table.intern(Ty::Void);
        let null_ty = table.intern(Ty::Null);
        let never_ty = table.intern(Ty::Never);

        assert_eq!(spark_type_to_clif(void_ty, &table, ptr), None);
        assert_eq!(spark_type_to_clif(null_ty, &table, ptr), None);
        assert_eq!(spark_type_to_clif(never_ty, &table, ptr), None);
    }

    #[test]
    fn pointer_types() {
        let mut table = TypeTable::new();
        let ptr = types::I64;

        let string_ty = table.intern(Ty::String);
        let int_ty = table.intern(Ty::Int(IntWidth::I64));
        let list_ty = table.intern(Ty::List(int_ty));

        assert_eq!(spark_type_to_clif(string_ty, &table, ptr), Some(types::I64));
        assert_eq!(spark_type_to_clif(list_ty, &table, ptr), Some(types::I64));
    }

    #[test]
    fn bool_and_byte() {
        let mut table = TypeTable::new();
        let ptr = types::I64;

        let bool_ty = table.intern(Ty::Bool);
        let byte_ty = table.intern(Ty::Byte);

        assert_eq!(spark_type_to_clif(bool_ty, &table, ptr), Some(types::I8));
        assert_eq!(spark_type_to_clif(byte_ty, &table, ptr), Some(types::I8));
    }

    #[test]
    fn mutable_transparent() {
        let mut table = TypeTable::new();
        let ptr = types::I64;

        let i32_ty = table.intern(Ty::Int(IntWidth::I32));
        let mut_i32 = table.intern(Ty::Mutable(i32_ty));

        assert_eq!(spark_type_to_clif(mut_i32, &table, ptr), Some(types::I32));
    }

    #[test]
    fn signed_unsigned_check() {
        assert!(int_is_signed(IntWidth::I8));
        assert!(int_is_signed(IntWidth::I64));
        assert!(!int_is_signed(IntWidth::U8));
        assert!(!int_is_signed(IntWidth::U64));
    }
}
