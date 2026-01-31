# spark_diagnostics

Error reporting and diagnostics for the Spark compiler.

## Responsibility

This crate handles **compiler diagnostics** — the errors, warnings, and suggestions that the compiler produces. It provides:

1. **Diagnostic types** — Structured representation of errors with source locations
2. **Diagnostic builder** — Ergonomic API for constructing diagnostics
3. **Rendering** — Beautiful terminal output using the `ariadne` crate
4. **Collection** — Accumulating diagnostics during compilation phases

### Core Types

| Type | Purpose |
|------|---------|
| `Diagnostic` | A single error, warning, or note with source locations |
| `Level` | Severity: `Error`, `Warning`, `Note`, `Help` |
| `Label` | A span with an associated message |
| `Suggestion` | A proposed code fix |
| `DiagnosticCode` | Error code like `E0001` for documentation lookup |
| `Diagnostics` | Collection of diagnostics from a compilation phase |

### Key Operations

```rust
use spark_diagnostics::{Diagnostic, Level, Label};
use spark_span::Span;

// Build a diagnostic
let diag = Diagnostic::error("mismatched types")
    .with_code("E0308")
    .with_label(Label::primary(span, "expected `Int`, found `String`"))
    .with_label(Label::secondary(other_span, "expected due to this"))
    .with_note("consider using `.toInt()` to convert");

// Render to terminal
diag.emit(&source_map);
```

## Relationship to Other Crates

```
                    spark_span
                        │
                        ▼
               spark_diagnostics (this crate)
                        │
        ┌───────────────┼───────────────┐
        ▼               ▼               ▼
   spark_lexer    spark_parser    spark_types
        │               │               │
        └───────────────┴───────────────┘
                        │
                        ▼
                  spark_driver (collects all diagnostics)
```

**Dependencies:**
- `spark_span` — For `Span`, `FileId`, and `SourceMap`

**Dependents:**
- Every compiler phase that can produce errors (lexer, parser, type checker, etc.)
- `spark_driver` collects diagnostics from all phases

## Design Decisions

### Structured Diagnostics

Diagnostics are structured data, not just strings. This enables:
- Consistent formatting across the compiler
- Machine-readable output (JSON) for IDEs
- Rich source annotations with multiple spans

### Builder Pattern

Diagnostics use a builder pattern for ergonomic construction:
```rust
Diagnostic::error("message")
    .with_code("E0001")
    .with_label(...)
    .with_suggestion(...)
```

### Deferred Rendering

Diagnostics are collected during compilation and rendered at the end. This allows:
- Sorting by severity or location
- Deduplication of similar errors
- Limiting output (e.g., "and 5 more errors")

### Error Codes

Each diagnostic can have an error code (e.g., `E0308`). These:
- Help users search for documentation
- Enable `--explain E0308` style help
- Make errors recognizable across versions

## Diagnostic Levels

| Level | Meaning | Stops compilation? |
|-------|---------|-------------------|
| `Error` | Invalid code that cannot compile | Yes |
| `Warning` | Suspicious code that compiles | No |
| `Note` | Additional context for another diagnostic | No |
| `Help` | Suggestion for fixing an error | No |

## Example Output

```
error[E0308]: mismatched types
  ┌─ src/main.spark:10:12
  │
8 │ fn greet(name: String): Int {
  │                         --- expected `Int` because of return type
  │
10│     return name
  │            ^^^^ expected `Int`, found `String`
  │
  = help: consider using `name.length` to get an Int
```

## Thread Safety

`Diagnostics` (the collection type) is designed to work with single-threaded compilation. For parallel compilation, each thread should collect diagnostics separately and merge them at the end.
