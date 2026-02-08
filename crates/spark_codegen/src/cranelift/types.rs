//! Cranelift-specific type utilities.
//!
//! Helpers for working with Cranelift types in the context of Spark code
//! generation.

use cranelift_codegen::ir::{types, AbiParam, Signature, Type as ClifType};
use cranelift_codegen::isa::CallConv;
use spark_types::{Ty, TypeId, TypeTable};

use crate::abi;

/// Builds a Cranelift function [`Signature`] from a Spark function's parameter
/// and return types.
#[must_use]
pub fn build_signature(
    param_types: &[TypeId],
    return_type: TypeId,
    table: &TypeTable,
    ptr_type: ClifType,
    call_conv: CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);

    for &param_ty in param_types {
        if let Some(clif_ty) = abi::spark_type_to_clif(param_ty, table, ptr_type) {
            sig.params.push(AbiParam::new(clif_ty));
        }
    }

    if let Some(ret_ty) = abi::spark_type_to_clif(return_type, table, ptr_type) {
        sig.returns.push(AbiParam::new(ret_ty));
    }

    sig
}

/// Returns the Cranelift type for a MIR local, or `None` if zero-sized.
#[must_use]
pub fn local_clif_type(
    ty_id: TypeId,
    table: &TypeTable,
    ptr_type: ClifType,
) -> Option<ClifType> {
    abi::spark_type_to_clif(ty_id, table, ptr_type)
}

/// Returns the integer Cranelift type for a given byte count.
///
/// Used for loading/storing fields and discriminants of known sizes.
#[must_use]
pub const fn int_type_for_bytes(bytes: u32) -> ClifType {
    match bytes {
        1 => types::I8,
        2 => types::I16,
        4 => types::I32,
        _ => types::I64,
    }
}

/// Returns the Cranelift integer type for a Spark integer width.
#[must_use]
pub const fn int_width_to_clif(w: spark_types::IntWidth) -> ClifType {
    use spark_types::IntWidth;
    match w {
        IntWidth::I8 | IntWidth::U8 => types::I8,
        IntWidth::I16 | IntWidth::U16 => types::I16,
        IntWidth::I32 | IntWidth::U32 => types::I32,
        IntWidth::I64 | IntWidth::U64 => types::I64,
    }
}

/// Returns the byte size for an integer width.
#[must_use]
pub const fn int_width_bytes(w: spark_types::IntWidth) -> u32 {
    use spark_types::IntWidth;
    match w {
        IntWidth::I8 | IntWidth::U8 => 1,
        IntWidth::I16 | IntWidth::U16 => 2,
        IntWidth::I32 | IntWidth::U32 => 4,
        IntWidth::I64 | IntWidth::U64 => 8,
    }
}

/// Resolves `Mutable(T)` wrappers to get the underlying type.
#[must_use]
pub fn unwrap_mutable(ty_id: TypeId, table: &TypeTable) -> (TypeId, &Ty) {
    let ty = table.resolve(ty_id);
    match ty {
        Ty::Mutable(inner) => unwrap_mutable(*inner, table),
        _ => (ty_id, ty),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::types;
    use spark_types::{IntWidth, Ty};

    #[test]
    fn build_signature_simple() {
        let mut table = TypeTable::new();
        let i64_ty = table.intern(Ty::Int(IntWidth::I64));
        let i32_ty = table.intern(Ty::Int(IntWidth::I32));

        let sig = build_signature(
            &[i64_ty, i32_ty],
            i64_ty,
            &table,
            types::I64,
            CallConv::SystemV,
        );

        assert_eq!(sig.params.len(), 2);
        assert_eq!(sig.params[0].value_type, types::I64);
        assert_eq!(sig.params[1].value_type, types::I32);
        assert_eq!(sig.returns.len(), 1);
        assert_eq!(sig.returns[0].value_type, types::I64);
    }

    #[test]
    fn build_signature_void_return() {
        let mut table = TypeTable::new();
        let void_ty = table.intern(Ty::Void);
        let i32_ty = table.intern(Ty::Int(IntWidth::I32));

        let sig = build_signature(
            &[i32_ty],
            void_ty,
            &table,
            types::I64,
            CallConv::SystemV,
        );

        assert_eq!(sig.params.len(), 1);
        assert!(sig.returns.is_empty());
    }

    #[test]
    fn int_type_for_common_sizes() {
        assert_eq!(int_type_for_bytes(1), types::I8);
        assert_eq!(int_type_for_bytes(2), types::I16);
        assert_eq!(int_type_for_bytes(4), types::I32);
        assert_eq!(int_type_for_bytes(8), types::I64);
    }

    #[test]
    fn unwrap_mutable_strips() {
        let mut table = TypeTable::new();
        let i32_ty = table.intern(Ty::Int(IntWidth::I32));
        let mut_ty = table.intern(Ty::Mutable(i32_ty));
        let mut_mut_ty = table.intern(Ty::Mutable(mut_ty));

        let (resolved, ty) = unwrap_mutable(mut_mut_ty, &table);
        assert_eq!(resolved, i32_ty);
        assert!(matches!(ty, Ty::Int(IntWidth::I32)));
    }
}
