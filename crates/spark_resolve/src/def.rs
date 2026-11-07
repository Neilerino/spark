//! Definition identifiers and metadata.
//!
//! This module provides the core types for tracking definitions in the program:
//! - `DefId`: Unique identifier for any definition
//! - `DefinitionKind`: What kind of thing is defined
//! - `DefinitionInfo`: Full metadata about a definition
//! - `DefinitionTable`: Storage for all definitions

use spark_intern::Symbol;
use spark_modules::ModulePath;
use spark_span::Span;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Unique identifier for any definition in the program.
///
/// `DefId`s are assigned sequentially as definitions are discovered during
/// the collection pass. They serve as keys into the `DefinitionTable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DefId(pub u32);

impl DefId {
    /// A dummy `DefId` used for error recovery.
    pub const DUMMY: Self = Self(u32::MAX);

    /// Creates a new `DefId` with the given index.
    #[must_use]
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    /// Returns the raw index value.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }

    /// Returns true if this is the dummy `DefId`.
    #[must_use]
    pub const fn is_dummy(self) -> bool {
        self.0 == u32::MAX
    }
}

impl Default for DefId {
    fn default() -> Self {
        Self::DUMMY
    }
}

/// The kind of definition.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum DefinitionKind {
    /// Top-level function.
    Function,
    /// Struct type.
    Struct,
    /// Field within a struct (contains parent struct's `DefId`).
    StructField(DefId),
    /// Enum type.
    Enum,
    /// Variant within an enum (contains parent enum's `DefId`).
    EnumVariant(DefId),
    /// Interface definition.
    Interface,
    /// Type alias (`alias X = Y`).
    TypeAlias,
    /// Distinct type (`type X = Y`).
    TypeDef,
    /// Module-level constant.
    Const,
    /// Local variable binding.
    Local,
    /// Function parameter.
    Parameter,
    /// Generic type parameter.
    GenericParam,
    /// Lifetime parameter (for Ref types).
    LifetimeParam,
    /// Built-in function (e.g., `print`).
    BuiltinFunction,
    /// Built-in type with a definition for protocol bindings.
    BuiltinType,
    /// Built-in method on a collection or string type (e.g., `list.len()`).
    BuiltinMethod,
    /// Method from an interface binding (contains the bound type's `DefId`).
    BindingMethod(DefId),
    /// Lambda (lifted anonymous function).
    Lambda,
    /// External (FFI) function declared with `extern fn`.
    ExternFunction,
}

impl DefinitionKind {
    /// Returns true if this is a type definition (struct, enum, interface, etc.).
    #[must_use]
    pub const fn is_type(&self) -> bool {
        matches!(
            self,
            Self::Struct
                | Self::Enum
                | Self::Interface
                | Self::TypeAlias
                | Self::TypeDef
                | Self::GenericParam
                | Self::BuiltinType
        )
    }

    /// Returns true if this is a value definition (function, const, local, etc.).
    ///
    /// Struct and Enum are included because they act as constructors in value
    /// position: `User { ... }` and `Status.Active`.
    #[must_use]
    pub const fn is_value(&self) -> bool {
        matches!(
            self,
            Self::Function
                | Self::Struct
                | Self::Enum
                | Self::Const
                | Self::Local
                | Self::Parameter
                | Self::BuiltinFunction
                | Self::Lambda
                | Self::ExternFunction
        )
    }
}

/// Visibility of a definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Visibility {
    /// Private (default) - only visible within the module.
    #[default]
    Private,
    /// Public - visible outside the module.
    Public,
}

impl Visibility {
    /// Returns true if this is public visibility.
    #[must_use]
    pub const fn is_public(self) -> bool {
        matches!(self, Self::Public)
    }
}

impl From<spark_ast::Visibility> for Visibility {
    fn from(vis: spark_ast::Visibility) -> Self {
        if vis.is_public() {
            Self::Public
        } else {
            Self::Private
        }
    }
}

/// Full information about a definition.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DefinitionInfo {
    /// The kind of definition.
    pub kind: DefinitionKind,
    /// The name of this definition.
    pub name: Symbol,
    /// Source location of the definition.
    pub span: Span,
    /// Visibility (pub or private).
    pub visibility: Visibility,
    /// Module this definition belongs to.
    pub module: ModulePath,
    /// FFI symbol name for stdlib functions (e.g., `"__spark_math_sqrt"`).
    /// `None` for non-stdlib definitions.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub ffi_symbol: Option<&'static str>,
}

impl DefinitionInfo {
    /// Creates a new definition info.
    #[must_use]
    pub const fn new(
        kind: DefinitionKind,
        name: Symbol,
        span: Span,
        visibility: Visibility,
        module: ModulePath,
    ) -> Self {
        Self {
            kind,
            name,
            span,
            visibility,
            module,
            ffi_symbol: None,
        }
    }

    /// Sets the FFI symbol for this definition (builder pattern).
    #[must_use]
    pub const fn with_ffi_symbol(mut self, symbol: &'static str) -> Self {
        self.ffi_symbol = Some(symbol);
        self
    }
}

/// Table of all definitions in the program.
///
/// Definitions are stored in a vector indexed by their `DefId`.
#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DefinitionTable {
    defs: Vec<DefinitionInfo>,
}

