# Design Note: Standard Library (`spark.*`)

This document describes the architecture for Spark's standard library — a set of namespaced, opt-in modules backed by native runtime functions.

---

## The `spark.*` Namespace

All standard library modules live under the `spark.*` prefix:

```spark
import spark.http { get, post, serve }
import spark.io { read_file, write_file }
import spark.json { parse, stringify }
import spark.net { TcpListener, TcpStream }
import spark.math { sqrt, pow, abs }
import spark.sync { Mutex, Channel }
```

### Why `spark.*` not `std.*`

- **Unambiguous** — no clash with a user directory named `std/`
- **Conventional** — matches the language name (Go: `fmt`, Rust: `std::`, Swift: `Foundation`)
- **Already implied** — existing parser test fixtures use `import std.math { sqrt }` style imports; `spark.*` follows the same pattern with a clearer prefix

---

## Scaling from Current Built-ins

The current `register_builtins()` in `spark_resolve` creates a flat list of globally-available functions. The stdlib extends this to namespaced, opt-in modules:

```
Global (always available):    print, len, append, contains, remove, add
spark.math (import needed):   sqrt, pow, abs, floor, ceil, min, max
spark.io (import needed):     read_file, write_file, read_lines
spark.json (import needed):   parse, stringify
spark.net (import needed):    TcpListener, TcpStream, UdpSocket
spark.http (import needed):   get, post, serve, Request, Response
spark.sync (import needed):   Mutex, Channel, WaitGroup
```

Core built-ins stay global. Everything else is opt-in via `import spark.*`.

---

## Module Architecture

Each stdlib module is a **manifest** — a mapping of function/type names to type signatures and runtime FFI symbols. No `.spark` source files are needed.

### Manifest Structure (Conceptual)

```rust
/// A single exported item from a stdlib module.
pub struct StdlibEntry {
    /// The symbol name visible to Spark code.
    pub name: Symbol,
    /// The type of this entry (function signature, struct type, etc.).
    pub ty: Ty,
    /// The runtime FFI function name (e.g., "__spark_math_sqrt").
    pub ffi_symbol: &'static str,
    /// Whether this is a function, type, or constant.
    pub kind: StdlibEntryKind,
}

pub enum StdlibEntryKind {
    Function,
    Type,       // For struct types like Request, Response
    Constant,   // For named constants
}

/// A stdlib module manifest.
pub struct StdlibModule {
    /// Module path (e.g., "spark.math").
    pub path: &'static str,
    /// Exported items.
    pub entries: Vec<StdlibEntry>,
}
```

### Example: `spark.math`

```rust
StdlibModule {
    path: "spark.math",
    entries: vec![
        StdlibEntry {
            name: intern("sqrt"),
            ty: Ty::Function { params: vec![float_id], ret: float_id },
            ffi_symbol: "__spark_math_sqrt",
            kind: StdlibEntryKind::Function,
        },
        StdlibEntry {
            name: intern("pow"),
            ty: Ty::Function { params: vec![float_id, float_id], ret: float_id },
            ffi_symbol: "__spark_math_pow",
            kind: StdlibEntryKind::Function,
        },
        StdlibEntry {
            name: intern("abs"),
            ty: Ty::Function { params: vec![float_id], ret: float_id },
            ffi_symbol: "__spark_math_abs",
            kind: StdlibEntryKind::Function,
        },
        // floor, ceil, min, max, ...
    ],
}
```

---

## Implementation in the Pipeline

### Resolver (`spark_resolve`)

New function: `register_stdlib_module(name, manifest)` — creates a module rib with `DefId`s for each exported function/type.

Update `resolve_import()`:
- If the import path starts with `spark.`, look up the manifest registry instead of searching the filesystem
- Each manifest entry gets a `DefinitionKind::BuiltinFunction` (same mechanism as `print`)
- Selective imports (`import spark.math { sqrt, pow }`) work naturally — only the named items are brought into scope

```rust
// In spark_resolve/src/resolver.rs

/// Registry of all stdlib modules.
struct StdlibRegistry {
    modules: HashMap<Symbol, StdlibModule>,
}

impl Resolver {
    fn resolve_import(&mut self, import: &Import) {
        if self.is_stdlib_path(&import.path) {
            self.resolve_stdlib_import(import);
        } else {
            self.resolve_filesystem_import(import);
        }
    }

    fn resolve_stdlib_import(&mut self, import: &Import) {
        let module = self.stdlib.get(&import.module_name)
            .unwrap_or_else(|| {
                self.diagnostics.push(
                    Diagnostic::error("unknown standard library module")
                        .with_label(Label::primary(import.span, "no such module"))
                );
                return;
            });

        for name in &import.names {
            if let Some(entry) = module.find(name.symbol) {
                let def_id = self.alloc_def_id(entry.kind);
                self.current_rib_mut().insert_value(name.symbol, def_id);
            } else {
                self.diagnostics.push(
                    Diagnostic::error("item not found in module")
                        .with_label(Label::primary(name.span, "not exported"))
                );
            }
        }
    }
}
```

