//! # `spark_stdlib`
//!
//! Standard library manifest registry for the Spark compiler.
//!
//! This crate provides a declarative registry of standard library modules
//! (e.g., `spark.math`). Each module is described as a manifest of entries
//! with their names, parameter types, return types, and FFI symbols.
//!
//! The registry is consumed by the resolver (to create `DefId`s for stdlib
//! functions) and the type checker (to assign type signatures).

#![forbid(unsafe_code)]

mod math;

use spark_intern::{kw, Symbol};

/// Lightweight type descriptor for stdlib function signatures.
///
/// Uses a simple enum instead of `TypeId` to avoid depending on `spark_types`.
/// The type checker converts these to real `TypeId`s during registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdlibType {
    /// 64-bit signed integer.
    Int,
    /// 64-bit floating point.
    Float,
    /// Boolean.
    Bool,
    /// Heap-allocated string.
    String,
    /// Unit / void.
    Void,
}

/// The kind of a stdlib entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdlibEntryKind {
    /// A callable function.
    Function,
}

/// A single entry in a stdlib module (e.g., `sqrt` in `spark.math`).
#[derive(Debug, Clone)]
pub struct StdlibEntry {
    /// The function name (pre-interned symbol).
    pub name: Symbol,
    /// Parameter types.
    pub params: Vec<StdlibType>,
    /// Return type.
    pub ret: StdlibType,
    /// The FFI symbol name used at link time (e.g., `"__spark_math_sqrt"`).
    pub ffi_symbol: &'static str,
    /// What kind of entry this is.
    pub kind: StdlibEntryKind,
}

/// A standard library module (e.g., `spark.math`).
#[derive(Debug, Clone)]
pub struct StdlibModule {
    /// Module path segments (e.g., `[kw::SPARK, kw::MATH]`).
    pub path: Vec<Symbol>,
    /// Entries exported by this module.
    pub entries: Vec<StdlibEntry>,
}

impl StdlibModule {
    /// Finds an entry by name within this module.
    #[must_use]
    pub fn find_entry(&self, name: Symbol) -> Option<&StdlibEntry> {
        self.entries.iter().find(|e| e.name == name)
    }
}

/// Registry of all standard library modules.
///
/// Created once and shared by the resolver and type checker.
#[derive(Debug, Clone)]
pub struct StdlibRegistry {
    modules: Vec<StdlibModule>,
}

impl Default for StdlibRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl StdlibRegistry {
    /// Creates a new registry with all known stdlib modules.
    #[must_use]
    pub fn new() -> Self {
        Self {
            modules: vec![math::math_module()],
        }
    }

    /// Returns true if the given path segments refer to a stdlib module.
    ///
    /// Stdlib paths start with `spark` and have at least 2 segments
    /// (e.g., `spark.math`).
    #[must_use]
    pub fn is_stdlib_path(segments: &[Symbol]) -> bool {
        segments.first() == Some(&kw::SPARK) && segments.len() >= 2
    }

    /// Finds a module by its path segments.
    #[must_use]
    pub fn find_module(&self, segments: &[Symbol]) -> Option<&StdlibModule> {
        self.modules.iter().find(|m| m.path == segments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_math_module() {
        let registry = StdlibRegistry::new();
        let module = registry.find_module(&[kw::SPARK, kw::MATH]);
        assert!(module.is_some());
    }

    #[test]
    fn math_module_has_entries() {
        let registry = StdlibRegistry::new();
        let module = registry.find_module(&[kw::SPARK, kw::MATH]).unwrap();
        assert_eq!(module.entries.len(), 8);
    }

    #[test]
    fn find_sqrt_entry() {
        let registry = StdlibRegistry::new();
        let module = registry.find_module(&[kw::SPARK, kw::MATH]).unwrap();
        let sqrt = module.find_entry(kw::SQRT);
        assert!(sqrt.is_some());
        let sqrt = sqrt.unwrap();
        assert_eq!(sqrt.ffi_symbol, "__spark_math_sqrt");
        assert_eq!(sqrt.params, vec![StdlibType::Float]);
        assert_eq!(sqrt.ret, StdlibType::Float);
    }

    #[test]
    fn is_stdlib_path_checks() {
        assert!(StdlibRegistry::is_stdlib_path(&[kw::SPARK, kw::MATH]));
        assert!(!StdlibRegistry::is_stdlib_path(&[kw::SPARK]));
        assert!(!StdlibRegistry::is_stdlib_path(&[kw::MATH]));
        assert!(!StdlibRegistry::is_stdlib_path(&[]));
    }

    #[test]
    fn unknown_module_returns_none() {
        let registry = StdlibRegistry::new();
        let module = registry.find_module(&[kw::SPARK, kw::SPARK]);
        assert!(module.is_none());
    }

    #[test]
    fn unknown_entry_returns_none() {
        let registry = StdlibRegistry::new();
        let module = registry.find_module(&[kw::SPARK, kw::MATH]).unwrap();
        let entry = module.find_entry(kw::PRINT);
        assert!(entry.is_none());
    }
}
