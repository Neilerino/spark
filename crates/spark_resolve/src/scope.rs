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

    /// Collects all visible value-namespace names at a scope, walking parent chain.
    ///
    /// Inner scopes shadow outer ones (first-seen wins via `seen` set).
    #[must_use]
    pub fn visible_values(&self, scope_id: ScopeId) -> Vec<(Symbol, DefId)> {
        let mut result = Vec::new();
        let mut seen = rustc_hash::FxHashSet::default();
        let mut current = Some(scope_id);
        while let Some(id) = current {
            let scope = self.get(id);
            for (&name, &def_id) in &scope.value_bindings {
                if seen.insert(name) {
                    result.push((name, def_id));
                }
            }
            current = scope.parent;
        }
        result
    }

    /// Collects all visible type-namespace names at a scope, walking parent chain.
    ///
    /// Inner scopes shadow outer ones (first-seen wins via `seen` set).
    #[must_use]
    pub fn visible_types(&self, scope_id: ScopeId) -> Vec<(Symbol, DefId)> {
        let mut result = Vec::new();
        let mut seen = rustc_hash::FxHashSet::default();
        let mut current = Some(scope_id);
        while let Some(id) = current {
            let scope = self.get(id);
            for (&name, &def_id) in &scope.type_bindings {
                if seen.insert(name) {
                    result.push((name, def_id));
                }
            }
            current = scope.parent;
        }
        result
    }

    /// Finds the innermost scope containing `offset` in `file_id`.
    ///
    /// Filters scopes by file and containment, then picks the narrowest
    /// (smallest span length).
    #[must_use]
    pub fn find_scope_at_offset(
        &self,
        offset: u32,
        file_id: spark_span::FileId,
    ) -> Option<ScopeId> {
        self.scopes
            .iter()
            .filter(|s| s.span.file == file_id && s.span.contains(offset))
            .min_by_key(|s| s.span.end - s.span.start)
            .map(|s| s.id)
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

    #[test]
    fn visible_values_walks_parent_chain() {
        let mut arena = ScopeArena::new();
        let root = arena.create_scope(None, ScopeKind::Module, Span::DUMMY);
        let child = arena.create_scope(Some(root), ScopeKind::Function, Span::DUMMY);

        let outer_def = DefId::new(0);
        let inner_def = DefId::new(1);

        arena
            .get_mut(root)
            .define_value(sym("outer"), outer_def)
            .unwrap();
        arena
            .get_mut(child)
            .define_value(sym("inner"), inner_def)
            .unwrap();

        let visible = arena.visible_values(child);
        assert!(visible.contains(&(sym("inner"), inner_def)));
        assert!(visible.contains(&(sym("outer"), outer_def)));
        assert_eq!(visible.len(), 2);
    }

    #[test]
    fn visible_values_inner_shadows_outer() {
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

        let visible = arena.visible_values(child);
        // Should see child's x (shadow), not root's
        assert_eq!(visible.len(), 1);
        assert!(visible.contains(&(sym("x"), child_def)));
    }

    #[test]
    fn visible_types_walks_parent_chain() {
        let mut arena = ScopeArena::new();
        let root = arena.create_scope(None, ScopeKind::Module, Span::DUMMY);
        let child = arena.create_scope(Some(root), ScopeKind::Function, Span::DUMMY);

        let struct_def = DefId::new(0);
        let generic_def = DefId::new(1);

        arena
            .get_mut(root)
            .define_type(sym("User"), struct_def)
            .unwrap();
        arena
            .get_mut(child)
            .define_type(sym("T"), generic_def)
            .unwrap();

        let visible = arena.visible_types(child);
        assert!(visible.contains(&(sym("User"), struct_def)));
        assert!(visible.contains(&(sym("T"), generic_def)));
        assert_eq!(visible.len(), 2);
    }

    #[test]
    fn find_scope_at_offset_returns_innermost() {
        let file = spark_span::FileId::new(0);
        let mut arena = ScopeArena::new();

        // Module scope: bytes 0..100
        let module = arena.create_scope(None, ScopeKind::Module, Span::new(file, 0, 100));
        // Function scope: bytes 10..50
        let func = arena.create_scope(Some(module), ScopeKind::Function, Span::new(file, 10, 50));
        // Block scope: bytes 20..40
        let block = arena.create_scope(Some(func), ScopeKind::Block, Span::new(file, 20, 40));

        // Offset 25 is in all three — should return innermost (block)
        let found = arena.find_scope_at_offset(25, file);
        assert_eq!(found, Some(block));

        // Offset 5 is only in module
        let found = arena.find_scope_at_offset(5, file);
        assert_eq!(found, Some(module));

        // Offset 200 is in nothing
        let found = arena.find_scope_at_offset(200, file);
        assert!(found.is_none());
    }

    #[test]
    fn find_scope_at_offset_filters_by_file() {
        let file0 = spark_span::FileId::new(0);
        let file1 = spark_span::FileId::new(1);
        let mut arena = ScopeArena::new();

        arena.create_scope(None, ScopeKind::Module, Span::new(file0, 0, 100));
        let scope1 = arena.create_scope(None, ScopeKind::Module, Span::new(file1, 0, 100));

        // Offset 50 in file1 should only find the file1 scope
        let found = arena.find_scope_at_offset(50, file1);
        assert_eq!(found, Some(scope1));
    }
}