### Type Checker (`spark_types`)

New function: `register_stdlib_types()` — for each stdlib module, register function signatures in `TypeTable`. Same pattern as `register_builtin_functions()` but organized by module.

```rust
// In spark_types/src/check_decl.rs

fn register_stdlib_types(&mut self) {
    for module in self.stdlib_registry.modules() {
        for entry in &module.entries {
            if let Some(def_id) = self.resolutions.get_stdlib_def(entry.name) {
                self.table.set_type(def_id, entry.ty.clone());
            }
        }
    }
}
```

This runs during the first pass (signature collection), before body checking.

### Codegen (`spark_codegen`)

Map stdlib `DefId`s to runtime stub `FuncId`s. Same mechanism as existing built-in function dispatch — just more entries in the stub table.

```rust
// In spark_codegen/src/cranelift/mod.rs (declare_all_functions)

for module in &stdlib_modules {
    for entry in &module.entries {
        if let StdlibEntryKind::Function = entry.kind {
            let sig = build_signature(&entry.ty);
            let func_id = self.module.declare_function(
                entry.ffi_symbol,
                Linkage::Import,
                &sig,
            )?;
            self.stdlib_stubs.insert(entry.def_id, func_id);
        }
    }
}
```

### Runtime (`spark_runtime`)

New source files per module:

```
spark_runtime/src/
├── lib.rs          # existing
├── string.rs       # existing
├── list.rs         # existing
├── map.rs          # existing
├── set.rs          # existing
├── rc.rs           # existing
├── math.rs         # NEW: spark.math
├── io.rs           # NEW: spark.io
├── json.rs         # NEW: spark.json (feature-gated)
├── net.rs          # NEW: spark.net  (feature-gated)
├── http.rs         # NEW: spark.http (feature-gated)
├── sync.rs         # NEW: spark.sync (feature-gated)
├── parallel.rs     # NEW: parallel each/via (feature-gated, see concurrency doc)
└── async_rt.rs     # NEW: async runtime (feature-gated, see concurrency doc)
```

Each file exposes `extern "C"` functions following the existing naming convention:

```rust
// spark_runtime/src/math.rs

#[no_mangle]
pub extern "C" fn __spark_math_sqrt(x: f64) -> f64 {
    x.sqrt()
}

#[no_mangle]
pub extern "C" fn __spark_math_pow(base: f64, exp: f64) -> f64 {
    base.powf(exp)
}

#[no_mangle]
pub extern "C" fn __spark_math_abs(x: f64) -> f64 {
    x.abs()
}
```

### Cargo Features

Feature-gated dependencies keep the base runtime small:

```toml
# spark_runtime/Cargo.toml

[features]
default = ["core"]
core = []                    # print, strings, collections, RC, math, io
http = ["hyper", "tokio"]    # spark.http
async = ["tokio"]            # async/await/spawn runtime
parallel = ["rayon"]         # parallel each/each via
json = ["serde_json"]        # spark.json

[dependencies]
rayon = { version = "1", optional = true }
tokio = { version = "1", features = ["full"], optional = true }
hyper = { version = "1", optional = true }
serde_json = { version = "1", optional = true }
```

`spark.math` and `spark.io` use only `std` — no external dependencies, always available.

---

## Module Catalog

### `spark.math` — Pure Math Functions

No external dependencies. Uses Rust's `std::f64` methods.

| Function | Signature | FFI Symbol |
|----------|-----------|------------|
| `sqrt` | `(Float): Float` | `__spark_math_sqrt` |
| `pow` | `(Float, Float): Float` | `__spark_math_pow` |
| `abs` | `(Float): Float` | `__spark_math_abs` |
| `floor` | `(Float): Int` | `__spark_math_floor` |
| `ceil` | `(Float): Int` | `__spark_math_ceil` |
| `min` | `(Float, Float): Float` | `__spark_math_min` |
| `max` | `(Float, Float): Float` | `__spark_math_max` |
| `round` | `(Float): Int` | `__spark_math_round` |

### `spark.io` — File I/O

No external dependencies. Uses Rust's `std::fs`.

| Function | Signature | FFI Symbol |
|----------|-----------|------------|
| `read_file` | `(String): String \| IoError` | `__spark_io_read_file` |
| `write_file` | `(String, String): Void \| IoError` | `__spark_io_write_file` |
| `read_lines` | `(String): List[String] \| IoError` | `__spark_io_read_lines` |
| `file_exists` | `(String): Bool` | `__spark_io_file_exists` |

