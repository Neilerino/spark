# Spark Compiler - Technical Handoff

## Project Overview

Spark is a statically-typed programming language ("lower-level Python") with:

- Immutability by default
- Union types for error handling (no exceptions)
- Opt-in borrow checking for performance-critical code
- Clean, Python-like syntax

## Completed Work

### All Crates

| Crate               | Purpose                                                        | Tests | Status   |
| ------------------- | -------------------------------------------------------------- | ----- | -------- |
| `spark_span`        | Source locations (`FileId`, `Span`, `SourceMap`)               | 13    | Complete |
| `spark_intern`      | String interning (`Symbol`, `Interner`, pre-interned keywords) | 14    | Complete |
| `spark_diagnostics` | Error reporting with ariadne rendering                         | 12    | Complete |
| `spark_lexer`       | Tokenization (keywords, literals, operators, comments)         | 33    | Complete |
| `spark_ast`         | AST node definitions for all language constructs               | 53    | Complete |
| `spark_parser`      | Recursive descent parser with Pratt expression parsing         | 125   | Complete |
| `spark_modules`     | Module resolution, dependency graph, cycle detection           | 38    | Complete |
| `spark_resolve`     | Name resolution: scoping, DefIds, imports                      | 51    | Complete |
| `spark_types`       | Type checking: two-pass signature collection + body checking   | 72    | Complete |
| `spark_hir`         | HIR lowering: desugar typed AST into core IR                   | 21    | Complete |
| `spark_mir`         | MIR construction: CFG-based IR for borrow checking             | 32    | Complete |
| `spark_borrow`      | Borrow checking: NLL-style region inference over MIR CFG       | 27    | Complete |
| `spark_codegen`     | Native code generation via Cranelift                           | 35    | Complete |
| `spark_runtime`     | `extern "C"` runtime library (print, strings)                  | —     | Complete |
| `spark_driver`      | CLI driver (`spark build` / `spark run`)                       | —     | Complete |

**Total: 540+ tests passing across workspace.**

All crates pass `cargo test` and `cargo clippy` (pedantic + nursery).

### Runtime & Driver

- **`spark_runtime`**: Static library (`libspark_runtime.a`) with `extern "C"` FFI functions for print, strings, and collection/RC stubs. Strings are `Box<String>` behind an opaque `*mut u8` pointer. `__spark_string_from_literal(ptr, len)` creates a heap string; `__spark_print_string/int/bool/float` dispatch at codegen time based on argument type.
- **`spark_driver`**: CLI binary (`spark build <file> [-o out]`, `spark run <file>`). Chains the full pipeline, writes the object file to a temp path, invokes `cc` to link against `libspark_runtime.a`, and optionally runs the result.
- **`install.sh`**: Builds in release mode and copies `spark` + `libspark_runtime.a` to a prefix directory (default `~/.local`).

### Recent: Real String Support

String literals now work end-to-end. `print("hello world")` produces output.

**Changes made:**
- `spark_runtime`: `__spark_string_new` / `__spark_string_from_literal` allocate real `Box<String>`; added `__spark_print_string`
- `spark_codegen/cranelift/runtime.rs`: Declared `print_string`, `print_bool`, `print_float` stubs
- `spark_codegen/cranelift/context.rs`: `FunctionContext` now carries `&RuntimeStubs`
- `spark_codegen/cranelift/emit_rvalue.rs`: `Constant::String` calls `__spark_string_from_literal(ptr, len)` instead of returning a raw pointer
- `spark_codegen/cranelift/emit_term.rs`: Built-in `print` dispatches by argument type (`Ty::String` → `print_string`, `Ty::Bool` → `print_bool`, `Ty::Float` → `print_float`, default → `print_int`). Uses `resolve_callee_def_id()` to trace function references through MIR locals.
- `spark_types/checker.rs`: `print` signature broadened from `(Int): Void` to `(Int | Float | Bool | String): Void`

