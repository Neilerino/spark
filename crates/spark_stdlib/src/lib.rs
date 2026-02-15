//! # `spark_stdlib`
//!
//! Standard library sources for the Spark compiler.
//!
//! This crate embeds the standard library as `.spark` source files. These
//! sources are injected into the compilation pipeline alongside user code
//! and compiled through the normal phases (lex, parse, resolve, type-check,
//! HIR, MIR, codegen).
//!
//! Each stdlib module uses `extern fn` declarations for raw FFI calls to
//! the Spark runtime, wrapped in `pub fn` Spark functions with proper types.

#![forbid(unsafe_code)]

/// A standard library source file.
///
/// The `path` is a synthetic file path (e.g., `"spark/math.spark"`) used by
/// the module resolver to derive the module path (e.g., `spark.math`).
/// The `source` is the embedded Spark source code.
#[derive(Debug, Clone)]
pub struct StdlibSource {
    /// Synthetic file path for module resolution.
    pub path: &'static str,
    /// Spark source code.
    pub source: &'static str,
}

/// Returns all standard library source files.
///
/// These should be injected into the compilation pipeline before lexing.
#[must_use]
pub fn stdlib_sources() -> Vec<StdlibSource> {
    vec![
        StdlibSource {
            path: "spark/math.spark",
            source: include_str!("../spark/math.spark"),
        },
        StdlibSource {
            path: "spark/json.spark",
            source: include_str!("../spark/json.spark"),
        },
        StdlibSource {
            path: "spark/io.spark",
            source: include_str!("../spark/io.spark"),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_math_source() {
        let sources = stdlib_sources();
        assert_eq!(sources.len(), 3);
        assert_eq!(sources[0].path, "spark/math.spark");
    }

    #[test]
    fn has_json_source() {
        let sources = stdlib_sources();
        assert_eq!(sources[1].path, "spark/json.spark");
    }

    #[test]
    fn has_io_source() {
        let sources = stdlib_sources();
        assert_eq!(sources[2].path, "spark/io.spark");
    }

    #[test]
    fn json_source_contains_module_decl() {
        let sources = stdlib_sources();
        assert!(sources[1].source.contains("module json"));
    }

    #[test]
    fn json_source_contains_parse() {
        let sources = stdlib_sources();
        assert!(sources[1].source.contains("pub fn parse"));
    }

    #[test]
    fn json_source_contains_extern_fns() {
        let sources = stdlib_sources();
        assert!(sources[1].source.contains("extern fn __spark_json_parse"));
    }

    #[test]
    fn math_source_contains_module_decl() {
        let sources = stdlib_sources();
        assert!(sources[0].source.contains("module math"));
    }

    #[test]
    fn math_source_contains_sqrt() {
        let sources = stdlib_sources();
        assert!(sources[0].source.contains("pub fn sqrt"));
    }

    #[test]
    fn math_source_contains_extern_fns() {
        let sources = stdlib_sources();
        assert!(sources[0].source.contains("extern fn __spark_math_sqrt"));
    }

    #[test]
    fn io_source_contains_module_decl() {
        let sources = stdlib_sources();
        assert!(sources[2].source.contains("module io"));
    }

    #[test]
    fn io_source_contains_read_file() {
        let sources = stdlib_sources();
        assert!(sources[2].source.contains("pub fn readFile"));
    }

    #[test]
    fn io_source_contains_failed() {
        let sources = stdlib_sources();
        assert!(sources[2].source.contains("pub fn failed"));
    }

    #[test]
    fn io_source_contains_last_error() {
        let sources = stdlib_sources();
        assert!(sources[2].source.contains("pub fn lastError"));
    }

    #[test]
    fn io_source_contains_extern_fns() {
        let sources = stdlib_sources();
        assert!(sources[2].source.contains("extern fn __spark_io_read_file"));
    }

    #[test]
    fn io_source_contains_read_line() {
        let sources = stdlib_sources();
        assert!(sources[2].source.contains("pub fn readLine"));
    }

    #[test]
    fn io_source_contains_write_stderr() {
        let sources = stdlib_sources();
        assert!(sources[2].source.contains("pub fn writeStderr"));
    }
}
