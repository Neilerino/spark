//! Type substitution for generic instantiation.
//!
//! Provides utilities to replace `Ty::Param` with concrete types during
//! generic type checking and monomorphization.

use rustc_hash::FxHashMap;
use spark_resolve::DefId;

use crate::table::TypeTable;
use crate::ty::{Ty, TypeId};

/// A mapping from generic parameter `DefId`s to concrete `TypeId`s.
pub type SubstMap = FxHashMap<DefId, TypeId>;

/// Builds a substitution map from parallel slices of param `DefId`s and arg `TypeId`s.
///
/// If lengths differ, only the minimum-length prefix is mapped.
#[must_use]
pub fn build_subst_map(params: &[DefId], args: &[TypeId]) -> SubstMap {
    params
        .iter()
        .zip(args.iter())
        .map(|(&p, &a)| (p, a))
        .collect()
}

/// Substitutes all `Ty::Param(def_id)` in `ty_id` with concrete types from `subst`.
///
/// Recursively walks compound types. Types not containing `Ty::Param` pass through unchanged.
///
/// Note: takes `&TypeTable` (immutable) and returns the substituted type.
/// The caller is responsible for interning the result if needed.
#[must_use]
pub fn substitute(ty_id: TypeId, subst: &SubstMap, table: &TypeTable) -> TypeId {
    if ty_id.is_error() || subst.is_empty() {
        return ty_id;
    }

    // We need to intern new types, but we only have an immutable reference.
    // Instead, check if substitution would change anything and return the
    // original if not. For actual substitution, we clone and intern.
    substitute_inner(ty_id, subst, table)
}

/// Inner substitution — works with an immutable table by returning `TypeId`s
/// that may need interning. We use `lookup` for types that already exist,
/// and for new types we must accept that we need a mutable table.
///
/// Since the caller (`check_field_access`, `check_struct_init`, `check_call`)
/// always has `&mut TypeTable`, we provide a mutable variant too.
#[allow(clippy::too_many_lines)]
fn substitute_inner(ty_id: TypeId, subst: &SubstMap, table: &TypeTable) -> TypeId {
    let ty = table.resolve(ty_id);
    match ty {
        Ty::Param(def_id) => subst.get(def_id).copied().unwrap_or(ty_id),

        Ty::List(inner) => {
            let new_inner = substitute_inner(*inner, subst, table);
            if new_inner == *inner {
                ty_id
            } else {
                table.lookup(&Ty::List(new_inner)).unwrap_or(ty_id)
            }
        }
        Ty::Set(inner) => {
            let new_inner = substitute_inner(*inner, subst, table);
            if new_inner == *inner {
                ty_id
            } else {
                table.lookup(&Ty::Set(new_inner)).unwrap_or(ty_id)
            }
        }
        Ty::Map(k, v) => {
            let new_k = substitute_inner(*k, subst, table);
            let new_v = substitute_inner(*v, subst, table);
            if new_k == *k && new_v == *v {
                ty_id
            } else {
                table.lookup(&Ty::Map(new_k, new_v)).unwrap_or(ty_id)
            }
        }
        Ty::Optional(inner) => {
            let new_inner = substitute_inner(*inner, subst, table);
            if new_inner == *inner {
                ty_id
            } else {
                table.lookup(&Ty::Optional(new_inner)).unwrap_or(ty_id)
            }
        }
        Ty::Mutable(inner) => {
            let new_inner = substitute_inner(*inner, subst, table);
            if new_inner == *inner {
                ty_id
            } else {
                table.lookup(&Ty::Mutable(new_inner)).unwrap_or(ty_id)
            }
        }
        Ty::Ref {
            inner,
            mutable,
            lifetime,
        } => {
            let new_inner = substitute_inner(*inner, subst, table);
            if new_inner == *inner {
                ty_id
            } else {
                table
                    .lookup(&Ty::Ref {
                        inner: new_inner,
                        mutable: *mutable,
                        lifetime: *lifetime,
                    })
                    .unwrap_or(ty_id)
            }
        }
        Ty::Tuple(elems) => {
            let new_elems: Vec<TypeId> = elems
                .iter()
                .map(|e| substitute_inner(*e, subst, table))
                .collect();
            if new_elems == *elems {
                ty_id
            } else {
                table.lookup(&Ty::Tuple(new_elems)).unwrap_or(ty_id)
            }
        }
        Ty::Union(members) => {
            let new_members: Vec<TypeId> = members
                .iter()
                .map(|m| substitute_inner(*m, subst, table))
                .collect();
            if new_members == *members {
                ty_id
            } else {
                table.lookup(&Ty::Union(new_members)).unwrap_or(ty_id)
            }
        }
        Ty::Function {
            params,
            ret,
            is_async,
        } => {
            let new_params: Vec<TypeId> = params
                .iter()
                .map(|p| substitute_inner(*p, subst, table))
                .collect();
            let new_ret = substitute_inner(*ret, subst, table);
            if new_params == *params && new_ret == *ret {
                ty_id
            } else {
                table
                    .lookup(&Ty::Function {
                        params: new_params,
                        ret: new_ret,
                        is_async: *is_async,
                    })
                    .unwrap_or(ty_id)
            }
        }
        Ty::Applied { base, args } => {
            let new_args: Vec<TypeId> = args
                .iter()
                .map(|a| substitute_inner(*a, subst, table))
                .collect();
            if new_args == *args {
                ty_id
            } else {
                table
                    .lookup(&Ty::Applied {
                        base: *base,
                        args: new_args,
                    })
                    .unwrap_or(ty_id)
            }
        }

        Ty::Future { inner } => {
            let new_inner = substitute_inner(*inner, subst, table);
            if new_inner == *inner {
                ty_id
            } else {
                table
                    .lookup(&Ty::Future { inner: new_inner })
                    .unwrap_or(ty_id)
            }
        }

        Ty::Channel { inner } => {
            let new_inner = substitute_inner(*inner, subst, table);
            if new_inner == *inner {
                ty_id
            } else {
                table
                    .lookup(&Ty::Channel { inner: new_inner })
                    .unwrap_or(ty_id)
            }
        }

        // Primitives and other leaves pass through unchanged.
        Ty::Int(_)
        | Ty::Float(_)
        | Ty::Bool
        | Ty::Char
        | Ty::String
        | Ty::Byte
        | Ty::CPtr
        | Ty::Void
        | Ty::Null
        | Ty::Never
        | Ty::Struct(_)
        | Ty::Enum(_)
        | Ty::AsyncState { .. }
        | Ty::Error => ty_id,
    }
}

