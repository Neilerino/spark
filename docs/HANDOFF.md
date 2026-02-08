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
| `spark_drop`        | Type queries for RC/drop decisions (`needs_rc`, etc.)          | 11    | Complete |
| `spark_runtime`     | `extern "C"` runtime library (print, strings, collections, RC) | —     | Complete |
| `spark_driver`      | CLI driver (`spark build` / `spark run`)                       | —     | Complete |

**Total: 568 tests passing across workspace.**

All crates pass `cargo test` and `cargo clippy` (pedantic + nursery).

### Runtime & Driver

- **`spark_runtime`**: Static library (`libspark_runtime.a`) with `extern "C"` FFI functions for print, strings, collections, and reference counting. All heap types use custom `repr(C)` structs with `rc: usize` as the first field. `SparkString` manages its own UTF-8 buffer (`{ rc, data, len, cap }`). `__spark_string_from_literal(ptr, len)` creates a heap string; `__spark_print_string/int/bool/float` dispatch at codegen time based on argument type. Collections use type-erased backing: `SparkList` (growable array), `SparkMap` (linear-scan), `SparkSet` (list + dedup). All parameterized by element byte size. RC operations: `__spark_rc_inc(ptr)` is generic (rc at offset 0); type-specific `_rc_dec` functions handle nested element cleanup and freeing. COW copy functions prepared for future mutation support.
- **`spark_driver`**: CLI binary (`spark build <file> [-o out]`, `spark run <file>`). Chains the full pipeline, writes the object file to a temp path, invokes `cc` to link against `libspark_runtime.a`, and optionally runs the result.
- **`install.sh`**: Builds in release mode and copies `spark` + `libspark_runtime.a` to a prefix directory (default `~/.local`).

### Recent: RC Infrastructure & Drop Elaboration

COW value semantics with reference counting (Swift model). All heap-allocated values now properly freed.

**New crate:**
- `spark_drop`: Type query functions — `needs_rc(ty, table)`, `element_needs_rc()`, `map_key_val_needs_rc()`. Determines which types need RC operations. Used by MIR lowering and codegen.

**Changes made:**
- `spark_runtime/src/lib.rs`: Replaced `Box<String>` with custom `SparkString { rc, data, len, cap }`. Added `rc: usize` as first field to all heap types. All `_new` functions initialize `rc: 1`. Added `__spark_rc_inc(ptr)` (generic), type-specific `_rc_dec` functions with nested element cleanup, and COW copy functions (prepared for future use).
- `spark_mir/src/builder.rs`: Added `ScopeContext` tracking with `locals_to_drop` for LIFO drop elaboration. Methods: `push_scope()`, `pop_scope_drops()`, `register_drop()`, `drops_for_return()`, `drops_to_loop()`.
- `spark_mir/src/lower_stmt.rs`: Registers RC'd locals for drop on `Let`. Drops old value before reassignment on `Assign`. Drops discarded RC'd temps on `Expr`.
- `spark_mir/src/lower_expr.rs`: Blocks push/pop scopes. Return/break/continue emit drops before jumping.
- `spark_mir/src/lower_function.rs`: RC'd parameters registered for drop (caller inc'd, callee dec's). Fallthrough path emits drops before Return.
- `spark_codegen/cranelift/runtime.rs`: Declared 8 new stubs: `rc_inc`, `string_rc_dec`, `list_rc_dec`, `map_rc_dec`, `set_rc_dec`, `string_cow`, `list_cow`, `map_cow`.
- `spark_codegen/cranelift/emit_stmt.rs`: `Statement::Drop` now emits type-dispatched `_rc_dec` calls (was no-op). `Operand::Copy` emits `rc_inc` for RC'd types.
- `spark_codegen/cranelift/emit_rvalue.rs`: `Operand::Copy` emits `__spark_rc_inc` for RC'd types. `Operand::Move` transfers without RC change.

### Earlier: Collection Support (List, Map, Set)

Collection literals and index access work end-to-end. `[10, 20, 30]` creates a real heap-backed list.

### Earlier: Real String Support

String literals work end-to-end. `print("hello world")` produces output.

## What's Next

### Suggested Next Steps (in rough priority order)

1. **COW mutation hooks** — Runtime COW functions (`__spark_string_cow`, `__spark_list_cow`, `__spark_map_cow`) are prepared but not wired into codegen. Wire up when mutable collection operations are implemented: before any mutation, emit COW uniqueness check (if RC>1, copy buffer first).
2. **Collection methods** — `.append()`, `.len()`, `.remove()`, `.contains()`, etc. Need method resolution infrastructure or special-cased built-in dispatch (like `print`). Each mutation method should emit COW check first.
3. **`??` / `?.` desugaring** — HIR stubs exist (`desugar.rs:35`, `desugar.rs:80`) but need a `DefId` allocator for synthetic bindings.
4. **`is` type test** — MIR stub (`lower_expr.rs:289`). Lower to discriminant extraction + comparison. Codegen already handles `Rvalue::Discriminant` and `SwitchInt`.
5. **Match exhaustiveness checking** — Would go in `spark_types`. Report missing/unreachable patterns.
6. **Method resolution & interface bindings** — Dot-call syntax, `binding Foo for Bar` validation. Prerequisite for collection methods via normal syntax.
7. **`!` propagation operator** — Needs union type analysis to distinguish success/error variants.
8. **Generic instantiation** — `Ty::Param` exists but isn't used. Needs monomorphization.
9. **`spark_opt`** — MIR optimization passes (constant folding, DCE, SSA, copy propagation, RC elision). Inserts between borrow and codegen. Performance-only; correctness is unaffected.
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
10. Runtime (spark_runtime) ✅ (strings, print, collections, RC implemented)
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
spark_drop          (depends on types)
spark_mir           (depends on span, intern, diagnostics, ast, resolve, types, hir, drop)
spark_borrow        (depends on span, intern, diagnostics, resolve, types, mir)
spark_codegen       (depends on span, intern, diagnostics, ast, resolve, types, mir, drop + cranelift)
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
11. **Reference counting** - COW value semantics (Swift model). `spark_drop::needs_rc()` determines which types need RC. `Operand::Copy` emits `rc_inc`; `Operand::Move` does not. `Statement::Drop` emits type-specific `_rc_dec`. Scope tracking in MIR builder handles drop elaboration at scope exits, returns, break/continue. Parameters are RC'd (caller inc's, callee dec's at scope exit).

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
cargo test                       # Test all (568 tests)
cargo clippy                     # Lint all
cargo test -p spark_codegen      # Test code generator
cargo doc --open                 # View documentation
```
