# Spark Compiler - Technical Handoff

## Project Overview

Spark is a statically-typed programming language ("lower-level Python") with:

- Immutability by default
- Union types for error handling (no exceptions)
- Opt-in borrow checking for performance-critical code
- Clean, Python-like syntax

## Completed Work

### Foundation Crates (8/10 compiler phases)

| Crate               | Purpose                                                        | Tests | Status      |
| ------------------- | -------------------------------------------------------------- | ----- | ----------- |
| `spark_span`        | Source locations (`FileId`, `Span`, `SourceMap`)               | 13    | Complete |
| `spark_intern`      | String interning (`Symbol`, `Interner`, pre-interned keywords) | 14    | Complete |
| `spark_diagnostics` | Error reporting with ariadne rendering                         | 12    | Complete |
| `spark_lexer`       | Tokenization (keywords, literals, operators, comments)         | 33    | Complete |
| `spark_ast`         | AST node definitions for all language constructs               | 53    | Complete |
| `spark_parser`      | Recursive descent parser with Pratt expression parsing         | 125   | Complete |
| `spark_modules`     | Module resolution, dependency graph, cycle detection           | 38    | Complete |
| `spark_resolve`     | Name resolution: scoping, DefIds, imports                      | 51    | Complete |

**Total: 341 tests passing across workspace.**

All crates pass `cargo test` and `cargo clippy` (pedantic + nursery).

### spark_resolve Overview (Just Completed)

Two-pass name resolution that binds every identifier to its definition:

- **Collection pass**: Registers all top-level defs (functions, structs, enums, interfaces, type aliases, consts), struct fields, enum variants, import bindings. Structs/Enums go in both value and type namespaces.
- **Resolution pass**: Walks all function bodies, expressions (35+ ExprKind variants), statements, type expressions, patterns. Resolves identifiers via `RibStack` lookup.
- **Scoping**: `RibStack` with dual namespaces (value + type), push/pop ribs for blocks, functions, generics, lambdas.
- **Built-in types**: `Int`, `String`, `Bool`, etc. silently skipped via `kw::is_builtin_type()` — deferred to `spark_types`.
- **Error reporting**: Undefined name (E0425), undefined type (E0412), duplicate definition (E0428).
- **Imports**: Cross-module name resolution via `ModuleGraph` exports.

**Key types**: `DefId`, `DefinitionTable`, `DefinitionKind`, `RibStack`, `ResolutionMap`, `ResolvedAst`.

**API**: `resolve(ResolverInput) -> ResolverOutput` where input is `{graph: ModuleGraph, asts: HashMap<FileId, Ast>}` and output is `{resolved: ResolvedAst, definitions: DefinitionTable, diagnostics: Diagnostics}`.

## What's Next

### Immediate: spark_types (NEXT MAIN TASK)

**Goal**: Type checking and inference.

**Core Tasks**:

1. **Type representation** - Internal type system (primitives, structs, enums, unions, optionals, functions, generics)
2. **Type inference** - Infer types for locals, lambda params, generic instantiation
3. **Type checking** - Verify assignments, function calls, operators, returns
4. **Union type handling** - Spark uses `Int | Error` instead of exceptions
5. **Generic instantiation** - Monomorphization or type erasure strategy
6. **Struct field / enum variant resolution** - Type-directed lookup using `DefId`s from resolver
7. **Interface conformance** - Check `binding` implementations satisfy interfaces

**Key Design Decisions**:

- **Type representation**: How to represent types internally (interned? arena-allocated?)
- **Inference strategy**: Hindley-Milner style or simpler forward-only inference?
- **Error type handling**: How union types interact with the `to` operator and error propagation

**Reference**: See Phase 5 in `docs/compiler-architecture.md` for detailed requirements.

**Available from resolver**: `DefinitionTable` maps every `DefId` to its `DefinitionInfo` (kind, name, span, visibility, module). `ResolutionMap` maps AST spans to their resolved `DefId`s.

### Full Pipeline

1. Lexer (spark_lexer)
2. Parser (spark_parser)
3. Module Resolution (spark_modules)
4. Name Resolution (spark_resolve)
5. **Type Checking (spark_types) <- NEXT**
6. HIR Lowering (spark_hir)
7. MIR Lowering (spark_mir)
8. Borrow Checking (spark_borrow)
9. Optimization (spark_opt)
10. Code Generation (spark_codegen)

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
spark_types         (will depend on all above)
```

## Key Implementation Details

1. **Pre-interned keywords** - `spark_intern::kw` module has constants like `kw::FN`, `kw::STRUCT`, plus `kw::is_builtin_type()`
2. **Lexer API** - `Lexer::new(file_id, source).tokenize()` (no free function)
3. **Error recovery** - `ExprKind::Error`, `TypeExprKind::Error`, `PatternKind::Error` nodes
4. **Visitor pattern** - `spark_ast::Visitor` trait uses `&self` (read-only). For mutable walks, use direct recursive methods (as resolver does).
5. **Parser limitation** - Deeply nested bare blocks with bindings (`{ { a: Int = 1 } }`) can misparse. Use `if`-blocks in tests to avoid this.

## Notes for Next Agent

1. **Read `docs/lang-spec.md`** - Full grammar and language semantics
2. **Read `docs/compiler-architecture.md`** - Pipeline design with I/O schemas for each phase
3. **The resolver output** is the starting point for type checking
4. **All phases use `Diagnostics`** for error accumulation — never panic on invalid input
5. **`#![forbid(unsafe_code)]`** in every crate, clippy pedantic + nursery lints

## Build Commands

```bash
cargo build                      # Build all
cargo test                       # Test all (341 tests)
cargo clippy                     # Lint all
cargo test -p spark_resolve      # Test specific crate
cargo doc --open                 # View documentation
```
