//! Scope hierarchy management.
//!
//! This module provides types for managing the scope hierarchy during
//! name resolution. Scopes form a tree structure where each scope can
//! have bindings and a parent scope.

use rustc_hash::FxHashMap;
use spark_intern::Symbol;
use spark_span::Span;

use crate::def::DefId;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Unique identifier for a scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ScopeId(pub u32);

impl ScopeId {
    /// The root scope ID.
    pub const ROOT: Self = Self(0);

    /// Creates a new scope ID.
    #[must_use]
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    /// Returns the raw index value.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// The kind of scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum ScopeKind {
    /// Module-level scope (top-level items).
    Module,
    /// Function body scope.
    Function,
    /// Block scope (if, loop, match arm, etc.).
    Block,
    /// Generic parameter scope (between `[` and `]` in fn/struct/etc).
    Generic,
    /// Lambda/closure scope.
    Lambda,
}

/// A single scope in the scope hierarchy.
#[derive(Debug)]
pub struct Scope {
    /// This scope's ID.
    pub id: ScopeId,
    /// Parent scope (None for root).
    pub parent: Option<ScopeId>,
    /// Kind of scope.
    pub kind: ScopeKind,
    /// Names defined directly in this scope (value namespace).
    pub value_bindings: FxHashMap<Symbol, DefId>,
    /// Names defined directly in this scope (type namespace).
    pub type_bindings: FxHashMap<Symbol, DefId>,
    /// Span of the scope.
    pub span: Span,
}

impl Scope {
    /// Creates a new scope.
    #[must_use]
    pub fn new(id: ScopeId, parent: Option<ScopeId>, kind: ScopeKind, span: Span) -> Self {
        Self {
            id,
            parent,
            kind,
            value_bindings: FxHashMap::default(),
            type_bindings: FxHashMap::default(),
            span,
        }
    }

    /// Attempts to define a value-namespace name in this scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` with the existing `DefId` if already defined at this level.
    pub fn define_value(&mut self, name: Symbol, def_id: DefId) -> Result<(), DefId> {
        match self.value_bindings.entry(name) {
            std::collections::hash_map::Entry::Occupied(e) => Err(*e.get()),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(def_id);
                Ok(())
            }
        }
    }

    /// Attempts to define a type-namespace name in this scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` with the existing `DefId` if already defined at this level.
    pub fn define_type(&mut self, name: Symbol, def_id: DefId) -> Result<(), DefId> {
        match self.type_bindings.entry(name) {
            std::collections::hash_map::Entry::Occupied(e) => Err(*e.get()),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(def_id);
                Ok(())
            }
        }
    }

    /// Looks up a value-namespace name in this scope only (not parents).
    #[must_use]
    pub fn lookup_value_local(&self, name: Symbol) -> Option<DefId> {
        self.value_bindings.get(&name).copied()
    }

    /// Looks up a type-namespace name in this scope only (not parents).
    #[must_use]
    pub fn lookup_type_local(&self, name: Symbol) -> Option<DefId> {
        self.type_bindings.get(&name).copied()
    }
}

/// Container for all scopes.
#[derive(Debug, Default)]
pub struct ScopeArena {
    scopes: Vec<Scope>,
}

impl ScopeArena {
    /// Creates a new empty scope arena.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new scope and returns its ID.
    pub fn create_scope(
        &mut self,
        parent: Option<ScopeId>,
        kind: ScopeKind,
        span: Span,
    ) -> ScopeId {
        let id = ScopeId(self.scopes.len() as u32);
        self.scopes.push(Scope::new(id, parent, kind, span));
        id
    }

    /// Gets a scope by its ID.
    #[must_use]
    pub fn get(&self, id: ScopeId) -> &Scope {
        &self.scopes[id.0 as usize]
    }

    /// Gets a mutable reference to a scope.
    #[must_use]
    pub fn get_mut(&mut self, id: ScopeId) -> &mut Scope {
        &mut self.scopes[id.0 as usize]
    }

    /// Looks up a value-namespace name, walking up the scope hierarchy.
    #[must_use]
    pub fn lookup_value(&self, start: ScopeId, name: Symbol) -> Option<DefId> {
        let mut current = Some(start);
        while let Some(scope_id) = current {
            let scope = self.get(scope_id);
            if let Some(def_id) = scope.lookup_value_local(name) {
                return Some(def_id);
            }
            current = scope.parent;
        }
        None
    }

