//! Rib-based lexical scoping.
//!
//! This module implements the "rib stack" pattern for lexical scoping,
//! similar to what rustc uses. Each rib represents a scope level where
//! new bindings can be introduced.
//!
//! The rib stack maintains two separate namespaces:
//! - **Value namespace**: variables, functions, constants, parameters
//! - **Type namespace**: structs, enums, interfaces, type aliases, generic params

use rustc_hash::FxHashMap;
use spark_intern::Symbol;

use crate::def::DefId;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// The kind of rib, which affects name lookup behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum RibKind {
    /// Normal binding rib (function body, block).
    Normal,
    /// Module-level rib (items can forward-reference each other).
    Module,
    /// Generic parameter rib.
    Generic,
    /// Function parameter rib.
    Parameter,
    /// Lambda/closure rib (for capture tracking).
    Lambda,
}

/// A single rib in the rib stack.
///
/// A rib contains bindings introduced at a particular scope level.
#[derive(Debug)]
pub struct Rib {
    /// Kind of rib.
    pub kind: RibKind,
    /// Bindings introduced in this rib.
    pub bindings: FxHashMap<Symbol, DefId>,
}

impl Rib {
    /// Creates a new rib of the given kind.
    #[must_use]
    pub fn new(kind: RibKind) -> Self {
        Self {
            kind,
            bindings: FxHashMap::default(),
        }
    }

    /// Defines a name in this rib.
    ///
    /// Returns the previous `DefId` if the name was already defined.
    pub fn define(&mut self, name: Symbol, def_id: DefId) -> Option<DefId> {
        self.bindings.insert(name, def_id)
    }

    /// Looks up a name in this rib only.
    #[must_use]
    pub fn lookup(&self, name: Symbol) -> Option<DefId> {
        self.bindings.get(&name).copied()
    }

    /// Returns true if this rib contains the given name.
    #[must_use]
    pub fn contains(&self, name: Symbol) -> bool {
        self.bindings.contains_key(&name)
    }
}

/// Stack of ribs for name resolution.
///
/// Maintains separate stacks for value and type namespaces to allow
/// the same name to refer to both a value and a type.
#[derive(Debug, Default)]
pub struct RibStack {
    /// Value namespace ribs (variables, functions, constants).
    value_ribs: Vec<Rib>,
    /// Type namespace ribs (structs, enums, type aliases).
    type_ribs: Vec<Rib>,
}

impl RibStack {
    /// Creates a new empty rib stack.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pushes a new value-namespace rib.
    pub fn push_value_rib(&mut self, kind: RibKind) {
        self.value_ribs.push(Rib::new(kind));
    }

    /// Pops the innermost value-namespace rib.
    pub fn pop_value_rib(&mut self) -> Option<Rib> {
        self.value_ribs.pop()
    }

    /// Pushes a new type-namespace rib.
    pub fn push_type_rib(&mut self, kind: RibKind) {
        self.type_ribs.push(Rib::new(kind));
    }

    /// Pops the innermost type-namespace rib.
    pub fn pop_type_rib(&mut self) -> Option<Rib> {
        self.type_ribs.pop()
    }

    /// Pushes ribs for both namespaces.
    pub fn push_rib(&mut self, kind: RibKind) {
        self.push_value_rib(kind);
        self.push_type_rib(kind);
    }

    /// Pops ribs from both namespaces.
    pub fn pop_rib(&mut self) {
        self.pop_value_rib();
        self.pop_type_rib();
    }

    /// Defines a value-namespace name in the innermost rib.
    ///
    /// Returns the previous `DefId` if the name was already defined in this rib.
    pub fn define_value(&mut self, name: Symbol, def_id: DefId) -> Option<DefId> {
        self.value_ribs.last_mut()?.define(name, def_id)
    }

    /// Defines a type-namespace name in the innermost rib.
    ///
    /// Returns the previous `DefId` if the name was already defined in this rib.
    pub fn define_type(&mut self, name: Symbol, def_id: DefId) -> Option<DefId> {
        self.type_ribs.last_mut()?.define(name, def_id)
    }

    /// Looks up a value-namespace name, searching from innermost to outermost.
    #[must_use]
    pub fn lookup_value(&self, name: Symbol) -> Option<DefId> {
        for rib in self.value_ribs.iter().rev() {
            if let Some(def_id) = rib.lookup(name) {
                return Some(def_id);
            }
        }
        None
    }

    /// Looks up a type-namespace name, searching from innermost to outermost.
    #[must_use]
    pub fn lookup_type(&self, name: Symbol) -> Option<DefId> {
        for rib in self.type_ribs.iter().rev() {
            if let Some(def_id) = rib.lookup(name) {
                return Some(def_id);
            }
        }
        None
    }

    /// Returns true if a value-namespace name is defined in the innermost rib.
    #[must_use]
    pub fn is_defined_in_current_value_rib(&self, name: Symbol) -> bool {
        self.value_ribs.last().is_some_and(|r| r.contains(name))
    }