## What's Next

### Suggested Next Steps (in rough priority order)

1. **Runtime: collection functions** — `List`, `Map`, `Set` stubs in `spark_runtime` are still placeholders (return null). Implement real backing (`Vec<u8>` type-erased or monomorphized) and wire through codegen like strings.
2. **Drop elaboration** — `Statement::Drop` exists in MIR but is a no-op in codegen (`emit_stmt.rs:56`). Insert drops at scope exits, emit `__spark_rc_release` for heap types. Needed to avoid leaking strings/collections.
3. **`??` / `?.` desugaring** — HIR stubs exist (`desugar.rs:35`, `desugar.rs:80`) but need a `DefId` allocator for synthetic bindings.
4. **`is` type test** — MIR stub (`lower_expr.rs:289`). Lower to discriminant extraction + comparison. Codegen already handles `Rvalue::Discriminant` and `SwitchInt`.
5. **Match exhaustiveness checking** — Would go in `spark_types`. Report missing/unreachable patterns.
6. **Method resolution & interface bindings** — Dot-call syntax, `binding Foo for Bar` validation.
7. **`!` propagation operator** — Needs union type analysis to distinguish success/error variants.
8. **Generic instantiation** — `Ty::Param` exists but isn't used. Needs monomorphization.
9. **`spark_opt`** — MIR optimization passes (constant folding, DCE, SSA, copy propagation). Inserts between borrow and codegen. Performance-only; correctness is unaffected.
10. **Lambda/closure conversion** — Capture analysis, environment structs, borrow checker support.
11. **Async/await/spawn** — Future types, state machine transform, runtime executor.
12. **String interpolation** — Lexer + parser changes for `"hello {expr}"` syntax.

### Deferred Design Decisions

- Reborrowing / two-phase borrows (borrow checker rejects; workarounds exist)
- Explicit lifetime parameter syntax (`fn foo[A](...)`)
- Lifetime elision rules (all lifetimes are `LifetimeId::ANONYMOUS` currently)

### Full Pipeline

1. Lexer (spark_lexer) ✅
2. Parser (spark_parser) ✅
3. Module Resolution (spark_modules) ✅
4. Name Resolution (spark_resolve) ✅
5. Type Checking (spark_types) ✅
6. HIR Lowering (spark_hir) ✅
7. MIR Construction (spark_mir) ✅
8. Borrow Checking (spark_borrow) ✅
9. Code Generation (spark_codegen) ✅
10. Runtime (spark_runtime) ✅ (strings + print working; collections stubbed)
11. Driver (spark_driver) ✅ (`spark build` / `spark run`)
12. Optimization (spark_opt) — not started, optional

## Crate Dependencies

```
spark_span          (no dependencies)
spark_intern        (no dependencies)
spark_diagnostics   (depends on spark_span)
spark_lexer         (depends on span, intern, diagnostics)
spark_ast           (depends on span, intern)
spark_parser        (depends on span, intern, diagnostics, lexer, ast)
spark_modules       (depends on span, intern, diagnostics, ast)
spark_resolve       (depends on span, intern, diagnostics, ast, modules)
spark_types         (depends on span, intern, diagnostics, ast, modules, resolve)
spark_hir           (depends on span, intern, diagnostics, ast, modules, resolve, types)
spark_mir           (depends on span, intern, diagnostics, ast, resolve, types, hir)
spark_borrow        (depends on span, intern, diagnostics, resolve, types, mir)
spark_codegen       (depends on span, intern, diagnostics, ast, resolve, types, mir + cranelift)
spark_runtime       (no internal dependencies — standalone staticlib)
spark_driver        (depends on all pipeline crates + codegen + clap)
```

## Key Implementation Details

