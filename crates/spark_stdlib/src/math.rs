//! `spark.math` module manifest.
//!
//! Provides mathematical functions operating on `Float` values.
//! All functions delegate to the corresponding `__spark_math_*` FFI symbols
//! implemented in `spark_runtime`.

use spark_intern::kw;

use crate::{StdlibEntry, StdlibEntryKind, StdlibModule, StdlibType};

/// Creates the `spark.math` module manifest.
pub fn math_module() -> StdlibModule {
    StdlibModule {
        path: vec![kw::SPARK, kw::MATH],
        entries: vec![
            StdlibEntry {
                name: kw::SQRT,
                params: vec![StdlibType::Float],
                ret: StdlibType::Float,
                ffi_symbol: "__spark_math_sqrt",
                kind: StdlibEntryKind::Function,
            },
            StdlibEntry {
                name: kw::POW,
                params: vec![StdlibType::Float, StdlibType::Float],
                ret: StdlibType::Float,
                ffi_symbol: "__spark_math_pow",
                kind: StdlibEntryKind::Function,
            },
            StdlibEntry {
                name: kw::ABS,
                params: vec![StdlibType::Float],
                ret: StdlibType::Float,
                ffi_symbol: "__spark_math_abs",
                kind: StdlibEntryKind::Function,
            },
            StdlibEntry {
                name: kw::FLOOR,
                params: vec![StdlibType::Float],
                ret: StdlibType::Int,
                ffi_symbol: "__spark_math_floor",
                kind: StdlibEntryKind::Function,
            },
            StdlibEntry {
                name: kw::CEIL,
                params: vec![StdlibType::Float],
                ret: StdlibType::Int,
                ffi_symbol: "__spark_math_ceil",
                kind: StdlibEntryKind::Function,
            },
            StdlibEntry {
                name: kw::MIN,
                params: vec![StdlibType::Float, StdlibType::Float],
                ret: StdlibType::Float,
                ffi_symbol: "__spark_math_min",
                kind: StdlibEntryKind::Function,
            },
            StdlibEntry {
                name: kw::MAX,
                params: vec![StdlibType::Float, StdlibType::Float],
                ret: StdlibType::Float,
                ffi_symbol: "__spark_math_max",
                kind: StdlibEntryKind::Function,
            },
            StdlibEntry {
                name: kw::ROUND,
                params: vec![StdlibType::Float],
                ret: StdlibType::Int,
                ffi_symbol: "__spark_math_round",
                kind: StdlibEntryKind::Function,
            },
        ],
    }
}