    /// Returns true if a type-namespace name is defined in the innermost rib.
    #[must_use]
    pub fn is_defined_in_current_type_rib(&self, name: Symbol) -> bool {
        self.type_ribs.last().is_some_and(|r| r.contains(name))
    }

    /// Returns the depth of the value rib stack.
    #[must_use]
    pub const fn value_depth(&self) -> usize {
        self.value_ribs.len()
    }

    /// Returns the depth of the type rib stack.
    #[must_use]
    pub const fn type_depth(&self) -> usize {
        self.type_ribs.len()
    }

    /// Returns a reference to the innermost value rib, if any.
    #[must_use]
    pub fn current_value_rib(&self) -> Option<&Rib> {
        self.value_ribs.last()
    }

    /// Returns a reference to the innermost type rib, if any.
    #[must_use]
    pub fn current_type_rib(&self) -> Option<&Rib> {
        self.type_ribs.last()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_intern::sym;

    #[test]
    fn rib_define_and_lookup() {
        let mut rib = Rib::new(RibKind::Normal);

        let def_id = DefId::new(0);
        assert!(rib.define(sym("x"), def_id).is_none());

        assert_eq!(rib.lookup(sym("x")), Some(def_id));
        assert_eq!(rib.lookup(sym("y")), None);
    }

    #[test]
    fn rib_redefine() {
        let mut rib = Rib::new(RibKind::Normal);

        let def1 = DefId::new(0);
        let def2 = DefId::new(1);

        assert!(rib.define(sym("x"), def1).is_none());
        assert_eq!(rib.define(sym("x"), def2), Some(def1));
        assert_eq!(rib.lookup(sym("x")), Some(def2));
    }

    #[test]
    fn rib_stack_push_pop() {
        let mut stack = RibStack::new();

        stack.push_value_rib(RibKind::Module);
        assert_eq!(stack.value_depth(), 1);

        stack.push_value_rib(RibKind::Normal);
        assert_eq!(stack.value_depth(), 2);

        stack.pop_value_rib();
        assert_eq!(stack.value_depth(), 1);
    }

    #[test]
    fn rib_stack_define_and_lookup() {
        let mut stack = RibStack::new();
        stack.push_value_rib(RibKind::Module);

        let def_id = DefId::new(0);
        assert!(stack.define_value(sym("x"), def_id).is_none());
        assert_eq!(stack.lookup_value(sym("x")), Some(def_id));
    }

    #[test]
    fn rib_stack_shadowing() {
        let mut stack = RibStack::new();

        stack.push_value_rib(RibKind::Module);
        let outer_def = DefId::new(0);
        stack.define_value(sym("x"), outer_def);

        stack.push_value_rib(RibKind::Normal);
        let inner_def = DefId::new(1);
        stack.define_value(sym("x"), inner_def);

        // Inner shadows outer
        assert_eq!(stack.lookup_value(sym("x")), Some(inner_def));

        stack.pop_value_rib();

        // Outer visible again
        assert_eq!(stack.lookup_value(sym("x")), Some(outer_def));
    }

    #[test]
    fn rib_stack_separate_namespaces() {
        let mut stack = RibStack::new();
        stack.push_rib(RibKind::Module);

        let value_def = DefId::new(0);
        let type_def = DefId::new(1);

        stack.define_value(sym("Foo"), value_def);
        stack.define_type(sym("Foo"), type_def);

        // Same name, different namespaces
        assert_eq!(stack.lookup_value(sym("Foo")), Some(value_def));
        assert_eq!(stack.lookup_type(sym("Foo")), Some(type_def));
    }

    #[test]
    fn rib_stack_is_defined_in_current() {
        let mut stack = RibStack::new();
        stack.push_value_rib(RibKind::Module);

        let def = DefId::new(0);
        stack.define_value(sym("x"), def);

        assert!(stack.is_defined_in_current_value_rib(sym("x")));
        assert!(!stack.is_defined_in_current_value_rib(sym("y")));

        stack.push_value_rib(RibKind::Normal);

        // x not defined in current (inner) rib
        assert!(!stack.is_defined_in_current_value_rib(sym("x")));
    }

    #[test]
    fn rib_stack_type_lookup_hierarchy() {
        let mut stack = RibStack::new();

        stack.push_type_rib(RibKind::Module);
        let struct_def = DefId::new(0);
        stack.define_type(sym("User"), struct_def);

        stack.push_type_rib(RibKind::Generic);
        let generic_def = DefId::new(1);
        stack.define_type(sym("T"), generic_def);

        // Both visible
        assert_eq!(stack.lookup_type(sym("User")), Some(struct_def));
        assert_eq!(stack.lookup_type(sym("T")), Some(generic_def));

        stack.pop_type_rib();

        // Generic param no longer visible
        assert_eq!(stack.lookup_type(sym("User")), Some(struct_def));
        assert_eq!(stack.lookup_type(sym("T")), None);
    }
}
