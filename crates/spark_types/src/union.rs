//! Union type analysis utilities.
//!
//! Provides functions to inspect union types, strip null members, and
//! classify union members as success vs error types. Used by HIR lowering
//! to desugar `??`, `?.`, `!`, and `to` expressions.

use crate::table::TypeTable;
use crate::ty::{Ty, TypeId};

/// Strips `Null` from a union or optional type, returning the non-null members.
///
/// - `Optional(T)` → `T`
/// - `Union([A, B, Null])` → `Union([A, B])` (or just `A` if only one non-null member)
/// - Any other type → unchanged
#[must_use]
pub fn strip_null(ty: TypeId, table: &TypeTable) -> TypeId {
    match table.resolve(ty) {
        Ty::Optional(inner) => *inner,
        Ty::Union(members) => {
            let null_id = table.lookup(&Ty::Null);
            let non_null: Vec<TypeId> = members
                .iter()
                .copied()
                .filter(|&m| Some(m) != null_id)
                .collect();

            match non_null.len() {
                0 => ty, // all null? shouldn't happen, return original
                1 => non_null[0],
                _ => {
                    // We can't intern here (immutable table), but we can check
                    // if this union already exists
                    let candidate = Ty::Union(non_null.clone());
                    table.lookup(&candidate).unwrap_or(ty)
                }
            }
        }
        _ => ty,
    }
}

/// Checks if a type is nullable (contains Null).
///
/// Returns `true` for:
/// - `Ty::Optional(_)`
/// - `Ty::Union([..., Null, ...])`
/// - `Ty::Null`
#[must_use]
pub fn is_nullable(ty: TypeId, table: &TypeTable) -> bool {
    match table.resolve(ty) {
        Ty::Optional(_) | Ty::Null => true,
        Ty::Union(members) => {
            let null_id = table.lookup(&Ty::Null);
            null_id.is_some_and(|nid| members.contains(&nid))
        }
        _ => false,
    }
}

/// Splits a union type into success and error types.
///
/// Given a union type, determines which members are the "success" type
/// and which are "error" types to be propagated.
///
/// **Strategy:**
/// 1. If `expected_ty` is provided and is a member of the union, that's the success type.
///    Everything else is error.
/// 2. If no `expected_ty`, falls back to: members NOT in `fn_ret_ty` are success.
/// 3. If `fn_ret_ty` is a union, error types are those that appear in both.
///
/// Returns `None` if the type is not a union or if classification fails.
#[must_use]
pub fn split_union(
    union_ty: TypeId,
    expected_ty: Option<TypeId>,
    fn_ret_ty: Option<TypeId>,
    table: &TypeTable,
) -> Option<(TypeId, Vec<TypeId>)> {
    let members = match table.resolve(union_ty) {
        Ty::Union(members) => members.clone(),
        Ty::Optional(inner) => {
            // Treat Optional as Union([inner, Null])
            let null_id = table.lookup(&Ty::Null)?;
            vec![*inner, null_id]
        }
        _ => return None,
    };

    // Strategy 1: expected_ty specifies the success type
    if let Some(expected) = expected_ty {
        if !expected.is_error() {
            // Expand expected into its members (handles both single types and unions)
            let expected_members = match table.resolve(expected) {
                Ty::Union(ms) => ms.clone(),
                _ => vec![expected],
            };

            // All expected members must be present in the source union
            if expected_members.iter().all(|em| members.contains(em)) {
                let errors: Vec<TypeId> = members
                    .iter()
                    .copied()
                    .filter(|m| !expected_members.contains(m))
                    .collect();
                if !errors.is_empty() {
                    return Some((expected, errors));
                }
            }
        }
    }

    // Strategy 2: fn_ret_ty tells us which types should be propagated (errors)
    if let Some(ret) = fn_ret_ty {
        let ret_members = match table.resolve(ret) {
            Ty::Union(ms) => ms.clone(),
            Ty::Optional(inner) => {
                if let Some(null_id) = table.lookup(&Ty::Null) {
                    vec![*inner, null_id]
                } else {
                    vec![*inner]
                }
            }
            _ => vec![ret],
        };

        let errors: Vec<TypeId> = members
            .iter()
            .copied()
            .filter(|m| ret_members.contains(m))
            .collect();
        let success: Vec<TypeId> = members
            .iter()
            .copied()
            .filter(|m| !ret_members.contains(m))
            .collect();

        if success.len() == 1 && !errors.is_empty() {
            return Some((success[0], errors));
        }
    }

    None
}

