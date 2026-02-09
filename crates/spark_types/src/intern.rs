//! Type interning for efficient type storage and comparison.
//!
//! The [`TypeInterner`] stores types in a `Vec` indexed by [`TypeId`],
//! with deduplication so that structurally identical types share the
//! same `TypeId`.

use rustc_hash::FxHashMap;

use crate::ty::{Ty, TypeId};

/// Interns types so that structurally identical types share the same [`TypeId`].
///
/// The interner stores types in a `Vec<Ty>` indexed by `TypeId`. A hash map
/// provides O(1) deduplication: if you intern the same `Ty` twice, you get
/// the same `TypeId` back.
///
/// Index 0 is always `Ty::Error` (the error sentinel).
#[derive(Debug, Clone)]
pub struct TypeInterner {
    /// All interned types. Index is the `TypeId`.
    types: Vec<Ty>,
    /// Reverse map for deduplication.
    dedup: FxHashMap<Ty, TypeId>,
}

impl Default for TypeInterner {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeInterner {
    /// Creates a new interner with `Ty::Error` pre-interned at index 0.
    #[must_use]
    pub fn new() -> Self {
        let mut interner = Self {
            types: Vec::with_capacity(128),
            dedup: FxHashMap::default(),
        };
        // Index 0 is always Error
        let error_id = interner.intern_unchecked(Ty::Error);
        debug_assert!(error_id == TypeId::ERROR);
        interner
    }

    /// Interns a type, returning its [`TypeId`].
    ///
    /// If an identical type has already been interned, returns the existing
    /// `TypeId` without storing a duplicate.
    #[allow(clippy::cast_possible_truncation)]
    pub fn intern(&mut self, ty: Ty) -> TypeId {
        if let Some(&id) = self.dedup.get(&ty) {
            return id;
        }
        self.intern_unchecked(ty)
    }

    /// Interns a type without checking for duplicates.
    #[allow(clippy::cast_possible_truncation)]
    fn intern_unchecked(&mut self, ty: Ty) -> TypeId {
        let id = TypeId(self.types.len() as u32);
        self.dedup.insert(ty.clone(), id);
        self.types.push(ty);
        id
    }

    /// Returns the type for a given [`TypeId`].
    ///
    /// # Panics
    ///
    /// Panics if the `TypeId` is out of bounds.
    #[must_use]
    pub fn resolve(&self, id: TypeId) -> &Ty {
        &self.types[id.0 as usize]
    }

    /// Tries to resolve a `TypeId`, returning `None` if out of bounds.
    #[must_use]
    pub fn try_resolve(&self, id: TypeId) -> Option<&Ty> {
        self.types.get(id.0 as usize)
    }

    /// Looks up a type without interning it. Returns `None` if not found.
    #[must_use]
    pub fn lookup(&self, ty: &Ty) -> Option<TypeId> {
        self.dedup.get(ty).copied()
    }

    /// Returns the number of interned types.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.types.len()
    }

    /// Returns true if no types have been interned (except the error sentinel).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        // Only the Error sentinel is present
        self.types.len() <= 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ty::{FloatWidth, IntWidth};

    #[test]
    fn new_interner_has_error() {
        let interner = TypeInterner::new();
        assert_eq!(interner.resolve(TypeId::ERROR), &Ty::Error);
        assert!(interner.is_empty());
    }

    #[test]
    fn intern_primitive() {
        let mut interner = TypeInterner::new();
        let id = interner.intern(Ty::Bool);
        assert_ne!(id, TypeId::ERROR);
        assert_eq!(interner.resolve(id), &Ty::Bool);
    }

    #[test]
    fn dedup_works() {
        let mut interner = TypeInterner::new();
        let id1 = interner.intern(Ty::Int(IntWidth::I64));
        let id2 = interner.intern(Ty::Int(IntWidth::I64));
        assert_eq!(id1, id2);
        // Different type gets different id
        let id3 = interner.intern(Ty::Float(FloatWidth::F64));
        assert_ne!(id1, id3);
    }

    #[test]
    fn intern_error_returns_sentinel() {
        let mut interner = TypeInterner::new();
        let id = interner.intern(Ty::Error);
        assert_eq!(id, TypeId::ERROR);
    }

    #[test]
    fn len_counts_all() {
        let mut interner = TypeInterner::new();
        interner.intern(Ty::Bool);
        interner.intern(Ty::Void);
        // Error + Bool + Void = 3
        assert_eq!(interner.len(), 3);
    }
}