    /// Looks up a type-namespace name, walking up the scope hierarchy.
    #[must_use]
    pub fn lookup_type(&self, start: ScopeId, name: Symbol) -> Option<DefId> {
        let mut current = Some(start);
        while let Some(scope_id) = current {
            let scope = self.get(scope_id);
            if let Some(def_id) = scope.lookup_type_local(name) {
                return Some(def_id);
            }
            current = scope.parent;
        }
        None
    }

    /// Returns the number of scopes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.scopes.len()
    }

    /// Returns true if there are no scopes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.scopes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_intern::sym;

    #[test]
    fn scope_id_basics() {
        let id = ScopeId::new(42);
        assert_eq!(id.as_u32(), 42);
        assert_eq!(ScopeId::ROOT.as_u32(), 0);
    }

    #[test]
    fn scope_define_and_lookup_value() {
        let mut scope = Scope::new(ScopeId::ROOT, None, ScopeKind::Module, Span::DUMMY);

        let def_id = DefId::new(0);
        scope.define_value(sym("foo"), def_id).unwrap();

        assert_eq!(scope.lookup_value_local(sym("foo")), Some(def_id));
        assert_eq!(scope.lookup_value_local(sym("bar")), None);
    }

    #[test]
    fn scope_define_and_lookup_type() {
        let mut scope = Scope::new(ScopeId::ROOT, None, ScopeKind::Module, Span::DUMMY);

        let def_id = DefId::new(0);
        scope.define_type(sym("Foo"), def_id).unwrap();

        assert_eq!(scope.lookup_type_local(sym("Foo")), Some(def_id));
        assert_eq!(scope.lookup_type_local(sym("Bar")), None);
    }

    #[test]
    fn scope_duplicate_error() {
        let mut scope = Scope::new(ScopeId::ROOT, None, ScopeKind::Module, Span::DUMMY);

        let def1 = DefId::new(0);
        let def2 = DefId::new(1);

        scope.define_value(sym("foo"), def1).unwrap();
        let result = scope.define_value(sym("foo"), def2);

        assert_eq!(result, Err(def1));
    }

    #[test]
    fn scope_arena_create_and_get() {
        let mut arena = ScopeArena::new();

        let id = arena.create_scope(None, ScopeKind::Module, Span::DUMMY);
        assert_eq!(id.as_u32(), 0);

        let scope = arena.get(id);
        assert!(scope.parent.is_none());
        assert_eq!(scope.kind, ScopeKind::Module);
    }

    #[test]
    fn scope_arena_hierarchy() {
        let mut arena = ScopeArena::new();

        let root = arena.create_scope(None, ScopeKind::Module, Span::DUMMY);
        let child = arena.create_scope(Some(root), ScopeKind::Function, Span::DUMMY);

        assert!(arena.get(root).parent.is_none());
        assert_eq!(arena.get(child).parent, Some(root));
    }

    #[test]
    fn scope_arena_lookup_value_hierarchy() {
        let mut arena = ScopeArena::new();

        let root = arena.create_scope(None, ScopeKind::Module, Span::DUMMY);
        let child = arena.create_scope(Some(root), ScopeKind::Function, Span::DUMMY);

        let root_def = DefId::new(0);
        let child_def = DefId::new(1);

        arena
            .get_mut(root)
            .define_value(sym("outer"), root_def)
            .unwrap();
        arena
            .get_mut(child)
            .define_value(sym("inner"), child_def)
            .unwrap();

        // Child can see both
        assert_eq!(arena.lookup_value(child, sym("inner")), Some(child_def));
        assert_eq!(arena.lookup_value(child, sym("outer")), Some(root_def));

        // Root can only see its own
        assert_eq!(arena.lookup_value(root, sym("outer")), Some(root_def));
        assert_eq!(arena.lookup_value(root, sym("inner")), None);
    }

    #[test]
    fn scope_arena_shadowing() {
        let mut arena = ScopeArena::new();

        let root = arena.create_scope(None, ScopeKind::Module, Span::DUMMY);
        let child = arena.create_scope(Some(root), ScopeKind::Function, Span::DUMMY);

        let root_def = DefId::new(0);
        let child_def = DefId::new(1);

        arena
            .get_mut(root)
            .define_value(sym("x"), root_def)
            .unwrap();
        arena
            .get_mut(child)
            .define_value(sym("x"), child_def)
            .unwrap();

        // Child sees its own (shadowed)
        assert_eq!(arena.lookup_value(child, sym("x")), Some(child_def));

        // Root sees its own
        assert_eq!(arena.lookup_value(root, sym("x")), Some(root_def));
    }
}
