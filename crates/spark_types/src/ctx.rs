//! Type-checking context bundle.
//!
//! [`TypeCheckCtx`] groups the five arguments that flow through almost every
//! internal type-checking function — the type table, built-in type IDs,
//! definition table, optional resolution map, and diagnostics collector.
//!
//! By threading a single `&mut TypeCheckCtx` instead of five separate
//! parameters, function signatures stay short, call sites are uniform, and
//! adding new context (e.g., a scope stack) only touches this struct.

use rustc_hash::FxHashMap;
use spark_diagnostics::Diagnostics;
use spark_resolve::{DefId, DefinitionTable, ResolutionMap};

use crate::builtins::BuiltinTypes;
use crate::table::TypeTable;
use crate::ty::TypeId;

/// Shared context for type-checking operations.
///
/// Bundles the mutable type table, immutable lookup tables, and the
/// diagnostics sink that every checking / lowering function needs.
///
/// `current_fn_ret` is intentionally **not** part of this struct because
/// it changes at each function scope boundary and is only meaningful
/// during body checking (Pass 2).
pub struct TypeCheckCtx<'a> {
    /// The type table being built (mutable).
    pub table: &'a mut TypeTable,
    /// Built-in type IDs (immutable, created once at checker init).
    pub builtins: &'a BuiltinTypes,
    /// All definitions from the resolver (borrowed, read-only).
    pub definitions: &'a DefinitionTable,
    /// Per-file resolution map (may be absent for synthetic items).
    pub resolutions: Option<&'a ResolutionMap>,
    /// Accumulated diagnostics (mutable).
    pub diagnostics: &'a mut Diagnostics,
    /// Stack of type narrowing scopes.
    ///
    /// Each scope maps a `DefId` to its narrowed type within that scope.
    /// Used for `is`-check narrowing in `if` statements — e.g., after
    /// `if x is Null { return }`, `x` is narrowed from `T?` to `T`.
    pub narrowings: Vec<FxHashMap<DefId, TypeId>>,
}

impl<'a> TypeCheckCtx<'a> {
    /// Creates a new type-checking context.
    pub const fn new(
        table: &'a mut TypeTable,
        builtins: &'a BuiltinTypes,
        definitions: &'a DefinitionTable,
        resolutions: Option<&'a ResolutionMap>,
        diagnostics: &'a mut Diagnostics,
    ) -> Self {
        Self {
            table,
            builtins,
            definitions,
            resolutions,
            diagnostics,
            narrowings: Vec::new(),
        }
    }

    /// Pushes a new empty narrowing scope onto the stack.
    pub fn push_narrowing_scope(&mut self) {
        self.narrowings.push(FxHashMap::default());
    }

    /// Pops the top narrowing scope from the stack.
    pub fn pop_narrowing_scope(&mut self) {
        self.narrowings.pop();
    }

    /// Adds a narrowing for `def_id` to `narrowed_ty` in the current
    /// (topmost) scope. Does nothing if the stack is empty.
    pub fn add_narrowing(&mut self, def_id: DefId, narrowed_ty: TypeId) {
        if let Some(scope) = self.narrowings.last_mut() {
            scope.insert(def_id, narrowed_ty);
        }
    }

    /// Looks up a narrowed type for `def_id`, searching from the top of
    /// the stack downward. Returns `None` if no narrowing exists.
    #[must_use]
    pub fn get_narrowed_type(&self, def_id: DefId) -> Option<TypeId> {
        for scope in self.narrowings.iter().rev() {
            if let Some(&ty) = scope.get(&def_id) {
                return Some(ty);
            }
        }
        None
    }
}
