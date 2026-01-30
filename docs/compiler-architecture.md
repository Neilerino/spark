# Compiler Architecture Document

## Table of Contents

1. [Overview](#overview)
2. [High-Level Architecture](#high-level-architecture)
3. [Project Structure](#project-structure)
4. [Compilation Phases](#compilation-phases)
5. [Development Roadmap](#development-roadmap)

---

## Overview

This document outlines the architecture for building the language compiler in Rust. The compiler follows a traditional multi-phase pipeline architecture.

### Design Goals

1. **Correctness** — The compiler must correctly implement the language specification
2. **Clear error messages** — Errors should be helpful and point to exact locations
3. **Incremental compilation** — Support for fast recompilation of changed files
4. **Extensibility** — Easy to add new features and optimizations
5. **Testability** — Each phase should be independently testable

### Technology Choices

| Component | Choice | Rationale |
|-----------|--------|-----------|
| Language | Rust | Memory safety, performance, excellent tooling |
| Lexer | Hand-written | Full control over error recovery and tokens |
| Parser | Hand-written recursive descent | Better error messages than generated parsers |
| Backend | LLVM via inkwell | Mature, optimizing, cross-platform |
| Alternative Backend | Cranelift | Faster compilation, simpler integration |

---

## High-Level Architecture

```
Source Files
    │
    ▼
Lexer (Tokenizer) ─────────────► Token Stream
    │
    ▼
Parser ────────────────────────► Abstract Syntax Tree (AST)
    │
    ▼
Name Resolution ───────────────► Resolved AST with DefIds
    │
    ▼
Type Checking ─────────────────► Type-annotated AST
    │
    ▼
Borrow Checking ───────────────► Validated Ref usage
    │
    ▼
HIR (High-Level IR) ───────────► Desugared representation
    │
    ▼
MIR (Mid-Level IR) ────────────► Control Flow Graph, SSA
    │
    ▼
Optimization ──────────────────► Optimized MIR
    │
    ▼
Code Generation ───────────────► LLVM IR / Machine Code
    │
    ▼
Executable / Library
```

---

## Project Structure

```
lang/
├── Cargo.toml                    # Workspace configuration
├── crates/
│   ├── lang_cli/                 # Command-line interface
│   ├── lang_lexer/               # Lexical analysis
│   ├── lang_parser/              # Parsing
│   ├── lang_ast/                 # AST definitions
│   ├── lang_hir/                 # High-level IR
│   ├── lang_mir/                 # Mid-level IR
│   ├── lang_resolve/             # Name resolution
│   ├── lang_types/               # Type system
│   ├── lang_borrow/              # Borrow checker
│   ├── lang_codegen/             # Code generation
│   ├── lang_runtime/             # Runtime library
│   ├── lang_errors/              # Error handling
│   └── lang_common/              # Shared utilities
├── stdlib/                       # Standard library
├── tests/                        # Test suites
└── examples/                     # Example programs
```

### Cargo Workspace Configuration

```toml
[workspace]
members = [
    "crates/lang_cli",
    "crates/lang_lexer",
    "crates/lang_parser",
    "crates/lang_ast",
    "crates/lang_hir",
    "crates/lang_mir",
    "crates/lang_resolve",
    "crates/lang_types",
    "crates/lang_borrow",
    "crates/lang_codegen",
    "crates/lang_runtime",
    "crates/lang_errors",
    "crates/lang_common",
]

[workspace.dependencies]
thiserror = "1.0"
ariadne = "0.3"
inkwell = "0.2"
cranelift = "0.100"
rustc-hash = "1.1"
indexmap = "2.0"
string-interner = "0.14"
typed-arena = "2.0"
tokio = { version = "1", features = ["full"] }
clap = { version = "4", features = ["derive"] }
```

---

## Compilation Phases

### Phase 1: Lexical Analysis

The lexer transforms source code into a stream of tokens.

**Key Files:**
- `lang_lexer/src/token.rs` — Token definitions
- `lang_lexer/src/lexer.rs` — Lexer implementation

**Token Types:**
```rust
pub enum TokenKind {
    // Literals
    IntLiteral, FloatLiteral, StringLiteral, CharLiteral,
    
    // Keywords
    Fn, Struct, Enum, Interface, Type, Alias, Const, Pub,
    Module, Using, Import, If, Else, Match, Loop, Break,
    Continue, Return, Each, Via, To, And, Or, Not, Is, As,
    Bind, Impl, Extends, Async, Await, Spawn, Block,
    True, False, Null,
    
    // Punctuation & Operators
    LParen, RParen, LBrace, RBrace, LBracket, RBracket,
    Comma, Colon, Dot, DotDot, DotDotEq, Arrow, Bang,
    Question, QuestionQuestion, QuestionDot,
    Plus, Minus, Star, Slash, Percent,
    Eq, EqEq, BangEq, Lt, Gt, LtEq, GtEq,
    Ampersand, Pipe, Caret, Tilde, LtLt, GtGt,
    
    Identifier, Eof, Error,
}
```

### Phase 2: Parsing

The parser transforms tokens into an Abstract Syntax Tree using recursive descent with precedence climbing for expressions.

**Key Components:**
- Expression parser with operator precedence
- Statement parser
- Type parser
- Pattern parser for match expressions

**Operator Precedence (lowest to highest):**
1. Assignment (`=`)
2. Null coalesce (`??`)
3. Logical or (`or`)
4. Logical and (`and`)
5. Equality (`==`, `!=`)
6. Comparison (`<`, `>`, `<=`, `>=`)
7. Bitwise or (`|`)
8. Bitwise xor (`^`)
9. Bitwise and (`&`)
10. Shift (`<<`, `>>`)
11. Term (`+`, `-`)
12. Factor (`*`, `/`, `%`)
13. Unary (`-`, `not`, `~`)
14. Postfix (`!`, `?`, `.`, `?.`, `[]`)
15. Call (`()`)

### Phase 3: Name Resolution

Resolves all identifiers to their definitions.

**Responsibilities:**
- Build scope hierarchy
- Define all top-level items
- Resolve imports
- Link identifiers to DefIds
- Detect undefined and duplicate definitions

**Key Data Structures:**
```rust
pub struct DefId(pub u32);

pub enum Definition {
    Function(DefId),
    Struct(DefId),
    Enum(DefId),
    EnumVariant(DefId, usize),
    Interface(DefId),
    TypeAlias(DefId),
    TypeDef(DefId),
    Const(DefId),
    Local(DefId),
    Parameter(DefId),
    Generic(DefId),
}
```

### Phase 4: Type Checking

Validates types and performs type inference where needed.

**Key Features:**
- Union type checking
- Generic instantiation
- Type narrowing with `to`
- Propagation operator (`!`) validation
- Interface conformance checking

**Type Representation:**
```rust
pub enum Ty {
    // Primitives
    Int(IntSize, Signedness),
    Float(FloatSize),
    Bool, Char, String,
    
    // Special
    Void, Never, Null,
    
    // Compound
    Tuple(Vec<Ty>),
    List(Arc<Ty>),
    Map(Arc<Ty>, Arc<Ty>),
    Set(Arc<Ty>),
    
    // User-defined
    Struct(DefId),
    Enum(DefId),
    
    // Type constructors
    Union(Vec<Ty>),
    Optional(Arc<Ty>),
    Mutable(Arc<Ty>),
    Ref(Arc<Ty>, Lifetime, Mutability),
    
    // Functions
    Function(FunctionTy),
    
    // Generics
    Generic(DefId),
    Applied(DefId, Vec<Ty>),
    
    // Inference
    Infer(InferVar),
    Error,
}
```

### Phase 5: Borrow Checking

Validates that `Ref` types are used correctly (only for opt-in borrow checking).

**Rules Enforced:**
- Multiple immutable borrows allowed
- Only one mutable borrow at a time
- No mutable + immutable borrows simultaneously
- Borrows must not outlive their source

### Phase 6: HIR Lowering

Desugars AST into High-Level IR.

**Desugaring Transforms:**
- `x!` → match with early return
- `x ?? y` → match on optional
- `x?.y` → optional chaining match
- `each via` → iterator transformation
- Partial application → closure creation

### Phase 7: MIR Construction

Transforms HIR into Mid-Level IR with control flow graph.

**MIR Characteristics:**
- Basic blocks with terminators
- SSA form
- Explicit control flow
- Simplified operations

### Phase 8: Code Generation

Generates LLVM IR or Cranelift IR from MIR.

**Key Mappings:**
- Types → LLVM types
- Functions → LLVM functions
- Basic blocks → LLVM basic blocks
- Operations → LLVM instructions

---

## Runtime System

### Reference Counting

Default memory management uses atomic reference counting.

```rust
#[repr(C)]
pub struct RcHeader {
    strong_count: AtomicUsize,
    weak_count: AtomicUsize,
    drop_fn: unsafe fn(*mut u8),
    size: usize,
}

// Runtime intrinsics
extern "C" fn __lang_rc_retain(ptr: *mut RcHeader);
extern "C" fn __lang_rc_release(ptr: *mut RcHeader);
```

### Async Runtime

For async/await support:
- Task scheduler
- Future polling
- Channel implementation
- Spawn/join primitives

### Standard Library Runtime

- String implementation (UTF-8)
- Collection implementations
- I/O primitives
- Math functions

---

## Error Handling and Diagnostics

### Diagnostic Structure

```rust
pub struct Diagnostic {
    pub level: Level,           // Error, Warning, Note, Help
    pub message: String,
    pub primary_span: Option<SpanLabel>,
    pub secondary_spans: Vec<SpanLabel>,
    pub notes: Vec<String>,
    pub suggestions: Vec<Suggestion>,
}
```

### Error Reporting

Use `ariadne` crate for beautiful error output:
- Source code snippets
- Colored labels
- Multi-span support
- Suggestions with replacements

---

## Testing Strategy

### Unit Tests
- Each crate has its own tests
- Test individual components in isolation

### Integration Tests
- End-to-end compilation tests
- Expected output verification

### Snapshot Tests
- AST snapshots
- Error message snapshots
- Code generation snapshots

### Fuzzing
- Input fuzzing for lexer/parser
- Type system edge cases

---

## CLI Interface

```rust
#[derive(Parser)]
enum Commands {
    /// Compile a source file
    Build {
        input: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[arg(short = 'O', long, default_value = "0")]
        opt_level: u8,
        #[arg(long)]
        emit_llvm: bool,
    },
    
    /// Check without compiling
    Check { input: PathBuf },
    
    /// Run a source file
    Run {
        input: PathBuf,
        args: Vec<String>,
    },
    
    /// Format source files
    Fmt {
        files: Vec<PathBuf>,
        #[arg(long)]
        check: bool,
    },
    
    /// Start REPL
    Repl,
}
```

---

## Development Roadmap

### Phase 1: Foundation (Weeks 1-4)
- [ ] Project setup and workspace
- [ ] Lexer implementation
- [ ] Basic parser (expressions, statements)
- [ ] AST data structures
- [ ] Simple error reporting

### Phase 2: Core Language (Weeks 5-8)
- [ ] Complete parser (functions, structs, enums)
- [ ] Name resolution
- [ ] Basic type checking
- [ ] HIR lowering

### Phase 3: Type System (Weeks 9-12)
- [ ] Union types
- [ ] Generics
- [ ] Type narrowing with `to`
- [ ] Interface checking

### Phase 4: Code Generation (Weeks 13-16)
- [ ] MIR construction
- [ ] Basic LLVM codegen
- [ ] Runtime library
- [ ] Simple programs running

### Phase 5: Advanced Features (Weeks 17-20)
- [ ] Borrow checker for `Ref` types
- [ ] Closures
- [ ] Pattern matching
- [ ] Standard library basics

### Phase 6: Concurrency (Weeks 21-24)
- [ ] Async/await
- [ ] Spawn and channels
- [ ] Mutex implementation

### Phase 7: Polish (Weeks 25-28)
- [ ] Comprehensive error messages
- [ ] Formatter
- [ ] LSP server basics
- [ ] Documentation generator

### Phase 8: Optimization (Weeks 29-32)
- [ ] LLVM optimization passes
- [ ] Inlining
- [ ] Dead code elimination
- [ ] Benchmark suite

---

## Key Dependencies

```toml
# Error reporting
ariadne = "0.3"

# Code generation
inkwell = "0.2"          # LLVM bindings
cranelift = "0.100"      # Alternative backend

# Utilities
rustc-hash = "1.1"       # Fast hashing
indexmap = "2.0"         # Ordered maps
string-interner = "0.14" # String interning
typed-arena = "2.0"      # Arena allocation

# Async runtime
tokio = "1"

# CLI
clap = "4"

# Testing
insta = "1.29"           # Snapshot testing
```

---

## Resources

### Compiler Design
- *Crafting Interpreters* by Robert Nystrom
- *Engineering a Compiler* by Cooper and Torczon
- rustc-dev-guide: https://rustc-dev-guide.rust-lang.org/

### LLVM
- LLVM Tutorial: https://llvm.org/docs/tutorial/
- inkwell examples: https://github.com/TheDan64/inkwell

### Type Systems
- *Types and Programming Languages* by Benjamin Pierce
