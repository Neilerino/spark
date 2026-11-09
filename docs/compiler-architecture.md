# Compiler architecture

[The driver pipeline](../crates/spark_driver/src/pipeline.rs) is the entry point for compiling a Spark program. The [CLI](../crates/spark_driver/src/main.rs) discovers inputs, invokes the pipeline, and links the resulting object with the runtime archive and any declared native libraries. The [workspace manifest](../Cargo.toml) lists the crates and their dependencies.

## Compilation stages

| Stage | Owner and output |
| --- | --- |
| Lex and parse | [spark_lexer](../crates/spark_lexer/src/lib.rs) and [spark_parser](../crates/spark_parser/src/lib.rs) turn source into tokens and then a [syntax tree](../crates/spark_ast/src/lib.rs). |
| Load and resolve | [spark_modules](../crates/spark_modules/src/lib.rs) builds the module graph. [spark_resolve](../crates/spark_resolve/src/lib.rs) associates names with definitions. |
| Check types | [spark_types](../crates/spark_types/src/lib.rs) checks declarations and bodies. The [type table](../crates/spark_types/src/table.rs) records resolved types and the results of type checking. |
| Lower to HIR | [spark_hir](../crates/spark_hir/src/lib.rs) translates typed syntax and desugars high-level expressions. |
| Lower to MIR | [spark_mir](../crates/spark_mir/src/lib.rs) constructs control-flow graphs and specializes generic functions. [The MIR definitions](../crates/spark_mir/src/mir.rs) describe locals, places, operands, statements, and terminators. |
| Check borrows and transform async functions | [spark_borrow](../crates/spark_borrow/src/lib.rs) checks the lowered program. [The async transform](../crates/spark_mir/src/async_transform.rs) builds poll functions and state machines. |
| Optimize and validate | [spark_opt](../crates/spark_opt/src/lib.rs) applies optional MIR passes. The driver runs [MIR validation](../crates/spark_mir/src/validate.rs) in debug builds before code generation. |
| Emit native code | [spark_codegen](../crates/spark_codegen/src/lib.rs) emits a native object for the host using [Cranelift](../crates/spark_codegen/src/cranelift/mod.rs). The driver links the object into an executable. |

The front end tracks source locations with [spark_span](../crates/spark_span/src/lib.rs), interns names with [spark_intern](../crates/spark_intern/src/lib.rs), and reports source diagnostics through [spark_diagnostics](../crates/spark_diagnostics/src/lib.rs). For operator precedence, consult the [parser's expression binding powers](../crates/spark_parser/src/expr.rs).

## Runtime and representation

[spark_stdlib](../crates/spark_stdlib/src/lib.rs) supplies bundled Spark sources that the driver includes when compiling. [spark_runtime](../crates/spark_runtime/src/lib.rs) provides the native entry points linked into the executable. Build the runtime alongside the compiler to keep their ABI in sync.

- [spark_abi](../crates/spark_abi/src/lib.rs) holds shared ABI definitions. Codegen owns [native value layout](../crates/spark_codegen/src/layout.rs) and [ABI lowering](../crates/spark_codegen/src/abi.rs).
- [spark_drop](../crates/spark_drop/src/lib.rs) answers ownership and drop questions for compiler types. Codegen and runtime cooperate on retention and destruction, including reference-counted collections and closure captures.
- Collection storage depends on the slot layout and ownership information. Changes to element types, tagged unions, or references must preserve the storage representation or explicitly convert the value before crossing the boundary.

The [package crate](../crates/spark_package/src/lib.rs) supports package manifests and native linking configuration. [spark_bindgen](../crates/spark_bindgen/src/lib.rs) handles C header bindings. The [language server](../tools/spark-lsp/src/main.rs) uses the compiler front end for editor features.

## Development checks

Run these from the repository root with the toolchain in [rust-toolchain.toml](../rust-toolchain.toml). These are the commands run by [CI](../.github/workflows/ci.yml).

```sh
cargo fmt --all -- --check
cargo clippy --all-features --workspace -- -D warnings
cargo test --all-features --workspace --exclude spark_driver
cargo build -p spark_driver -p spark_runtime
SPARK_RUNTIME_DIR=target/debug cargo test --all-features -p spark_driver --test e2e -- --test-threads=1
```

Use [driver tests](../crates/spark_driver/tests/e2e.rs) for behavior that crosses stages, including emitted diagnostics and native execution. Use tests in the owning crate for representation invariants or phase-local regressions. For changes to optimization or native lowering, exercise both default compilation and `--no-opt` when the behavior should agree.