/// Substitutes all `Ty::Param(def_id)` in `ty_id`, interning new types as needed.
///
/// This is the primary substitution entry point used during type checking
/// where a mutable type table is available.
pub fn substitute_mut(ty_id: TypeId, subst: &SubstMap, table: &mut TypeTable) -> TypeId {
    if ty_id.is_error() || subst.is_empty() {
        return ty_id;
    }
    substitute_mut_inner(ty_id, subst, table)
}

/// Inner mutable substitution that interns new types.
fn substitute_mut_inner(ty_id: TypeId, subst: &SubstMap, table: &mut TypeTable) -> TypeId {
    let ty = table.resolve(ty_id).clone();
    match ty {
        Ty::Param(def_id) => subst.get(&def_id).copied().unwrap_or(ty_id),

        Ty::List(inner) => {
            let new_inner = substitute_mut_inner(inner, subst, table);
            if new_inner == inner {
                ty_id
            } else {
                table.intern(Ty::List(new_inner))
            }
        }
        Ty::Set(inner) => {
            let new_inner = substitute_mut_inner(inner, subst, table);
            if new_inner == inner {
                ty_id
            } else {
                table.intern(Ty::Set(new_inner))
            }
        }
        Ty::Map(k, v) => {
            let new_k = substitute_mut_inner(k, subst, table);
            let new_v = substitute_mut_inner(v, subst, table);
            if new_k == k && new_v == v {
                ty_id
            } else {
                table.intern(Ty::Map(new_k, new_v))
            }
        }
        Ty::Optional(inner) => {
            let new_inner = substitute_mut_inner(inner, subst, table);
            if new_inner == inner {
                ty_id
            } else {
                table.intern(Ty::Optional(new_inner))
            }
        }
        Ty::Mutable(inner) => {
            let new_inner = substitute_mut_inner(inner, subst, table);
            if new_inner == inner {
                ty_id
            } else {
                table.intern(Ty::Mutable(new_inner))
            }
        }
        Ty::Ref {
            inner,
            mutable,
            lifetime,
        } => {
            let new_inner = substitute_mut_inner(inner, subst, table);
            if new_inner == inner {
                ty_id
            } else {
                table.intern(Ty::Ref {
                    inner: new_inner,
                    mutable,
                    lifetime,
                })
            }
        }
        Ty::Tuple(elems) => {
            let new_elems: Vec<TypeId> = elems
                .iter()
                .map(|e| substitute_mut_inner(*e, subst, table))
                .collect();
            if new_elems == elems {
                ty_id
            } else {
                table.intern(Ty::Tuple(new_elems))
            }
        }
        Ty::Union(members) => {
            let new_members: Vec<TypeId> = members
                .iter()
                .map(|m| substitute_mut_inner(*m, subst, table))
                .collect();
            if new_members == members {
                ty_id
            } else {
                table.intern(Ty::Union(new_members))
            }
        }
        Ty::Function {
            params,
            ret,
            is_async,
        } => {
            let new_params: Vec<TypeId> = params
                .iter()
                .map(|p| substitute_mut_inner(*p, subst, table))
                .collect();
            let new_ret = substitute_mut_inner(ret, subst, table);
            if new_params == params && new_ret == ret {
                ty_id
            } else {
                table.intern(Ty::Function {
                    params: new_params,
                    ret: new_ret,
                    is_async,
                })
            }
        }
        Ty::Applied { base, args } => {
            let new_args: Vec<TypeId> = args
                .iter()
                .map(|a| substitute_mut_inner(*a, subst, table))
                .collect();
            if new_args == args {
                ty_id
            } else {
                table.intern(Ty::Applied {
                    base,
                    args: new_args,
                })
            }
        }

        Ty::Future { inner } => {
            let new_inner = substitute_mut_inner(inner, subst, table);
            if new_inner == inner {
                ty_id
            } else {
                table.intern(Ty::Future { inner: new_inner })
            }
        }

        Ty::Channel { inner } => {
            let new_inner = substitute_mut_inner(inner, subst, table);
            if new_inner == inner {
                ty_id
            } else {
                table.intern(Ty::Channel { inner: new_inner })
            }
        }

        Ty::Int(_)
        | Ty::Float(_)
        | Ty::Bool
        | Ty::Char
        | Ty::String
        | Ty::Byte
        | Ty::CPtr
        | Ty::Void
        | Ty::Null
        | Ty::Never
        | Ty::Struct(_)
        | Ty::Enum(_)
        | Ty::AsyncState { .. }
        | Ty::Error => ty_id,
    }
}

