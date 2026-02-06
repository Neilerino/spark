//! Type table: stores type information for all definitions and expressions.
//!
//! The [`TypeTable`] is the primary output of type checking. It maps
//! definitions to their types and expressions to their inferred types.

use rustc_hash::FxHashMap;
use spark_intern::Symbol;
use spark_resolve::DefId;
use spark_span::Span;

use crate::intern::TypeInterner;
use crate::ty::{Ty, TypeId};

/// The type table produced by the type checker.
///
/// Contains all type information gathered during type checking:
/// - The type interner (maps `TypeId` → `Ty`)
/// - Definition types (maps `DefId` → `TypeId`)
/// - Expression types (maps `Span` → `TypeId`)
/// - Struct field information
/// - Enum variant information
#[derive(Debug)]
pub struct TypeTable {
    /// The type interner backing all `TypeId`s.
    pub(crate) interner: TypeInterner,
    /// Maps each definition to its type.
    pub(crate) def_types: FxHashMap<DefId, TypeId>,
    /// Maps each expression (by span) to its type.
    pub(crate) expr_types: FxHashMap<Span, TypeId>,
    /// Struct fields: struct `DefId` → `[(field_name, field_type)]`.
    pub(crate) struct_fields: FxHashMap<DefId, Vec<(Symbol, TypeId)>>,
    /// Enum variants: enum `DefId` → `[(variant_name, variant_def_id)]`.
    pub(crate) enum_variants: FxHashMap<DefId, Vec<(Symbol, DefId)>>,
    /// Variant fields: variant `DefId` → `[(field_name, field_type)]`.
    pub(crate) variant_fields: FxHashMap<DefId, Vec<(Symbol, TypeId)>>,
}

impl Default for TypeTable {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeTable {
    /// Creates a new empty type table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            interner: TypeInterner::new(),
            def_types: FxHashMap::default(),
            expr_types: FxHashMap::default(),
            struct_fields: FxHashMap::default(),
            enum_variants: FxHashMap::default(),
            variant_fields: FxHashMap::default(),
        }
    }

    /// Interns a type and returns its [`TypeId`].
    pub fn intern(&mut self, ty: Ty) -> TypeId {
        self.interner.intern(ty)
    }

    /// Resolves a [`TypeId`] to its [`Ty`].
    #[must_use]
    pub fn resolve(&self, id: TypeId) -> &Ty {
        self.interner.resolve(id)
    }

    /// Records the type of a definition.
    pub fn set_def_type(&mut self, def_id: DefId, type_id: TypeId) {
        self.def_types.insert(def_id, type_id);
    }

    /// Returns the type of a definition, if known.
    #[must_use]
    pub fn get_def_type(&self, def_id: DefId) -> Option<TypeId> {
        self.def_types.get(&def_id).copied()
    }

    /// Records the type of an expression (by its span).
    pub fn set_expr_type(&mut self, span: Span, type_id: TypeId) {
        self.expr_types.insert(span, type_id);
    }

    /// Returns the type of an expression, if known.
    #[must_use]
    pub fn get_expr_type(&self, span: Span) -> Option<TypeId> {
        self.expr_types.get(&span).copied()
    }

    /// Records the fields of a struct.
    pub fn set_struct_fields(&mut self, def_id: DefId, fields: Vec<(Symbol, TypeId)>) {
        self.struct_fields.insert(def_id, fields);
    }

    /// Returns the fields of a struct.
    #[must_use]
    pub fn get_struct_fields(&self, def_id: DefId) -> Option<&[(Symbol, TypeId)]> {
        self.struct_fields.get(&def_id).map(Vec::as_slice)
    }

    /// Records the variants of an enum.
    pub fn set_enum_variants(&mut self, def_id: DefId, variants: Vec<(Symbol, DefId)>) {
        self.enum_variants.insert(def_id, variants);
    }

    /// Returns the variants of an enum.
    #[must_use]
    pub fn get_enum_variants(&self, def_id: DefId) -> Option<&[(Symbol, DefId)]> {
        self.enum_variants.get(&def_id).map(Vec::as_slice)
    }

    /// Records the fields of an enum variant.
    pub fn set_variant_fields(&mut self, def_id: DefId, fields: Vec<(Symbol, TypeId)>) {
        self.variant_fields.insert(def_id, fields);
    }

    /// Returns the fields of an enum variant.
    #[must_use]
    pub fn get_variant_fields(&self, def_id: DefId) -> Option<&[(Symbol, TypeId)]> {
        self.variant_fields.get(&def_id).map(Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ty::IntWidth;

    #[test]
    fn table_intern_and_resolve() {
        let mut table = TypeTable::new();
        let id = table.intern(Ty::Bool);
        assert_eq!(table.resolve(id), &Ty::Bool);
    }

    #[test]
    fn table_def_type() {
        let mut table = TypeTable::new();
        let type_id = table.intern(Ty::Int(IntWidth::I64));
        let def_id = DefId::new(0);

        table.set_def_type(def_id, type_id);
        assert_eq!(table.get_def_type(def_id), Some(type_id));
        assert_eq!(table.get_def_type(DefId::new(999)), None);
    }

    #[test]
    fn table_expr_type() {
        use spark_span::{FileId, Span};

        let mut table = TypeTable::new();
        let type_id = table.intern(Ty::String);
        let span = Span::new(FileId::new(0), 10, 20);

        table.set_expr_type(span, type_id);
        assert_eq!(table.get_expr_type(span), Some(type_id));
    }

    #[test]
    fn table_struct_fields() {
        let mut table = TypeTable::new();
        let def_id = DefId::new(0);
        let name_type = table.intern(Ty::String);
        let age_type = table.intern(Ty::Int(IntWidth::I64));

        let fields = vec![
            (spark_intern::sym("name"), name_type),
            (spark_intern::sym("age"), age_type),
        ];
        table.set_struct_fields(def_id, fields);

        let stored = table.get_struct_fields(def_id).unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].0, spark_intern::sym("name"));
    }
}
