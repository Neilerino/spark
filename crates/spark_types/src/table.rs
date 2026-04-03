//! Type table: stores type information for all definitions and expressions.
//!
//! The [`TypeTable`] is the primary output of type checking. It maps
//! definitions to their types and expressions to their inferred types.

use rustc_hash::FxHashMap;
use spark_ast::ExprId;
use spark_intern::Symbol;
use spark_resolve::DefId;
use spark_span::Span;

use crate::intern::TypeInterner;
use crate::ty::{Constraint, LifetimeId, Ty, TypeId};

/// Information about an interface method, stored during collection for validation.
///
/// Captures the lowered signature of each method declared in an interface
/// definition so that `validate_interface_methods` can compare binding
/// implementations against the contract without re-parsing the AST.
#[derive(Debug, Clone)]
pub struct InterfaceMethodInfo {
    /// Method name symbol.
    pub name: Symbol,
    /// Lowered parameter types (excluding `self`).
    pub param_types: Vec<TypeId>,
    /// Lowered return type.
    pub ret_type: TypeId,
    /// Whether the method is async.
    pub is_async: bool,
    /// Whether the method has a default body.
    pub has_default: bool,
    /// Whether any parameter or return type involves a generic parameter.
    pub has_generics: bool,
    /// The span of the method declaration in the interface.
    pub span: Span,
}

/// `DefId`s for standard protocol interfaces from `spark.core`.
///
/// These are populated after all interfaces are collected and allow the
/// type checker, HIR lowering, and codegen to check if a type binds one of
/// the standard protocols (e.g., `Displayable`, `Iterable`).
///
/// Adding a new protocol requires only adding a field here and populating
/// it in `resolve_well_known_interfaces()`.
#[derive(Debug, Clone, Default)]
pub struct WellKnownInterfaces {
    /// `Displayable` — enables `print()` and string interpolation for structs.
    pub displayable: Option<DefId>,
    /// `Iterable[E]` — enables `each` iteration on custom types.
    pub iterable: Option<DefId>,
    /// `Equatable` — enables `==` and `!=` for structs.
    pub equatable: Option<DefId>,
    /// `Comparable` — enables `<`, `>`, `<=`, `>=` and `sort()` for structs.
    pub comparable: Option<DefId>,
}

