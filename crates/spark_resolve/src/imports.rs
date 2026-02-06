//! Import handling and visibility checking.
//!
//! This module manages the import scope for a file, tracking which names
//! are imported from other modules and checking visibility.

use rustc_hash::FxHashMap;
use spark_intern::Symbol;
use spark_modules::ModulePath;

use crate::def::DefId;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// A single import binding.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ImportBinding {
    /// Source module path.
    pub source_module: ModulePath,
    /// Original name in source module.
    pub original_name: Symbol,
    /// Resolved `DefId` (filled in during resolution).
    pub def_id: Option<DefId>,
    /// Whether this is a re-export (`pub import`).
    pub is_reexport: bool,
}

impl ImportBinding {
    /// Creates a new import binding.
    #[must_use]
    pub const fn new(source_module: ModulePath, original_name: Symbol, is_reexport: bool) -> Self {
        Self {
            source_module,
            original_name,
            def_id: None,
            is_reexport,
        }
    }
}

/// Resolved import scope for a file.
#[derive(Debug, Default)]
pub struct ImportScope {
    /// Direct imports: local name -> binding info.
    imports: FxHashMap<Symbol, ImportBinding>,
}

impl ImportScope {
    /// Creates a new empty import scope.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an import binding.
    pub fn add_import(&mut self, local_name: Symbol, binding: ImportBinding) {
        self.imports.insert(local_name, binding);
    }

    /// Looks up an import by local name.
    #[must_use]
    pub fn lookup(&self, name: Symbol) -> Option<&ImportBinding> {
        self.imports.get(&name)
    }

    /// Looks up an import mutably.
    #[must_use]
    pub fn lookup_mut(&mut self, name: Symbol) -> Option<&mut ImportBinding> {
        self.imports.get_mut(&name)
    }

    /// Returns an iterator over all imports.
    pub fn iter(&self) -> impl Iterator<Item = (&Symbol, &ImportBinding)> {
        self.imports.iter()
    }

    /// Returns the number of imports.
    #[must_use]
    pub fn len(&self) -> usize {
        self.imports.len()
    }

    /// Returns true if there are no imports.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.imports.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_intern::sym;

    #[test]
    fn import_scope_add_and_lookup() {
        let mut scope = ImportScope::new();

        let binding = ImportBinding::new(ModulePath::from_single(sym("user")), sym("User"), false);

        scope.add_import(sym("User"), binding);

        let result = scope.lookup(sym("User")).unwrap();
        assert_eq!(result.original_name, sym("User"));
        assert!(!result.is_reexport);
    }

    #[test]
    fn import_scope_with_alias() {
        let mut scope = ImportScope::new();

        let binding = ImportBinding::new(ModulePath::from_single(sym("user")), sym("User"), false);

        // Imported with alias
        scope.add_import(sym("U"), binding);

        assert!(scope.lookup(sym("U")).is_some());
        assert!(scope.lookup(sym("User")).is_none());
    }

    #[test]
    fn import_scope_reexport() {
        let mut scope = ImportScope::new();

        let binding = ImportBinding::new(
            ModulePath::from_single(sym("auth")),
            sym("authenticate"),
            true, // pub import
        );

        scope.add_import(sym("authenticate"), binding);

        let result = scope.lookup(sym("authenticate")).unwrap();
        assert!(result.is_reexport);
    }
}