1. **Pre-interned keywords** - `spark_intern::kw` module has constants like `kw::FN`, `kw::STRUCT`, plus `kw::is_builtin_type()`
2. **Lexer API** - `Lexer::new(file_id, source).tokenize()` (no free function)
3. **Error recovery** - `ExprKind::Error`, `TypeExprKind::Error`, `PatternKind::Error`, `TypeId::ERROR`, `HirExprKind::Error` nodes
4. **Visitor pattern** - `spark_ast::Visitor` trait uses `&self` (read-only). For mutable walks, use direct recursive methods (as resolver and type checker do).
5. **Borrow conflict pattern** - Use `std::mem::take` to temporarily move `self.asts`/`self.resolutions` when methods need `&mut self` alongside shared AST data.
6. **Parser limitation** - Deeply nested bare blocks with bindings (`{ { a: Int = 1 } }`) can misparse. Use `if`-blocks in tests to avoid this.
7. **Pipeline API** - Each crate: `Input` struct → public `lower()`/`check()` fn → `Output` struct. Output passes `TypeTable` through for subsequent phases.
8. **Double-parse workaround** - Type checker consumes ASTs, so test helpers re-parse + re-resolve for HIR/MIR input.
9. **Lifetime system** - `LifetimeId` in `Ty::Ref`, interned in `TypeTable`. `LifetimeId::ANONYMOUS` (0) is the default inferred lifetime. Parser already extracts lifetimes from `Ref[T, A]` syntax.
10. **Borrow checker architecture** - NLL-style: fact extraction → liveness → region inference → conflict detection. Functions without `Ref` types skip the analysis entirely.

## Notes for Next Agent

1. **Read `docs/lang-spec.md`** - Full grammar and language semantics
2. **Read `docs/compiler-architecture.md`** - Pipeline design with I/O schemas for each phase
3. **Read `docs/roadmap.md`** - Prioritized list of remaining work with status
4. **All phases use `Diagnostics`** for error accumulation — never panic on invalid input
5. **`#![forbid(unsafe_code)]`** in every crate (except `spark_runtime`), clippy pedantic + nursery lints
6. **MIR structure**: Each `MirFunction` has `locals: Vec<LocalDecl>` and `blocks: Vec<BasicBlock>`. Iterate blocks in order; each block has `statements` + `terminator`. Use `pretty_function()` to dump MIR for debugging.
7. **Built-in function dispatch**: `print` is handled specially in codegen — `resolve_callee_def_id()` traces the callee through MIR locals, then `emit_builtin_print()` dispatches by argument type. New built-in functions should follow this pattern.

## Installing & Running Spark Programs

### Quick start (from source tree)

```bash
cargo build -p spark_driver -p spark_runtime

# Compile and run a .spark file (runtime lib found automatically next to binary)
cargo run -p spark_driver -- run hello.spark

# Or compile to an executable
cargo run -p spark_driver -- build hello.spark -o hello
./hello
```

### Install system-wide

```bash
./install.sh              # installs to ~/.local/bin + ~/.local/lib
./install.sh /usr/local   # installs to /usr/local/bin + /usr/local/lib
```

After installing, set `SPARK_RUNTIME_DIR` so the compiler can find `libspark_runtime.a`:

```bash
export SPARK_RUNTIME_DIR="$HOME/.local/lib"   # add to your shell profile
```

Then:

```bash
spark run hello.spark          # compile + run
spark build hello.spark -o hello   # compile to executable
```

### What `print` supports

`print` accepts `Int`, `Float`, `Bool`, or `String`:

```
fn main(): Int {
    print("hello world")
    print(42)
    print(3.14)
    print(true)
    0
}
```

### Requirements

- Rust toolchain (for building from source)
- A C compiler accessible as `cc` (Xcode CLI tools on macOS, `gcc`/`clang` on Linux) — used by the linker step

## Build Commands

```bash
cargo build                      # Build all
cargo test                       # Test all (540+ tests)
cargo clippy                     # Lint all
cargo test -p spark_codegen      # Test code generator
cargo doc --open                 # View documentation
```