Note: Return types use union types for error handling (Spark's error model — no exceptions).

### `spark.json` — JSON Parsing

Depends on `serde_json` (feature-gated).

| Function | Signature | FFI Symbol |
|----------|-----------|------------|
| `parse` | `(String): JsonValue \| JsonError` | `__spark_json_parse` |
| `stringify` | `(JsonValue): String` | `__spark_json_stringify` |

`JsonValue` would be a Spark enum: `String | Int | Float | Bool | Null | List[JsonValue] | Map[String, JsonValue]`. This requires recursive enum support — may need to be deferred or simplified to string-based access patterns initially.

### `spark.net` — Networking

Depends on `tokio` (feature-gated). All functions are `async`.

| Function | Signature | FFI Symbol |
|----------|-----------|------------|
| `tcp_listen` | `async (String, Int): TcpListener \| NetError` | `__spark_net_tcp_listen` |
| `tcp_connect` | `async (String, Int): TcpStream \| NetError` | `__spark_net_tcp_connect` |

Depends on async runtime from concurrency model (see `docs/design-notes/concurrency-model.md`).

### `spark.http` — HTTP Client and Server

Depends on `hyper` + `tokio` (feature-gated). All functions are `async`.

| Function | Signature | FFI Symbol |
|----------|-----------|------------|
| `get` | `async (String): String \| HttpError` | `__spark_http_get` |
| `post` | `async (String, String): String \| HttpError` | `__spark_http_post` |
| `serve` | `async (Int, (Request): Response): Void` | `__spark_http_serve` |

Depends on both async runtime and `spark.net`.

### `spark.sync` — Synchronization Primitives

For coordination between parallel/async tasks.

| Function | Signature | FFI Symbol |
|----------|-----------|------------|
| `mutex_new` | `[T](T): Mutex[T]` | `__spark_sync_mutex_new` |
| `mutex_lock` | `[T](Mutex[T]): T` | `__spark_sync_mutex_lock` |
| `mutex_unlock` | `[T](Mutex[T], T): Void` | `__spark_sync_mutex_unlock` |
| `channel` | `[T](): (Sender[T], Receiver[T])` | `__spark_sync_channel` |

Depends on generic types (already complete) and concurrency runtime.

---

## Key Files to Modify

| Area | Files |
|------|-------|
| Resolver | `spark_resolve/src/resolver.rs` — `register_stdlib_module()`, update `resolve_import()` |
| Resolver | `spark_resolve/src/stdlib.rs` (new) — `StdlibRegistry`, manifest definitions |
| Type checker | `spark_types/src/check_decl.rs` — `register_stdlib_types()` |
| Codegen | `spark_codegen/src/cranelift/mod.rs` — stdlib stub declarations |
| Runtime | `spark_runtime/src/math.rs` (new) |
| Runtime | `spark_runtime/src/io.rs` (new) |
| Runtime | `spark_runtime/src/json.rs` (new, feature-gated) |
| Runtime | `spark_runtime/src/net.rs` (new, feature-gated) |
| Runtime | `spark_runtime/src/http.rs` (new, feature-gated) |
| Runtime | `spark_runtime/src/sync.rs` (new, feature-gated) |
| Runtime | `spark_runtime/Cargo.toml` — feature-gated dependencies |

---

## Suggested Implementation Order

Each module validates the pipeline incrementally:

1. **`spark.math`** — Pure functions, no external deps, validates the entire stdlib pipeline (resolver import → type check → codegen → runtime FFI). Start here to prove the architecture.

2. **`spark.io`** — File I/O via `std::fs`. Introduces union return types (`String | IoError`) in stdlib context. No async needed.

3. **`spark.json`** — String to structured data. Useful for web servers and data processing. First feature-gated dependency (`serde_json`).

4. **`spark.net`** — TCP/UDP. Depends on async runtime (see `docs/design-notes/concurrency-model.md` Phase 2+).

5. **`spark.http`** — Highest-level module. Depends on net + async + json. The "hello world" web server is a compelling demo:

```spark
import spark.http { serve, Request, Response }

fn handler(req: Request): Response {
    return Response { status: 200, body: "Hello, Spark!" }
}

fn main(): Int {
    serve(8080, handler)
    return 0
}
```

---

## Open Questions

- **Error types**: Should each module define its own error type (`IoError`, `HttpError`, `JsonError`), or use a shared `spark.Error` type? Per-module types are more informative but add type proliferation. Recommendation: per-module, with a common `message: String` field.
- **JsonValue representation**: Recursive enum types may not be supported yet. A simpler initial approach could be string-keyed accessor functions: `json_get_string(value, key)`, `json_get_int(value, key)`. Revisit when enum boxing is available.
- **Module discoverability**: Should `import spark` (without a submodule) be an error, or list available modules in the diagnostic? Recommendation: error with a help note listing modules.
- **Versioning**: Should stdlib modules have versions? Not needed for v1 — the stdlib ships with the compiler and versions together.

---

## References

- Current built-in function architecture: MEMORY.md "Runtime & Built-in Functions" section
- Concurrency runtime (Tokio/Rayon): `docs/design-notes/concurrency-model.md`
- Language spec (import syntax): `docs/lang-spec.md`
- Resolver architecture: `spark_resolve/src/resolver.rs`