/// Returns the flattened members of a union type.
///
/// For `Union([A, B, C])` returns `[A, B, C]`.
/// For `Optional(T)` returns `[T, Null]`.
/// For any other type returns `[ty]`.
#[must_use]
pub fn union_members(ty: TypeId, table: &TypeTable) -> Vec<TypeId> {
    match table.resolve(ty) {
        Ty::Union(members) => members.clone(),
        Ty::Optional(inner) => {
            let mut members = vec![*inner];
            if let Some(null_id) = table.lookup(&Ty::Null) {
                members.push(null_id);
            }
            members
        }
        _ => vec![ty],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::register_builtins;

    #[test]
    fn strip_null_from_optional() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let opt_int = table.intern(Ty::Optional(builtins.int));
        assert_eq!(strip_null(opt_int, &table), builtins.int);
    }

    #[test]
    fn strip_null_from_union_with_null() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let null_id = builtins.null;
        let mut members = vec![builtins.int, null_id];
        members.sort();
        let union_ty = table.intern(Ty::Union(members));
        let stripped = strip_null(union_ty, &table);
        // Should return just Int
        assert_eq!(stripped, builtins.int);
    }

    #[test]
    fn strip_null_non_nullable() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        assert_eq!(strip_null(builtins.int, &table), builtins.int);
    }

    #[test]
    fn is_nullable_optional() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let opt_int = table.intern(Ty::Optional(builtins.int));
        assert!(is_nullable(opt_int, &table));
    }

    #[test]
    fn is_nullable_union_with_null() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let null_id = builtins.null;
        let mut members = vec![builtins.int, null_id];
        members.sort();
        let union_ty = table.intern(Ty::Union(members));
        assert!(is_nullable(union_ty, &table));
    }

    #[test]
    fn is_nullable_plain_type() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        assert!(!is_nullable(builtins.int, &table));
    }

    #[test]
    fn split_union_with_expected() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let mut members = vec![builtins.int, builtins.string, builtins.null];
        members.sort();
        let union_ty = table.intern(Ty::Union(members));

        let result = split_union(union_ty, Some(builtins.int), None, &table);
        assert!(result.is_some());
        let (success, errors) = result.unwrap();
        assert_eq!(success, builtins.int);
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn split_union_with_fn_ret() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        // Union: Int | String | Bool
        let mut members = vec![builtins.int, builtins.string, builtins.bool_ty];
        members.sort();
        let union_ty = table.intern(Ty::Union(members));

        // fn ret type is String | Bool (the errors to propagate)
        let mut ret_members = vec![builtins.string, builtins.bool_ty];
        ret_members.sort();
        let ret_ty = table.intern(Ty::Union(ret_members));

        let result = split_union(union_ty, None, Some(ret_ty), &table);
        assert!(result.is_some());
        let (success, errors) = result.unwrap();
        assert_eq!(success, builtins.int);
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn split_non_union_returns_none() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let result = split_union(builtins.int, None, None, &table);
        assert!(result.is_none());
    }

    #[test]
    fn union_members_union() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let mut members = vec![builtins.int, builtins.string];
        members.sort();
        let union_ty = table.intern(Ty::Union(members.clone()));
        assert_eq!(union_members(union_ty, &table), members);
    }

    #[test]
    fn union_members_optional() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let opt = table.intern(Ty::Optional(builtins.int));
        let members = union_members(opt, &table);
        assert_eq!(members.len(), 2);
        assert!(members.contains(&builtins.int));
        assert!(members.contains(&builtins.null));
    }

    #[test]
    fn union_members_plain() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        assert_eq!(union_members(builtins.int, &table), vec![builtins.int]);
    }
}
