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
            path: "spark/core.spark",
            source: include_str!("../spark/core.spark"),
        },
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
        StdlibSource {
            path: "spark/time.spark",
            source: include_str!("../spark/time.spark"),
        },
        StdlibSource {
            path: "spark/net.spark",
            source: include_str!("../spark/net.spark"),
        },
        StdlibSource {
            path: "spark/http.spark",
            source: include_str!("../spark/http.spark"),
        },
        StdlibSource {
            path: "spark/sys.spark",
            source: include_str!("../spark/sys.spark"),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_core_source() {
        let sources = stdlib_sources();
        assert_eq!(sources.len(), 7);
        assert_eq!(sources[0].path, "spark/core.spark");
    }

    #[test]
    fn core_source_contains_module_decl() {
        let sources = stdlib_sources();
        assert!(sources[0].source.contains("module core"));
    }

    #[test]
    fn core_source_contains_displayable() {
        let sources = stdlib_sources();
        assert!(sources[0].source.contains("pub interface Displayable"));
    }

    #[test]
    fn core_source_contains_iterable() {
        let sources = stdlib_sources();
        assert!(sources[0].source.contains("pub interface Iterable"));
    }

    #[test]
    fn core_source_contains_equatable() {
        let sources = stdlib_sources();
        assert!(sources[0].source.contains("pub interface Equatable"));
    }

    #[test]
    fn core_source_contains_comparable() {
        let sources = stdlib_sources();
        assert!(sources[0].source.contains("pub interface Comparable"));
    }

    #[test]
    fn has_math_source() {
        let sources = stdlib_sources();
        assert_eq!(sources.len(), 7);
        assert_eq!(sources[0].path, "spark/math.spark");
    }

    #[test]
    fn has_json_source() {
        let sources = stdlib_sources();
        assert_eq!(sources[2].path, "spark/json.spark");
    }

    #[test]
    fn has_io_source() {
        let sources = stdlib_sources();
        assert_eq!(sources[3].path, "spark/io.spark");
    }

    #[test]
    fn json_source_contains_module_decl() {
        let sources = stdlib_sources();
        assert!(sources[2].source.contains("module json"));
    }

    #[test]
    fn json_source_contains_parse() {
        let sources = stdlib_sources();
        assert!(sources[2].source.contains("pub fn parse"));
    }

    #[test]
    fn json_source_contains_extern_fns() {
        let sources = stdlib_sources();
        assert!(sources[2].source.contains("extern fn __spark_json_parse"));
    }

    #[test]
    fn math_source_contains_module_decl() {
        let sources = stdlib_sources();
        assert!(sources[1].source.contains("module math"));
    }

    #[test]
    fn math_source_contains_sqrt() {
        let sources = stdlib_sources();
        assert!(sources[1].source.contains("pub fn sqrt"));
    }

    #[test]
    fn math_source_contains_extern_fns() {
        let sources = stdlib_sources();
        assert!(sources[1].source.contains("extern fn __spark_math_sqrt"));
    }

    #[test]
    fn io_source_contains_module_decl() {
        let sources = stdlib_sources();
        assert!(sources[3].source.contains("module io"));
    }

    #[test]
    fn io_source_contains_read_file() {
        let sources = stdlib_sources();
        assert!(sources[3].source.contains("pub fn readFile"));
    }

    #[test]
    fn io_source_contains_failed() {
        let sources = stdlib_sources();
        assert!(sources[3].source.contains("IoError binds IIoError"));
    }

    #[test]
    fn io_source_contains_last_error() {
        let sources = stdlib_sources();
        assert!(sources[3].source.contains("pub fn lastError"));
    }

    #[test]
    fn io_source_contains_extern_fns() {
        let sources = stdlib_sources();
        assert!(sources[3].source.contains("extern fn __spark_io_read_file"));
    }

    #[test]
    fn io_source_contains_read_line() {
        let sources = stdlib_sources();
        assert!(sources[3].source.contains("pub fn readLine"));
    }

    #[test]
    fn io_source_contains_write_stderr() {
        let sources = stdlib_sources();
        assert!(sources[3].source.contains("pub fn writeStderr"));
    }

    #[test]
    fn has_time_source() {
        let sources = stdlib_sources();
        assert_eq!(sources[4].path, "spark/time.spark");
    }

    #[test]
    fn time_source_contains_module_decl() {
        let sources = stdlib_sources();
        assert!(sources[4].source.contains("module time"));
    }

    #[test]
    fn time_source_contains_sleep() {
        let sources = stdlib_sources();
        assert!(sources[4].source.contains("pub fn sleep"));
    }

    #[test]
    fn time_source_contains_extern_fns() {
        let sources = stdlib_sources();
        assert!(sources[4].source.contains("extern fn __spark_time_sleep"));
    }

    #[test]
    fn has_net_source() {
        let sources = stdlib_sources();
        assert_eq!(sources[5].path, "spark/net.spark");
    }

    #[test]
    fn net_source_contains_module_decl() {
        let sources = stdlib_sources();
        assert!(sources[5].source.contains("module net"));
    }

    #[test]
    fn net_source_contains_tcp_listen() {
        let sources = stdlib_sources();
        assert!(sources[5].source.contains("pub fn tcpListen"));
    }

    #[test]
    fn net_source_contains_extern_fns() {
        let sources = stdlib_sources();
        assert!(sources[5]
            .source
            .contains("extern fn __spark_net_tcp_listen"));
    }

    #[test]
    fn net_source_contains_failed() {
        let sources = stdlib_sources();
        assert!(sources[5].source.contains("NetError binds INetError"));
    }

    #[test]
    fn net_source_contains_resolve() {
        let sources = stdlib_sources();
        assert!(sources[5].source.contains("pub fn resolve"));
    }

    #[test]
    fn has_http_source() {
        let sources = stdlib_sources();
        assert_eq!(sources[6].path, "spark/http.spark");
    }

    #[test]
    fn http_source_contains_module_decl() {
        let sources = stdlib_sources();
        assert!(sources[6].source.contains("module http"));
    }

    #[test]
    fn http_source_contains_get() {
        let sources = stdlib_sources();
        assert!(sources[6].source.contains("pub fn get"));
    }

    #[test]
    fn http_source_contains_serve() {
        let sources = stdlib_sources();
        assert!(sources[6].source.contains("pub fn serve"));
    }

    #[test]
    fn http_source_contains_extern_fns() {
        let sources = stdlib_sources();
        assert!(sources[6].source.contains("extern fn __spark_http_get"));
    }

    #[test]
    fn http_source_contains_respond() {
        let sources = stdlib_sources();
        assert!(sources[6].source.contains("Request binds IRequest"));
    }

    #[test]
    fn has_sys_source() {
        let sources = stdlib_sources();
        assert_eq!(sources[6].path, "spark/sys.spark");
    }

    #[test]
    fn sys_source_contains_module_decl() {
        let sources = stdlib_sources();
        assert!(sources[6].source.contains("module sys"));
    }

    #[test]
    fn sys_source_contains_args() {
        let sources = stdlib_sources();
        assert!(sources[6].source.contains("pub fn args"));
    }

    #[test]
    fn sys_source_contains_get_env() {
        let sources = stdlib_sources();
        assert!(sources[6].source.contains("pub fn getEnv"));
    }

    #[test]
    fn sys_source_contains_extern_fns() {
        let sources = stdlib_sources();
        assert!(sources[6].source.contains("extern fn __spark_sys_args"));
    }

    #[test]
    fn sys_source_contains_has_env() {
        let sources = stdlib_sources();
        assert!(sources[6].source.contains("pub fn hasEnv"));
    }

    #[test]
    fn sys_source_contains_set_env() {
        let sources = stdlib_sources();
        assert!(sources[6].source.contains("pub fn setEnv"));
    }

    #[test]
    fn sys_source_contains_env_keys() {
        let sources = stdlib_sources();
        assert!(sources[6].source.contains("pub fn envKeys"));
    }
}