impl DefinitionTable {
    /// Creates a new empty definition table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a new definition and returns its `DefId`.
    pub fn insert(&mut self, info: DefinitionInfo) -> DefId {
        let id = DefId(self.defs.len() as u32);
        self.defs.push(info);
        id
    }

    /// Gets a definition by its `DefId`.
    #[must_use]
    pub fn get(&self, id: DefId) -> Option<&DefinitionInfo> {
        if id.is_dummy() {
            return None;
        }
        self.defs.get(id.0 as usize)
    }

    /// Gets a mutable reference to a definition.
    #[must_use]
    pub fn get_mut(&mut self, id: DefId) -> Option<&mut DefinitionInfo> {
        if id.is_dummy() {
            return None;
        }
        self.defs.get_mut(id.0 as usize)
    }

    /// Returns the number of definitions.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.defs.len()
    }

    /// Returns true if there are no definitions.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// Allocates a fresh `DefId` for a synthetic local variable.
    ///
    /// Used during HIR lowering to create bindings for desugared constructs
    /// (e.g., the bound variable in `match a { v -> v, Null -> b }` from `a ?? b`).
    /// The synthetic name is `__synth_N` where N is the `DefId` index.
    pub fn next_synthetic_id(&mut self, span: Span) -> DefId {
        let index = self.defs.len() as u32;
        let name = spark_intern::sym(&format!("__synth_{index}"));
        self.insert(DefinitionInfo::new(
            DefinitionKind::Local,
            name,
            span,
            Visibility::Private,
            ModulePath::root(),
        ))
    }

    /// Iterates over all definitions with their `DefId`s.
    pub fn iter(&self) -> impl Iterator<Item = (DefId, &DefinitionInfo)> {
        self.defs
            .iter()
            .enumerate()
            .map(|(i, info)| (DefId(i as u32), info))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_intern::sym;

    #[test]
    fn def_id_basics() {
        let id = DefId::new(42);
        assert_eq!(id.as_u32(), 42);
        assert!(!id.is_dummy());

        let dummy = DefId::DUMMY;
        assert!(dummy.is_dummy());
    }

    #[test]
    fn def_id_default() {
        let id: DefId = DefId::default();
        assert!(id.is_dummy());
    }

    #[test]
    fn definition_kind_is_type() {
        assert!(DefinitionKind::Struct.is_type());
        assert!(DefinitionKind::Enum.is_type());
        assert!(DefinitionKind::Interface.is_type());
        assert!(DefinitionKind::TypeAlias.is_type());
        assert!(DefinitionKind::GenericParam.is_type());

        assert!(!DefinitionKind::Function.is_type());
        assert!(!DefinitionKind::Local.is_type());
    }

    #[test]
    fn definition_kind_is_value() {
        assert!(DefinitionKind::Function.is_value());
        assert!(DefinitionKind::Const.is_value());
        assert!(DefinitionKind::Local.is_value());
        assert!(DefinitionKind::Parameter.is_value());
        // Struct and Enum are constructors in value position
        assert!(DefinitionKind::Struct.is_value());
        assert!(DefinitionKind::Enum.is_value());

        assert!(!DefinitionKind::Interface.is_value());
        assert!(!DefinitionKind::TypeAlias.is_value());
        assert!(!DefinitionKind::GenericParam.is_value());
    }

    #[test]
    fn visibility_is_public() {
        assert!(Visibility::Public.is_public());
        assert!(!Visibility::Private.is_public());
    }

    #[test]
    fn definition_table_insert_and_get() {
        let mut table = DefinitionTable::new();

        let info = DefinitionInfo::new(
            DefinitionKind::Function,
            sym("foo"),
            Span::DUMMY,
            Visibility::Public,
            ModulePath::root(),
        );

        let id = table.insert(info);
        assert_eq!(id.as_u32(), 0);
        assert_eq!(table.len(), 1);

        let retrieved = table.get(id).unwrap();
        assert_eq!(retrieved.name, sym("foo"));
        assert!(matches!(retrieved.kind, DefinitionKind::Function));
    }

    #[test]
    fn definition_table_multiple() {
        let mut table = DefinitionTable::new();

        let id1 = table.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            sym("a"),
            Span::DUMMY,
            Visibility::Public,
            ModulePath::root(),
        ));

        let id2 = table.insert(DefinitionInfo::new(
            DefinitionKind::Struct,
            sym("B"),
            Span::DUMMY,
            Visibility::Private,
            ModulePath::root(),
        ));

        assert_eq!(id1.as_u32(), 0);
        assert_eq!(id2.as_u32(), 1);
        assert_eq!(table.len(), 2);

        assert_eq!(table.get(id1).unwrap().name, sym("a"));
        assert_eq!(table.get(id2).unwrap().name, sym("B"));
    }

    #[test]
    fn definition_table_get_dummy() {
        let table = DefinitionTable::new();
        assert!(table.get(DefId::DUMMY).is_none());
    }

    #[test]
    fn definition_table_iter() {
        let mut table = DefinitionTable::new();

        table.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            sym("a"),
            Span::DUMMY,
            Visibility::Public,
            ModulePath::root(),
        ));

        table.insert(DefinitionInfo::new(
            DefinitionKind::Struct,
            sym("B"),
            Span::DUMMY,
            Visibility::Private,
            ModulePath::root(),
        ));

        let items: Vec<_> = table.iter().collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].0.as_u32(), 0);
        assert_eq!(items[1].0.as_u32(), 1);
    }
}