/// Attempts to infer type arguments by unifying declared parameter types
/// against concrete argument types.
///
/// Walks `declared_params` and `arg_types` pairwise, recursively matching
/// structure to extract `Ty::Param → concrete` mappings.
///
/// Returns `None` if not all params could be inferred.
#[must_use]
pub fn infer_type_args(
    generic_params: &[DefId],
    declared_params: &[TypeId],
    arg_types: &[TypeId],
    table: &TypeTable,
) -> Option<SubstMap> {
    let mut subst = SubstMap::default();

    for (&declared, &actual) in declared_params.iter().zip(arg_types.iter()) {
        unify(declared, actual, &mut subst, table);
    }

    // Check that all generic params were inferred
    if generic_params.iter().all(|p| subst.contains_key(p)) {
        Some(subst)
    } else {
        None
    }
}

/// Recursively unifies a declared type (which may contain `Ty::Param`) against
/// a concrete type, recording inferred mappings in `subst`.
fn unify(declared: TypeId, actual: TypeId, subst: &mut SubstMap, table: &TypeTable) {
    if declared.is_error() || actual.is_error() {
        return;
    }

    let decl_ty = table.resolve(declared);
    match decl_ty {
        Ty::Param(def_id) => {
            // If already mapped, check consistency (but don't fail — let type checking catch it)
            subst.entry(*def_id).or_insert(actual);
        }
        Ty::List(inner) => {
            if let Ty::List(actual_inner) = table.resolve(actual) {
                unify(*inner, *actual_inner, subst, table);
            }
        }
        Ty::Set(inner) => {
            if let Ty::Set(actual_inner) = table.resolve(actual) {
                unify(*inner, *actual_inner, subst, table);
            }
        }
        Ty::Map(k, v) => {
            if let Ty::Map(ak, av) = table.resolve(actual) {
                unify(*k, *ak, subst, table);
                unify(*v, *av, subst, table);
            }
        }
        Ty::Optional(inner) => {
            if let Ty::Optional(actual_inner) = table.resolve(actual) {
                unify(*inner, *actual_inner, subst, table);
            }
        }
        Ty::Tuple(elems) => {
            if let Ty::Tuple(actual_elems) = table.resolve(actual) {
                for (e, ae) in elems.iter().zip(actual_elems.iter()) {
                    unify(*e, *ae, subst, table);
                }
            }
        }
        Ty::Function { params, ret, .. } => {
            if let Ty::Function {
                params: ap,
                ret: ar,
                ..
            } = table.resolve(actual)
            {
                for (p, ap_item) in params.iter().zip(ap.iter()) {
                    unify(*p, *ap_item, subst, table);
                }
                unify(*ret, *ar, subst, table);
            }
        }
        Ty::Applied { base, args } => {
            if let Ty::Applied { base: ab, args: aa } = table.resolve(actual) {
                if base == ab {
                    for (a, actual_a) in args.iter().zip(aa.iter()) {
                        unify(*a, *actual_a, subst, table);
                    }
                }
            }
        }
        Ty::Future { inner } => {
            if let Ty::Future {
                inner: actual_inner,
            } = table.resolve(actual)
            {
                unify(*inner, *actual_inner, subst, table);
            }
        }
        Ty::Channel { inner } => {
            if let Ty::Channel {
                inner: actual_inner,
            } = table.resolve(actual)
            {
                unify(*inner, *actual_inner, subst, table);
            }
        }
        Ty::Mutable(inner) => {
            // Strip mutable wrapper on both sides
            let actual_inner = match table.resolve(actual) {
                Ty::Mutable(ai) => *ai,
                _ => actual,
            };
            unify(*inner, actual_inner, subst, table);
        }
        // Leaf types don't contribute to inference
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::register_builtins;

    #[test]
    fn build_subst_map_basic() {
        let p1 = DefId::new(10);
        let p2 = DefId::new(11);
        let t1 = TypeId::ERROR; // placeholder
        let t2 = TypeId(1);

        let map = build_subst_map(&[p1, p2], &[t1, t2]);
        assert_eq!(map.len(), 2);
        assert_eq!(map[&p1], t1);
        assert_eq!(map[&p2], t2);
    }

    #[test]
    fn build_subst_map_mismatched_lengths() {
        let p1 = DefId::new(10);
        let p2 = DefId::new(11);
        let t1 = TypeId::ERROR;

        let map = build_subst_map(&[p1, p2], &[t1]);
        assert_eq!(map.len(), 1);
        assert_eq!(map[&p1], t1);
    }

    #[test]
    fn substitute_param_replaced() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let param_def = DefId::new(100);
        let param_ty = table.intern(Ty::Param(param_def));

        let subst = build_subst_map(&[param_def], &[builtins.int]);
        let result = substitute_mut(param_ty, &subst, &mut table);
        assert_eq!(result, builtins.int);
    }

    #[test]
    fn substitute_no_params_unchanged() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let subst = SubstMap::default();
        let result = substitute_mut(builtins.int, &subst, &mut table);
        assert_eq!(result, builtins.int);
    }

    #[test]
    fn substitute_list_of_param() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let param_def = DefId::new(100);
        let param_ty = table.intern(Ty::Param(param_def));
        let list_of_param = table.intern(Ty::List(param_ty));

        let subst = build_subst_map(&[param_def], &[builtins.string]);
        let result = substitute_mut(list_of_param, &subst, &mut table);

        assert!(matches!(table.resolve(result), Ty::List(inner) if *inner == builtins.string));
    }

    #[test]
    fn substitute_function_type() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let param_def = DefId::new(100);
        let param_ty = table.intern(Ty::Param(param_def));
        let fn_ty = table.intern(Ty::Function {
            params: vec![param_ty],
            ret: param_ty,
            is_async: false,
        });

        let subst = build_subst_map(&[param_def], &[builtins.int]);
        let result = substitute_mut(fn_ty, &subst, &mut table);

        match table.resolve(result) {
            Ty::Function { params, ret, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0], builtins.int);
                assert_eq!(*ret, builtins.int);
            }
            other => panic!("expected Function, got {other:?}"),
        }
    }

    #[test]
    fn infer_simple_param() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let param_def = DefId::new(100);
        let param_ty = table.intern(Ty::Param(param_def));

        let result = infer_type_args(&[param_def], &[param_ty], &[builtins.int], &table);
        assert!(result.is_some());
        let subst = result.unwrap();
        assert_eq!(subst[&param_def], builtins.int);
    }

    #[test]
    fn infer_nested_param() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let param_def = DefId::new(100);
        let param_ty = table.intern(Ty::Param(param_def));
        let list_of_param = table.intern(Ty::List(param_ty));
        let list_of_string = table.intern(Ty::List(builtins.string));

        let result = infer_type_args(&[param_def], &[list_of_param], &[list_of_string], &table);
        assert!(result.is_some());
        let subst = result.unwrap();
        assert_eq!(subst[&param_def], builtins.string);
    }

    #[test]
    fn infer_fails_when_not_all_inferred() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let param_a = DefId::new(100);
        let param_b = DefId::new(101);
        let param_a_ty = table.intern(Ty::Param(param_a));

        // Only param_a appears in the declared types, param_b cannot be inferred
        let result = infer_type_args(&[param_a, param_b], &[param_a_ty], &[builtins.int], &table);
        assert!(result.is_none());
    }
}
