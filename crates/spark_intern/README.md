# spark_intern

String interning for the Spark compiler.

## Responsibility

This crate provides **string interning** — the process of storing unique copies of strings and referring to them by small integer identifiers. This is a fundamental optimization for compilers where:

1. **Identifiers are compared frequently** — Integer comparison is O(1) vs string comparison O(n)
2. **Many strings are duplicated** — Variable names appear many times; interning deduplicates them
3. **Symbols are Copy** — Unlike `String`, a `Symbol` can be cheaply copied everywhere

### Core Types

| Type | Purpose |
|------|---------|
| `Symbol` | A 32-bit identifier representing an interned string |
| `Interner` | The storage that maps strings ↔ symbols |
| `INTERNER` | Thread-local global interner for convenience |

### Key Operations

```rust
use spark_intern::{sym, Symbol};

// Intern a string (returns Symbol)
let foo: Symbol = sym("foo");

// Get the string back
assert_eq!(foo.as_str(), "foo");

// Symbols with the same string are equal
let foo2 = sym("foo");
assert_eq!(foo, foo2);

// Symbols are Copy and cheap to compare
fn uses_symbol(s: Symbol) {
    if s == sym("bar") { /* ... */ }
}
```

## Relationship to Other Crates

```
spark_intern (this crate)
    │
    ├──► spark_span       (no dependency)
    │
    ├──► spark_lexer      (identifiers become Symbols)
    │
    ├──► spark_ast        (names in AST are Symbols)
    │
    ├──► spark_resolve    (definition names are Symbols)
    │
    └──► [all crates that deal with identifiers]
```

**This crate has no dependencies on other Spark crates.** Like `spark_span`, it is foundational infrastructure.

## Design Decisions

### Thread-Local Global Interner

We use a thread-local global interner accessed via the `sym()` function. This is the most ergonomic API for a single-threaded compiler. If we need multi-threaded interning later, we can switch to a synchronized interner.

**Why not pass an `&mut Interner` everywhere?**
- It would pollute every function signature
- The lexer, parser, and most passes would all need access
- A global is simpler and sufficient for our use case

### `u32` Symbol IDs

Symbols use `u32` internally, allowing ~4 billion unique strings. This is more than enough for any reasonable codebase while keeping `Symbol` small (4 bytes vs 24 bytes for `String` on 64-bit).

### Pre-Interned Keywords

Common strings (keywords, operators) are pre-interned at startup for efficiency. These can be accessed via constants like `kw::FN`, `kw::IF`, etc.

### No Deallocation

Interned strings are never freed during compilation. This simplifies the implementation and is fine because:
- Compilation is finite
- The total unique string count is bounded by codebase size
- Memory is reclaimed when the process exits

## Example Usage

```rust
use spark_intern::{sym, Symbol, kw};

// Intern strings
let name = sym("myFunction");
let param = sym("x");

// Compare efficiently
if name == sym("myFunction") {
    println!("Found it!");
}

// Keywords are pre-interned
let fn_keyword = kw::FN;
assert_eq!(fn_keyword.as_str(), "fn");

// Get string back when needed
println!("Function name: {}", name.as_str());
```

## Thread Safety

The default interner is thread-local. Each thread has its own interner, which means:
- Symbols from different threads are **not** comparable
- This is fine for a single-threaded compiler
- Multi-threaded builds should use separate interners per compilation unit

For explicit multi-threaded use, create and pass around an `Interner` directly.
