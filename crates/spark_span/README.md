# spark_span

Source location tracking for the Spark compiler.

## Responsibility

This crate provides the foundational types for tracking **where** things are in source code. It answers the question: "What file and what byte range does this AST node / token / error come from?"

### Core Types

| Type | Purpose |
|------|---------|
| `FileId` | Unique identifier for a source file |
| `Span` | A contiguous range in source code (file + byte offsets) |
| `Spanned<T>` | Wrapper that attaches a `Span` to any value |
| `SourceMap` | Registry of all source files; enables looking up source text |

### Key Operations

- **Creating spans**: `Span::new(file, start, end)`
- **Combining spans**: `span1.merge(span2)` — useful for AST nodes that span multiple tokens
- **Looking up source**: `source_map.source(span)` — get the actual source text for a span
- **Line/column info**: `source_map.line_col(span.start)` — for error messages

## Relationship to Other Crates

```
spark_span (this crate)
    │
    ├──► spark_intern     (no dependency)
    │
    ├──► spark_diagnostics (uses Span for error locations)
    │
    ├──► spark_lexer      (tokens have Spans)
    │
    ├──► spark_ast        (all AST nodes have Spans)
    │
    └──► [all other crates that deal with source locations]
```

**This crate has no dependencies on other Spark crates.** It is the foundation that everything else builds upon.

## Design Decisions

### Byte Offsets, Not Line/Column

Spans use byte offsets because:
1. They're cheap to store (just two `u32`s)
2. They're what the lexer naturally produces
3. Line/column can be computed on-demand from the `SourceMap`

### `u32` for Offsets

We use `u32` for file IDs and byte offsets, limiting files to ~4GB. This is a reasonable trade-off for reduced memory usage in AST nodes (which are allocated by the millions in large codebases).

### Copy Semantics

`FileId` and `Span` are `Copy` types — they're small enough to pass by value efficiently, and this makes them easier to work with throughout the compiler.

## Example Usage

```rust
use spark_span::{FileId, Span, SourceMap};

// Create a source map and add a file
let mut source_map = SourceMap::new();
let file_id = source_map.add_file("main.spark", "fn main(): Void { }");

// Create a span for "main" (bytes 3..7)
let span = Span::new(file_id, 3, 7);

// Look up the source text
assert_eq!(source_map.source(span), "main");

// Get line/column for error messages
let (line, col) = source_map.line_col(file_id, span.start);
assert_eq!((line, col), (1, 4)); // 1-indexed
```
