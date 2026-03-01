# SPARK-059 — Package Management & FFI Interop Design

## Status: Open

## Problem

Spark has a complete compilation pipeline and a working module system, but two critical gaps
block collaboration and ecosystem growth:

1. **No external packages.** There is no way to depend on code outside the project root. Every
   Spark project is an island — no shared libraries, no reusable components, no ecosystem.

2. **FFI is compiler-maintainer-only.** The `extern fn` mechanism works, but users cannot use it
   in practice because (a) the linker only links `libspark_runtime.a` plus hardcoded system libs,
   (b) there is no way to specify additional link libraries, and (c) opaque C handles require
   workarounds like `handle: Int` (see `net.spark`).

This document proposes concrete designs for both systems, aligned with Spark's philosophy of
explicitness, readability, and progressive complexity.

---

## 1. Current State

### Module System (fully implemented)

- File-based: `module`, `using`, `import` keywords
- Module graph with cycle detection (Tarjan's) and topological sort
- Visibility enforcement (`pub`), re-exports (`pub import`)
- File discovery: input file's parent = project root, recursive `.spark` file scan
- Stdlib injected as source via `spark_stdlib::stdlib_sources()` before user sources

### Extern FFI (working but limited)

- Syntax: `extern fn name(params): ReturnType` (no body)
- AST: `FunctionDef.is_extern: bool` (`crates/spark_ast/src/decl.rs:145`)
- Resolver: `DefinitionKind::ExternFunction` (`crates/spark_resolve/src/def.rs:91`)
- Codegen: function name used as linker symbol, declared with `Linkage::Import`
  (`crates/spark_codegen/src/cranelift/emit_term.rs:1770-1834`)
- Stdlib pattern: private `extern fn __spark_*()` wrapped in `pub fn`

### Linking (hardcoded)

```rust
// crates/spark_driver/src/main.rs:303-310
cmd.arg(&tmp_obj)
    .arg(&runtime_lib)
    .arg("-o").arg(output_path)
    .arg("-lm")
    .arg("-lpthread")
    .arg("-ldl");
```

No mechanism to pass additional `-l` or `-L` flags. Users cannot link against their own libraries.

### What's Missing

| Capability                      | Status      |
| ------------------------------- | ----------- |
| Project manifest file           | None        |
| External dependency declaration | None        |
| Dependency resolution           | None        |
| User-specified link libraries   | None        |
| Opaque pointer type             | Workaround  |
| Package registry                | None        |

---

## 2. Package Management Design

### 2.1 Manifest: `Spark.toml`

The presence of `Spark.toml` in a directory marks it as a Spark project root. If absent,
the driver falls back to current behavior (input file's parent = root, no dependencies).

```toml
[project]
name = "my-web-app"
version = "0.1.0"
entry = "main.spark"       # optional, defaults to main.spark

[dependencies]
utils = { path = "../shared-utils" }
json  = { git = "https://github.com/spark-pkg/json", tag = "v1.0.0" }
http  = "2.1.0"            # shorthand for registry dependency (Phase 3)
```

**Design decisions:**

- **TOML format** — Readable, widely understood, no indentation ambiguity. Familiar to Rust
  developers (a likely early-adopter audience).
- **`entry` field** — Explicit, not convention-based. Defaults to `main.spark` for simplicity.
- **Three dependency sources** (implemented incrementally):
  1. `path` — local filesystem reference (Phase 1)
  2. `git` — remote repository with optional `tag`, `branch`, or `rev` (Phase 2)
  3. Version string — registry lookup (Phase 3)

### 2.2 Package Structure on Disk

```
my-web-app/
  Spark.toml
  Spark.lock               # auto-generated, committed to VCS
  main.spark
  utils/
    utils.spark
  .spark/
    packages/               # downloaded dependencies (gitignored)
      json-abc1234/
        Spark.toml
        json/
          json.spark
```

- **`.spark/packages/`** — Local cache for fetched dependencies. Gitignored, deletable,
  recreated on next build.
- **`Spark.lock`** — Records exact resolved versions/commit hashes. Committed to VCS for
  reproducible builds.
- **No global cache initially.** Each project gets its own copy. A shared cache
  (`~/.spark/cache/`) is a future optimization.

### 2.3 Import Integration

The import syntax does not change. Package names become module path prefixes:

```spark
// main.spark
import json { parse, stringify }     // from "json" dependency
import utils { formatName }          // from local module
import spark.math { sqrt }           // from stdlib
```

**Resolution order** (checked during module discovery in `spark_modules`):

1. Local modules — files in the project tree
2. Stdlib modules — `spark.*` prefix
3. Package modules — first path segment looked up in `Spark.toml` dependencies

**Name normalization:** Dependency names in `Spark.toml` use kebab-case (`json-parser`).
Import paths use snake_case (`json_parser`). The resolver normalizes automatically, following
Rust's convention.

**A package exposes its modules rooted at its own `Spark.toml`.** If the `json` package
has `json/json.spark` declaring `module json`, then `import json { parse }` resolves to
that module.

### 2.4 Dependency Resolution

**Algorithm: Minimal Version Selection (MVS)**, inspired by Go's approach.

- Each dependency specifies a minimum version.
- The resolver selects the minimum version that satisfies all constraints.
- Deterministic, no SAT solver needed.
- Conflicts produce clear diagnostics.

**Why MVS over SAT-based (Cargo)?** Spark's philosophy is explicitness. MVS is predictable —
you get the version you asked for, not the latest that happens to satisfy constraints. It is
also dramatically simpler to implement.

For path dependencies (Phase 1), no resolution algorithm is needed — just validate the path
exists and contains a `Spark.toml`.

### 2.5 New Crate: `spark_package`

Responsibilities:
- Parse `Spark.toml` (using `toml` + `serde` crates)
- Resolve dependencies (path → local, git → fetch, registry → download)
- Manage `.spark/packages/` directory
- Generate/verify `Spark.lock`
- Collect transitive link requirements
- Return source file paths and link info to the driver

**This crate runs before the compilation pipeline.** It does not touch lexing, parsing,
type checking, or codegen. It produces two outputs:

1. `Vec<(PackageName, Vec<(PathBuf, String)>)>` — package source files to compile
2. `LinkInfo { libraries: Vec<String>, search_paths: Vec<PathBuf> }` — for the linker

---

## 3. FFI Interop Design

### 3.1 The `link` Directive

A new language-level construct for declaring link dependencies, co-located with the
`extern fn` declarations that need them:

```spark
// sqlite.spark
module sqlite

link "sqlite3"

extern fn sqlite3_open(path: String): Int
extern fn sqlite3_close(handle: Int): Int
extern fn sqlite3_exec(handle: Int, sql: String): Int
```

**Syntax:**

```
link "<library-name>"
link "<library-name>" from "<search-path>"
```

The `link` directive is purely declarative — it tells the compiler which libraries to pass
to the linker. It does not affect name resolution or type checking.

**Implementation:**

- New keyword `link` in `spark_lexer/src/token.rs`
- Pre-intern `link` in `spark_intern/src/lib.rs`
- New AST node `LinkDecl` in `spark_ast/src/decl.rs`:

```rust
pub struct LinkDecl {
    pub library: Symbol,
    pub search_path: Option<Symbol>,
    pub span: Span,
}
```

- Add `links: Vec<LinkDecl>` to `Ast` in `spark_ast/src/ast.rs`
- Parser: parse `link` directives after module/using/import, before items
- Driver: collect `LinkDecl` from all parsed ASTs, merge with `Spark.toml` link info,
  pass as `-l`/`-L` flags to `cc`

**Why language-level, not just `Spark.toml`?**

Co-locating the link requirement with the `extern fn` declarations follows Spark's
explicitness principle. When you read a file and see `extern fn sqlite3_open(...)`, you
also see `link "sqlite3"` — no hunting through config files to understand where the
symbol comes from.

### 3.2 The `CPtr` Type

A new built-in type representing an opaque C pointer. It replaces the current workaround
of using `handle: Int` for opaque C handles.

**Motivation — the current workaround is leaky:**

```spark
// net.spark today — Int is semantically wrong
pub struct TcpListener {
    handle: Int          // actually a pointer-sized opaque handle
}
```

**With `CPtr`:**

```spark
pub struct TcpListener {
    handle: CPtr         // explicit: this is a C pointer
}
```

**Rules:**

1. `CPtr` is a primitive type, pointer-sized (8 bytes on 64-bit)
2. Can be stored in variables, struct fields, passed as parameters and return values
3. Can be compared with `==` and `!=`
4. Can be checked against `null` (for optional returns)
5. **Cannot be dereferenced** — you cannot read memory through a `CPtr`
6. Can only meaningfully interact via `extern fn` calls

**Implementation:**

- Pre-intern `CPtr` in `spark_intern/src/lib.rs`
- Add `Ty::CPtr` variant in `spark_types/src/ty.rs`
- Type checker: allow `CPtr` in function signatures, struct fields, variable bindings
- Codegen: map `CPtr` to Cranelift `ptr_type` (i64 on 64-bit platforms)
- No special runtime support needed — it is just a pointer-sized integer at the machine level

**No `unsafe` keyword needed.** Spark's safety model is progressive:
- Pure Spark code is fully safe (no null, no dangling references, RC-managed memory)
- `extern fn` is the opt-in safety boundary — the compiler cannot verify what happens on
  the C side
- `CPtr` cannot be dereferenced, so Spark code cannot cause memory corruption through it
- The risk is confined to extern calls, which is explicit in the source code

### 3.3 Type Mapping

| Spark Type | C Type      | Size    | Notes                      |
| ---------- | ----------- | ------- | -------------------------- |
| `Int`      | `int64_t`   | 8 bytes | Always 64-bit signed       |
| `Int32`    | `int32_t`   | 4 bytes |                            |
| `Int16`    | `int16_t`   | 2 bytes |                            |
| `Int8`     | `int8_t`    | 1 byte  |                            |
| `Float`    | `double`    | 8 bytes | Always 64-bit              |
| `Float32`  | `float`     | 4 bytes |                            |
| `Bool`     | `int8_t`    | 1 byte  | 0 or 1                     |
| `Char`     | `int32_t`   | 4 bytes | Unicode code point         |
| `String`   | `char*`     | 8 bytes | Pointer to SparkString     |
| `CPtr`     | `void*`     | 8 bytes | Opaque, non-dereferencable |
| `Void`     | `void`      | 0 bytes | No return value            |

**Limitations for MVP:**
- No struct passing by value across FFI (wrap in `CPtr` instead)
- No function pointers / callbacks (use `CPtr` as opaque callback handle)
- No variadic functions (`printf`-style — rare in modern C APIs)

### 3.4 End-to-End FFI Example

Here is what a complete SQLite binding looks like for a Spark user:

```spark
// sqlite/sqlite.spark
module sqlite

link "sqlite3"

// --- Raw extern declarations (private) ---

extern fn sqlite3_open(filename: String, db_out: CPtr): Int32
extern fn sqlite3_close(db: CPtr): Int32
extern fn sqlite3_exec(db: CPtr, sql: String, callback: CPtr, arg: CPtr, errmsg: CPtr): Int32

// --- Public safe API ---

pub struct Db {
    handle: CPtr
}

pub struct SqliteError {
    code: Int32
    message: String
}

pub fn open(path: String): Db | SqliteError {
    db: CPtr = null
    rc: Int32 = sqlite3_open(path, db)
    if rc != 0 {
        return SqliteError { code: rc, message: "failed to open database" }
    }
    return Db { handle: db }
}

pub fn close(db: Db): SqliteError? {
    rc: Int32 = sqlite3_close(db.handle)
    if rc != 0 {
        return SqliteError { code: rc, message: "failed to close database" }
    }
    return null
}

pub fn exec(db: Db, sql: String): SqliteError? {
    rc: Int32 = sqlite3_exec(db.handle, sql, null, null, null)
    if rc != 0 {
        return SqliteError { code: rc, message: "query failed" }
    }
    return null
}
```

**User's main file:**

```spark
// main.spark
import sqlite { open, close, exec, Db, SqliteError }

fn main(): Int {
    result: Db = open("test.db") to {
        SqliteError(e) -> {
            print("Error: " + e.message)
            return 1
        }
    }

    err: SqliteError? = exec(result, "CREATE TABLE users (id INTEGER, name TEXT)")
    if err != null {
        print("Query error")
    }

    close(result)
    return 0
}
```

This follows the exact same pattern as the stdlib (`math.spark`, `io.spark`, `net.spark`),
but now any developer can create these bindings for any C library.

---

## 4. Phased Implementation Plan

### Phase 1: MVP — Local Packages + Link Directive + CPtr

**Goal:** Projects can have a manifest, use path dependencies, declare link libraries,
and use opaque C pointers.

| Step | Crate(s) | Description |
| ---- | -------- | ----------- |
| 1a | `spark_package` (new) | Parse `Spark.toml` `[project]` and `[dependencies]` sections. Support `path = "..."` only. Parse `[build].links` and `[build].link-paths`. |
| 1b | `spark_lexer`, `spark_intern` | Add `link` keyword token and pre-intern it. |
| 1c | `spark_ast` | Add `LinkDecl` struct. Add `links: Vec<LinkDecl>` to `Ast`. |
| 1d | `spark_parser` | Parse `link "name"` and `link "name" from "path"` after imports, before items. |
| 1e | `spark_intern`, `spark_types` | Pre-intern `CPtr`. Add `Ty::CPtr` variant. |
| 1f | `spark_types` | Allow `CPtr` in type resolution — function params, return types, struct fields, bindings. |
| 1g | `spark_codegen` | Map `Ty::CPtr` to Cranelift `ptr_type`. |
| 1h | `spark_driver` | Look for `Spark.toml`, use `spark_package` to resolve path deps, add their sources to file discovery. Collect link info from ASTs + manifest. Extend `link_object` with `-l`/`-L` flags. |
| 1i | `spark_modules` | Accept package namespaces — prefix package sources with package name during module discovery. |
| 1j | Tests | Unit tests for TOML parsing, link directive parsing, CPtr type checking. E2E test with path dependency and import. |

### Phase 2: Git Dependencies + Lock File + CLI

**Goal:** Fetch dependencies from git. Reproducible builds via lock file.

| Step | Description |
| ---- | ----------- |
| 2a | Git fetching in `spark_package` — `git clone --depth 1` to `.spark/packages/`. |
| 2b | `Spark.lock` generation — record exact commit hashes, verify on build. |
| 2c | `spark init` CLI command — create new `Spark.toml`. |
| 2d | `spark deps` CLI command — fetch/update dependencies. |
| 2e | Transitive dependency resolution — recursively resolve deps-of-deps. |

### Phase 3: Package Registry + Versioning

**Goal:** Central registry for discovering and publishing packages.

| Step | Description |
| ---- | ----------- |
| 3a | Git-based package index (like crates.io's index repo). |
| 3b | MVS version resolution for semver constraints. |
| 3c | `spark publish` — package source as `.tar.gz` and upload. |
| 3d | `spark search` / `spark add` CLI commands. |

### Phase 4: Advanced FFI Tooling

**Goal:** Make FFI as frictionless as possible.

| Step | Description |
| ---- | ----------- |
| 4a | `spark bindgen <header.h>` — generate `.spark` from C headers. |
| 4b | Platform conditionals for `link` directives. |
| 4c | Static vs dynamic linking options. |
| 4d | `spark vendor` — copy all deps into `vendor/` for offline builds. |

---

## 5. Key Integration Points

| Component | File | Change |
| --------- | ---- | ------ |
| Lexer | `crates/spark_lexer/src/token.rs` | Add `Link` token kind |
| Interning | `crates/spark_intern/src/lib.rs` | Pre-intern `link`, `CPtr` |
| AST | `crates/spark_ast/src/decl.rs` | Add `LinkDecl` struct |
| AST | `crates/spark_ast/src/ast.rs` | Add `links: Vec<LinkDecl>` to `Ast` |
| Parser | `crates/spark_parser/src/decl.rs` | Parse `link` directives |
| Types | `crates/spark_types/src/ty.rs` | Add `Ty::CPtr` variant |
| Type checker | `crates/spark_types/src/checker.rs` | Resolve `CPtr` type references |
| Codegen | `crates/spark_codegen/src/cranelift/` | Map `CPtr` to `ptr_type` |
| Module resolver | `crates/spark_modules/src/lib.rs` | Package namespace prefixing |
| Driver | `crates/spark_driver/src/main.rs` | Manifest loading, extended linking |
| Package manager | `crates/spark_package/` (new) | Manifest parsing, dependency resolution |

---

## 6. Open Questions

1. **Should `CPtr` support arithmetic?** Some C APIs use pointer arithmetic for buffer
   offsets. For MVP, no — require calling back into C for any pointer manipulation.

2. **How should `String` work across FFI?** Currently Spark strings are RC-managed with a
   header. C functions expect `const char*`. The runtime already handles conversion for
   stdlib, but user extern fns may need explicit documentation about ownership transfer.

3. **Should packages be source-only or allow pre-compiled?** Source-only is simpler and
   matches the current stdlib model. Pre-compiled packages would speed up builds but require
   ABI stability guarantees. Recommendation: source-only for now.

4. **What about non-C FFI?** Calling C++ (name mangling), Rust (`extern "C"`), or Python
   (CPython API) all go through C ABI. The `extern fn` + `link` + `CPtr` combination handles
   all of these. No special support needed.

5. **Should `link` support conditional compilation?** Platform-specific libraries
   (e.g., `ws2_32` on Windows, `pthread` on Linux) would need conditionals. Defer to Phase 4.

---

## 7. Comparison with Other Languages

| Aspect | Spark (proposed) | Rust (Cargo) | Python (pip) | Go (modules) |
| ------ | ---------------- | ------------ | ------------ | ------------- |
| Manifest | `Spark.toml` | `Cargo.toml` | `pyproject.toml` | `go.mod` |
| Lock file | `Spark.lock` | `Cargo.lock` | `requirements.txt` | `go.sum` |
| Dep sources | path/git/registry | path/git/registry | PyPI | proxy |
| Resolution | MVS | SAT-based | Latest | MVS |
| FFI declaration | `extern fn` + `link` | `extern "C"` + `build.rs` | `ctypes`/`cffi` | `cgo` |
| Opaque pointers | `CPtr` | `*mut c_void` | `c_void_p` | `unsafe.Pointer` |
| Package format | Source | Source or binary | Wheel/sdist | Source |
| Bindgen | `spark bindgen` (Phase 4) | `bindgen` crate | `cffi` | `cgo` |

Spark's approach is closest to Go's — simple, explicit, deterministic. The key difference is
that Spark adds a language-level `link` directive rather than relying on build-system comments.

---

## Deliverables

- [x] Trade-off analysis: MVS vs SAT, source vs binary, language-level vs manifest-only link
- [x] Complete manifest format specification
- [x] FFI interop design with `CPtr` and `link` directive
- [x] Phased implementation plan with crate-level breakdown
- [ ] Implementation (tracked as separate tasks per phase)