/// The type table produced by the type checker.
///
/// Contains all type information gathered during type checking:
/// - The type interner (maps `TypeId` → `Ty`)
/// - Definition types (maps `DefId` → `TypeId`)
/// - Expression types (maps `ExprId` → `TypeId`)
/// - Struct field information
/// - Enum variant information
#[derive(Debug, Clone)]
pub struct TypeTable {
    /// The type interner backing all `TypeId`s.
    pub(crate) interner: TypeInterner,
    /// Maps each definition to its type.
    pub(crate) def_types: FxHashMap<DefId, TypeId>,
    /// Maps each expression (by `ExprId`) to its type.
    pub(crate) expr_types: FxHashMap<ExprId, TypeId>,
    /// Struct fields: struct `DefId` → `[(field_name, field_type)]`.
    pub(crate) struct_fields: FxHashMap<DefId, Vec<(Symbol, TypeId)>>,
    /// Enum variants: enum `DefId` → `[(variant_name, variant_def_id)]`.
    pub(crate) enum_variants: FxHashMap<DefId, Vec<(Symbol, DefId)>>,
    /// Variant fields: variant `DefId` → `[(field_name, field_type)]`.
    pub(crate) variant_fields: FxHashMap<DefId, Vec<(Symbol, TypeId)>>,
    /// Lifetime names: `LifetimeId` → name symbol.
    pub(crate) lifetime_names: Vec<Symbol>,
    /// Reverse map for lifetime deduplication.
    pub(crate) lifetime_dedup: FxHashMap<Symbol, LifetimeId>,
    /// Methods available on user-defined types: type `DefId` → `[(method_name, method_def_id)]`.
    pub(crate) type_methods: FxHashMap<DefId, Vec<(Symbol, DefId)>>,
    /// Resolved method `DefId` for each method call site (keyed by call `Span`).
    pub(crate) method_resolutions: FxHashMap<Span, DefId>,
    /// Resolved field `DefId` for each field access site (keyed by field name `Span`).
    pub(crate) field_resolutions: FxHashMap<Span, DefId>,
    /// Maps a generic definition (struct/enum/function `DefId`) to its ordered generic param `DefId`s.
    pub(crate) generic_params: FxHashMap<DefId, Vec<DefId>>,
    /// Monomorphization instances: call-site `Span` → (callee `DefId`, substitution map).
    /// Populated during type checking for generic function calls, consumed during MIR monomorphization.
    pub(crate) mono_instances: FxHashMap<Span, (DefId, FxHashMap<DefId, TypeId>)>,
    /// Maps a generic param `DefId` to its constraint, if any.
    /// Populated during `collect_generic_params()`, consumed during constraint validation.
    pub(crate) generic_constraints: FxHashMap<DefId, Constraint>,
    /// Maps a type `DefId` (struct/enum) to the interface `DefId`s it binds.
    /// Populated during `collect_binding()`, consumed during constraint validation.
    pub(crate) type_interface_bindings: FxHashMap<DefId, Vec<DefId>>,
    /// Maps call-site spans to the function type of callable struct fields.
    /// Populated during type checking, consumed during HIR lowering to emit
    /// `Call` with `FieldAccess` callee instead of `MethodCall`.
    pub(crate) field_call_fn_types: FxHashMap<Span, TypeId>,
    /// Maps a function (or method) `DefId` to its ordered parameter names.
    /// Populated during declaration collection, consumed during partial
    /// application type checking to resolve named arguments to positions.
    pub(crate) fn_param_names: FxHashMap<DefId, Vec<Symbol>>,
    /// Maps an interface `DefId` to its method signatures.
    /// Populated during `collect_interface()`, consumed during `validate_interface_methods()`.
    pub(crate) interface_methods: FxHashMap<DefId, Vec<InterfaceMethodInfo>>,
    /// Standard protocol interface `DefId`s from `spark.core`.
    /// Populated after declaration collection via `resolve_well_known_interfaces()`.
    pub well_known: WellKnownInterfaces,
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
        // Pre-intern the anonymous lifetime at index 0.
        let anon_sym = spark_intern::sym("");
        Self {
            interner: TypeInterner::new(),
            def_types: FxHashMap::default(),
            expr_types: FxHashMap::default(),
            struct_fields: FxHashMap::default(),
            enum_variants: FxHashMap::default(),
            variant_fields: FxHashMap::default(),
            lifetime_names: vec![anon_sym],
            lifetime_dedup: {
                let mut m = FxHashMap::default();
                m.insert(anon_sym, LifetimeId::ANONYMOUS);
                m
            },
            type_methods: FxHashMap::default(),
            method_resolutions: FxHashMap::default(),
            field_resolutions: FxHashMap::default(),
            field_call_fn_types: FxHashMap::default(),
            generic_params: FxHashMap::default(),
            mono_instances: FxHashMap::default(),
            generic_constraints: FxHashMap::default(),
            type_interface_bindings: FxHashMap::default(),
            fn_param_names: FxHashMap::default(),
            interface_methods: FxHashMap::default(),
            well_known: WellKnownInterfaces::default(),
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

    /// Looks up a type without interning it. Returns `None` if not found.
    #[must_use]
    pub fn lookup(&self, ty: &Ty) -> Option<TypeId> {
        self.interner.lookup(ty)
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

    /// Records the type of an expression (by its `ExprId`).
    pub fn set_expr_type(&mut self, id: ExprId, type_id: TypeId) {
        self.expr_types.insert(id, type_id);
    }

    /// Returns the type of an expression, if known.
    #[must_use]
    pub fn get_expr_type(&self, id: ExprId) -> Option<TypeId> {
        self.expr_types.get(&id).copied()
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

    /// Records the methods available on a user-defined type.
    pub fn set_type_methods(&mut self, def_id: DefId, methods: Vec<(Symbol, DefId)>) {
        self.type_methods.insert(def_id, methods);
    }

    /// Returns the methods available on a user-defined type.
    #[must_use]
    pub fn get_type_methods(&self, def_id: DefId) -> Option<&[(Symbol, DefId)]> {
        self.type_methods.get(&def_id).map(Vec::as_slice)
    }

    /// Records the resolved method `DefId` for a method call site.
    pub fn set_method_resolution(&mut self, span: Span, def_id: DefId) {
        self.method_resolutions.insert(span, def_id);
    }

    /// Returns the resolved method `DefId` for a method call site.
    #[must_use]
    pub fn get_method_resolution(&self, span: Span) -> Option<DefId> {
        self.method_resolutions.get(&span).copied()
    }

    /// Records the resolved field `DefId` for a field access site.
    pub fn set_field_resolution(&mut self, span: Span, def_id: DefId) {
        self.field_resolutions.insert(span, def_id);
    }

    /// Returns the resolved field `DefId` for a field access site.
    #[must_use]
    pub fn get_field_resolution(&self, span: Span) -> Option<DefId> {
        self.field_resolutions.get(&span).copied()
    }

    /// Returns an iterator over all field resolution entries.
    pub fn field_resolutions(&self) -> impl Iterator<Item = (&Span, &DefId)> {
        self.field_resolutions.iter()
    }

    /// Records the function type of a field-call callee.
    pub fn set_field_call_fn_type(&mut self, span: Span, ty: TypeId) {
        self.field_call_fn_types.insert(span, ty);
    }

    /// Returns the function type of a field-call callee.
    #[must_use]
    pub fn get_field_call_fn_type(&self, span: Span) -> Option<TypeId> {
        self.field_call_fn_types.get(&span).copied()
    }

    /// Returns an iterator over all method resolution entries.
    pub fn method_resolutions(&self) -> impl Iterator<Item = (&Span, &DefId)> {
        self.method_resolutions.iter()
    }

    /// Records the generic parameter `DefId`s for a generic definition.
    pub fn set_generic_params(&mut self, def_id: DefId, params: Vec<DefId>) {
        self.generic_params.insert(def_id, params);
    }

    /// Returns the generic parameter `DefId`s for a definition, if any.
    #[must_use]
    pub fn get_generic_params(&self, def_id: DefId) -> Option<&[DefId]> {
        self.generic_params.get(&def_id).map(Vec::as_slice)
    }

    /// Records a constraint for a generic parameter.
    pub fn set_generic_constraint(&mut self, param_def_id: DefId, constraint: Constraint) {
        self.generic_constraints.insert(param_def_id, constraint);
    }

    /// Returns the constraint for a generic parameter, if any.
    #[must_use]
    pub fn get_generic_constraint(&self, param_def_id: DefId) -> Option<&Constraint> {
        self.generic_constraints.get(&param_def_id)
    }

    /// Records that a type binds an interface.
    pub fn add_interface_binding(&mut self, type_def_id: DefId, interface_def_id: DefId) {
        self.type_interface_bindings
            .entry(type_def_id)
            .or_default()
            .push(interface_def_id);
    }

    /// Returns the interface `DefId`s bound by a type.
    #[must_use]
    pub fn get_interface_bindings(&self, type_def_id: DefId) -> Option<&[DefId]> {
        self.type_interface_bindings.get(&type_def_id).map(Vec::as_slice)
    }

    /// Records the ordered parameter names for a function or method.
    pub fn set_fn_param_names(&mut self, def_id: DefId, names: Vec<Symbol>) {
        self.fn_param_names.insert(def_id, names);
    }

    /// Returns the ordered parameter names for a function or method.
    #[must_use]
    pub fn get_fn_param_names(&self, def_id: DefId) -> Option<&[Symbol]> {
        self.fn_param_names.get(&def_id).map(Vec::as_slice)
    }

    /// Records the method signatures for an interface.
    pub fn set_interface_methods(&mut self, def_id: DefId, methods: Vec<InterfaceMethodInfo>) {
        self.interface_methods.insert(def_id, methods);
    }

    /// Returns the method signatures for an interface, if any.
    #[must_use]
    pub fn get_interface_methods(&self, def_id: DefId) -> Option<&[InterfaceMethodInfo]> {
        self.interface_methods.get(&def_id).map(Vec::as_slice)
    }

    /// Returns `true` if a type binds a specific interface.
    #[must_use]
    pub fn type_binds_interface(&self, type_def_id: DefId, interface_def_id: DefId) -> bool {
        self.type_interface_bindings
            .get(&type_def_id)
            .is_some_and(|bindings| bindings.contains(&interface_def_id))
    }

    /// Records a monomorphization instance for a generic function call.
    ///
    /// Keyed by the call expression `Span`, stores the callee `DefId` and
    /// the substitution map from generic param `DefId`s to concrete `TypeId`s.
    pub fn set_mono_instance(
        &mut self,
        span: Span,
        def_id: DefId,
        subst: FxHashMap<DefId, TypeId>,
    ) {
        self.mono_instances.insert(span, (def_id, subst));
    }

    /// Returns all monomorphization instances recorded during type checking.
    #[must_use]
    pub const fn mono_instances(&self) -> &FxHashMap<Span, (DefId, FxHashMap<DefId, TypeId>)> {
        &self.mono_instances
    }

    /// Interns a lifetime name and returns its [`LifetimeId`].
    ///
    /// If the same name has already been interned, returns the existing ID.
    /// An empty-string symbol maps to [`LifetimeId::ANONYMOUS`].
    #[allow(clippy::cast_possible_truncation)]
    pub fn intern_lifetime(&mut self, name: Symbol) -> LifetimeId {
        if let Some(&id) = self.lifetime_dedup.get(&name) {
            return id;
        }
        let id = LifetimeId(self.lifetime_names.len() as u32);
        self.lifetime_names.push(name);
        self.lifetime_dedup.insert(name, id);
        id
    }

    /// Returns the name of a lifetime, if known.
    #[must_use]
    pub fn lifetime_name(&self, id: LifetimeId) -> Option<Symbol> {
        self.lifetime_names.get(id.0 as usize).copied()
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
        let mut table = TypeTable::new();
        let type_id = table.intern(Ty::String);
        let id = ExprId::next();

        table.set_expr_type(id, type_id);
        assert_eq!(table.get_expr_type(id), Some(type_id));
    }

    #[test]
    fn table_fn_param_names() {
        let mut table = TypeTable::new();
        let def_id = DefId::new(5);
        let names = vec![spark_intern::sym("x"), spark_intern::sym("y")];
        table.set_fn_param_names(def_id, names.clone());
        let stored = table.get_fn_param_names(def_id).unwrap();
        assert_eq!(stored, &names);
        assert!(table.get_fn_param_names(DefId::new(999)).is_none());
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
