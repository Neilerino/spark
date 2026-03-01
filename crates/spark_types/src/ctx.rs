//! Type-checking context bundle.
//!
//! [`TypeCheckCtx`] groups the five arguments that flow through almost every
//! internal type-checking function — the type table, built-in type IDs,
//! definition table, optional resolution map, and diagnostics collector.
//!
//! By threading a single `&mut TypeCheckCtx` instead of five separate
//! parameters, function signatures stay short, call sites are uniform, and
//! adding new context (e.g., a scope stack) only touches this struct.

use spark_diagnostics::Diagnostics;
use spark_resolve::{DefinitionTable, ResolutionMap};

use crate::builtins::BuiltinTypes;
use crate::table::TypeTable;

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
}
