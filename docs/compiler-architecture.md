# Compiler Architecture Document

## Table of Contents

1. [Overview](#overview)
2. [High-Level Architecture](#high-level-architecture)
3. [Project Structure](#project-structure)
4. [Compilation Phases](#compilation-phases)
5. [Phase Input/Output Schemas](#phase-inputoutput-schemas)
6. [Runtime System](#runtime-system)
7. [Error Handling and Diagnostics](#error-handling-and-diagnostics)
8. [Testing Strategy](#testing-strategy)
9. [CLI Interface](#cli-interface)
10. [Development Roadmap](#development-roadmap)

---

## Overview

This document outlines the architecture for building the Spark language compiler in Rust. The compiler follows a traditional multi-phase pipeline architecture with clearly defined boundaries between components.

### Design Goals

1. **Correctness** — The compiler must correctly implement the language specification
2. **Clear error messages** — Errors should be helpful and point to exact locations
3. **Incremental compilation** — Support for fast recompilation of changed files
4. **Extensibility** — Easy to add new features and optimizations
5. **Testability** — Each phase should be independently testable
6. **Minimal coupling** — Phases communicate through well-defined interfaces
7. **High cohesion** — Each crate has a single, focused responsibility

### Technology Choices

| Component           | Choice                         | Rationale                                     |
| ------------------- | ------------------------------ | --------------------------------------------- |
| Language            | Rust                           | Memory safety, performance, excellent tooling |
| Lexer               | Hand-written                   | Full control over error recovery and tokens   |
| Parser              | Hand-written recursive descent | Better error messages than generated parsers  |
| Backend             | LLVM via inkwell               | Mature, optimizing, cross-platform            |
| Alternative Backend | Cranelift                      | Faster compilation, simpler integration       |

---

## High-Level Architecture

```
Source Files (.spark)
    │
    ▼
┌─────────────────────────────────────────────────────────────────┐
│ PHASE 1: Lexical Analysis (spark_lexer)                         │
│   Input:  Source text (&str)                                    │
│   Output: Vec<Token>                                            │
└─────────────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────────────┐
│ PHASE 2: Parsing (spark_parser)                                 │
│   Input:  Vec<Token>                                            │
│   Output: Ast (unresolved)                                      │
└─────────────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────────────┐
│ PHASE 3: Module Resolution (spark_modules)                      │
│   Input:  Map<ModulePath, Ast>                                  │
│   Output: ModuleGraph                                           │
└─────────────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────────────┐
│ PHASE 4: Name Resolution (spark_resolve)                        │
│   Input:  ModuleGraph + Ast                                     │
│   Output: ResolvedAst (identifiers → DefIds)                    │
└─────────────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────────────┐
│ PHASE 5: Type Checking (spark_types)                            │
│   Input:  ResolvedAst                                           │
│   Output: TypedAst + TypeTable                                  │
└─────────────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────────────┐
│ PHASE 6: HIR Lowering (spark_hir)                               │
│   Input:  TypedAst                                              │
│   Output: Hir (desugared, still tree-structured)                │
└─────────────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────────────┐
│ PHASE 7: MIR Construction (spark_mir)                           │
│   Input:  Hir                                                   │
│   Output: Mir (CFG, SSA form)                                   │
└─────────────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────────────┐
│ PHASE 8: Borrow Checking (spark_borrow)                         │
│   Input:  Mir + TypeTable                                       │
│   Output: Mir (validated, unchanged)                            │
│   Note:   Only activated for functions using Ref types          │
└─────────────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────────────┐
│ PHASE 9: Optimization (spark_mir)                               │
│   Input:  Mir                                                   │
│   Output: OptimizedMir                                          │
└─────────────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────────────┐
│ PHASE 10: Code Generation (spark_codegen)                       │
│   Input:  OptimizedMir + TypeTable                              │
│   Output: LLVM IR → Object files → Executable                   │
└─────────────────────────────────────────────────────────────────┘
```

---

## Project Structure

```
spark/
├── Cargo.toml                    # Workspace configuration
├── crates/
│   ├── spark_cli/                # Command-line interface
│   ├── spark_driver/             # Compilation orchestration
│   ├── spark_lexer/              # Lexical analysis
│   ├── spark_parser/             # Parsing
│   ├── spark_ast/                # AST definitions
│   ├── spark_modules/            # Module resolution
│   ├── spark_resolve/            # Name resolution
│   ├── spark_types/              # Type system & checking
│   ├── spark_hir/                # High-level IR
│   ├── spark_mir/                # Mid-level IR & optimization
│   ├── spark_borrow/             # Borrow checker
│   ├── spark_codegen/            # Code generation
│   ├── spark_runtime/            # Runtime library (compiled to .a)
│   ├── spark_diagnostics/        # Error handling & reporting
│   ├── spark_span/               # Source locations & spans
│   └── spark_intern/             # String interning & symbols
├── stdlib/                       # Standard library (Spark source)
├── tests/                        # Integration tests
│   ├── ui/                       # UI tests (error message snapshots)
│   ├── run-pass/                 # Tests expected to compile & run
│   └── compile-fail/             # Tests expected to fail compilation
└── examples/                     # Example programs
```

### Crate Responsibilities (Single Responsibility)

| Crate               | Responsibility                                 |
| ------------------- | ---------------------------------------------- |
| `spark_cli`         | Parse CLI args, invoke driver, display results |
| `spark_driver`      | Orchestrate compilation phases, manage caching |
| `spark_lexer`       | Convert source text to tokens                  |
| `spark_parser`      | Convert tokens to AST                          |
| `spark_ast`         | Define AST node types (data only, no logic)    |
| `spark_modules`     | Resolve module paths, build dependency graph   |
| `spark_resolve`     | Resolve identifiers to definitions             |
| `spark_types`       | Type checking, inference, interface validation |
| `spark_hir`         | Desugar AST to HIR                             |
| `spark_mir`         | Build CFG, optimize                            |
| `spark_borrow`      | Validate borrow rules for Ref types            |
| `spark_codegen`     | Generate LLVM/Cranelift IR                     |
| `spark_runtime`     | Runtime support (RC, async, collections)       |
| `spark_diagnostics` | Diagnostic creation and rendering              |
| `spark_span`        | Source spans, file tracking                    |
| `spark_intern`      | Symbol interning, string deduplication         |

### Crate Dependencies (Dependency Graph)

```
                    spark_cli
                        │
                        ▼
                   spark_driver
                        │
        ┌───────────────┼───────────────┐
        ▼               ▼               ▼
   spark_lexer    spark_parser    spark_codegen
        │               │               │
        └───────┬───────┘               │
                ▼                       │
           spark_ast ◄──────────────────┤
                │                       │
                ▼                       │
         spark_modules                  │
                │                       │
                ▼                       │
         spark_resolve                  │
                │                       │
                ▼                       │
          spark_types ◄─────────────────┤
                │                       │
                ▼                       │
           spark_hir                    │
                │                       │
                ▼                       │
           spark_mir ◄──────────────────┘
                │
                ▼
          spark_borrow

Common dependencies (used by multiple crates):
├── spark_span        (source locations)
├── spark_intern      (symbol interning)
└── spark_diagnostics (error reporting)
```

### Cargo Workspace Configuration

```toml
[workspace]
members = [
    "crates/spark_cli",
    "crates/spark_driver",
    "crates/spark_lexer",
    "crates/spark_parser",
    "crates/spark_ast",
    "crates/spark_modules",
    "crates/spark_resolve",
    "crates/spark_types",
    "crates/spark_hir",
    "crates/spark_mir",
    "crates/spark_borrow",
    "crates/spark_codegen",
    "crates/spark_runtime",
    "crates/spark_diagnostics",
    "crates/spark_span",
    "crates/spark_intern",
]

[workspace.dependencies]
# Error reporting
thiserror = "1.0"
ariadne = "0.4"

# Code generation
inkwell = "0.4"
cranelift = "0.105"
cranelift-module = "0.105"

# Data structures
rustc-hash = "1.1"
indexmap = "2.0"
la-arena = "0.3"           # Typed arenas (from rust-analyzer)

# String handling
string-interner = "0.17"
smol_str = "0.2"           # Small string optimization

# Async runtime (for compiler parallelism)
rayon = "1.8"

# CLI
clap = { version = "4", features = ["derive"] }

# Testing
insta = "1.34"
```

---

## Compilation Phases

### Phase 1: Lexical Analysis ✅

**Status:** Complete

The lexer transforms source code into a stream of tokens.

**Crate:** `spark_lexer`

**Key Files:**

- `src/token.rs` — Token definitions
- `src/lexer.rs` — Lexer implementation
- `src/cursor.rs` — Character iteration with lookahead

**Implementation:**

- Hand-written lexer with full Unicode support
- 65 token kinds (4 literals, 31 keywords, 25 operators, 3 special)
- Error recovery for invalid characters
- 33 tests covering all token types

**Token Definition:**

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    pub value: Option<TokenValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenValue {
    Int(i64),
    Float(f64),
    String(Symbol),      // Interned string
    Char(char),
    Ident(Symbol),       // Interned identifier
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    // Literals
    IntLiteral,
    FloatLiteral,
    StringLiteral,
    CharLiteral,

    // Keywords
    Fn, Struct, Enum, Interface, Type, Alias, Const, Pub,
    Module, Using, Import, If, Else, Match, Loop, Break,
    Continue, Return, Each, Via, To, And, Or, Not, Is, As,
    Binds, Extends, Async, Await, Spawn, Block,
    True, False, Null,

    // Punctuation & Operators
    LParen, RParen, LBrace, RBrace, LBracket, RBracket,
    Comma, Colon, ColonColon, Semicolon,
    Dot, DotDot, DotDotEq, Arrow, FatArrow,
    Bang, Question, QuestionQuestion, QuestionDot,
    Plus, Minus, Star, Slash, Percent,
    PlusEq, MinusEq, StarEq, SlashEq,
    Eq, EqEq, BangEq, Lt, Gt, LtEq, GtEq,
    Ampersand, Pipe, Caret, Tilde, LtLt, GtGt,
    At,                  // For decorators
    Ellipsis,            // For spread: ...

    // Special
    Identifier,
    DocComment,
    Eof,
    Error,
}
```

---

### Phase 2: Parsing ✅

**Status:** Complete

The parser transforms tokens into an Abstract Syntax Tree.

**Crate:** `spark_parser`

**Key Files:**

- `src/lib.rs` — Public API and Parser struct
- `src/expr.rs` — Expression parsing (Pratt algorithm)
- `src/stmt.rs` — Statement parsing
- `src/types.rs` — Type expression parsing
- `src/decl.rs` — Declaration parsing
- `src/pattern.rs` — Pattern parsing for match

**Implementation:**

- Recursive descent parser with Pratt expression parsing
- Error recovery to report multiple syntax errors per file
- Span tracking for all AST nodes
- 102 tests covering all language constructs

**Operator Precedence (lowest to highest):**

| Level | Operators                        | Associativity  |
| ----- | -------------------------------- | -------------- |
| 1     | `=`, `+=`, `-=`, `*=`, `/=`      | Right          |
| 2     | `??`                             | Left           |
| 3     | `or`                             | Left           |
| 4     | `and`                            | Left           |
| 5     | `==`, `!=`                       | Left           |
| 6     | `<`, `>`, `<=`, `>=`, `is`, `as` | Left           |
| 7     | `\|`                             | Left           |
| 8     | `^`                              | Left           |
| 9     | `&`                              | Left           |
| 10    | `<<`, `>>`                       | Left           |
| 11    | `+`, `-`                         | Left           |
| 12    | `*`, `/`, `%`                    | Left           |
| 13    | `-` (unary), `not`, `~`          | Right (prefix) |
| 14    | `!`, `?`, `.`, `?.`, `[]`, `()`  | Left (postfix) |

---

### Phase 3: Module Resolution

Resolves module paths and builds the module dependency graph.

**Crate:** `spark_modules`

**Key Files:**

- `src/resolver.rs` — Module path resolution
- `src/graph.rs` — Dependency graph construction
- `src/visibility.rs` — Visibility checking

**Responsibilities:**

- Map `module` declarations to file paths
- Resolve `import` statements to module paths
- Handle `using` (joining existing module)
- Detect circular module dependencies
- Enforce visibility rules (`pub`)
- Handle re-exports (`pub import`)

---

### Phase 4: Name Resolution

Resolves all identifiers to their definitions.

**Crate:** `spark_resolve`

**Key Files:**

- `src/resolver.rs` — Main resolution pass
- `src/scope.rs` — Scope management
- `src/ribs.rs` — Lexical scoping (rib stack)

**Responsibilities:**

- Build scope hierarchy
- Define all top-level items (functions, structs, enums, etc.)
- Resolve identifier references to DefIds
- Resolve type references
- Detect undefined and duplicate definitions
- Handle shadowing rules

---

### Phase 5: Type Checking

Validates types and checks program correctness.

**Crate:** `spark_types`

**Key Files:**

- `src/check.rs` — Main type checking
- `src/infer.rs` — Type unification (internal only)
- `src/union.rs` — Union type handling
- `src/narrow.rs` — Type narrowing with `to`
- `src/interface.rs` — Interface conformance
- `src/exhaustive.rs` — Match exhaustiveness checking

**Responsibilities:**

- Validate all type annotations
- Check function call argument types
- Union type normalization and subtyping
- Generic instantiation and constraint checking
- Type narrowing through `to` blocks
- Propagation operator (`!`) validation
- Interface binding validation
- Match exhaustiveness checking
- Mutability checking (`Type!`)

---

### Phase 6: HIR Lowering

Desugars typed AST into High-Level IR.

**Crate:** `spark_hir`

**Key Files:**

- `src/lower.rs` — AST to HIR lowering
- `src/desugar.rs` — Syntactic sugar removal

**Desugaring Transforms:**

| Source                         | Target                                           |
| ------------------------------ | ------------------------------------------------ |
| `x!`                           | `match x { Ok(v) -> v, err -> return err }`      |
| `x ?? y`                       | `match x { Some(v) -> v, Null -> y }`            |
| `x?.y`                         | `match x { Some(v) -> Some(v.y), Null -> Null }` |
| `x?.y?.z`                      | Nested optional chain                            |
| `list each via (x): T { ... }` | Iterator map                                     |
| `fn f = g{a: 1}`               | Closure capturing bound args                     |
| `@decorator`                   | Function wrapping                                |
| `[0..10]`                      | Range to list conversion                         |
| `"Hello, {name}"`              | String concatenation                             |

---

### Phase 7: MIR Construction

Transforms HIR into control flow graph with SSA form.

**Crate:** `spark_mir`

**Key Files:**

- `src/build.rs` — HIR to MIR construction
- `src/ssa.rs` — SSA construction
- `src/optimize.rs` — Optimization passes
- `src/cfg.rs` — Control flow graph utilities

**MIR Characteristics:**

- Basic blocks with single entry, single exit
- Explicit control flow (no implicit fallthrough)
- SSA form (each variable assigned exactly once)
- Explicit temporaries for all intermediate values
- Drop/cleanup operations explicit

---

### Phase 8: Borrow Checking

Validates lifetime and borrowing rules for `Ref` types.

**Crate:** `spark_borrow`

**Key Files:**

- `src/check.rs` — Main borrow checker
- `src/lifetime.rs` — Lifetime inference
- `src/facts.rs` — Polonius-style fact generation

**Rules Enforced:**

- Multiple immutable borrows (`Ref[T, A]`) allowed simultaneously
- Only one mutable borrow (`Ref[T, A]!`) at a time
- No mutable + immutable borrows simultaneously
- Borrows must not outlive their source
- Lifetime parameters must be satisfied

**Note:** Only runs on functions that use `Ref` types. Functions using default copy/RC semantics skip this phase entirely.

---

### Phase 9: Optimization

Optimizes MIR before code generation.

**Crate:** `spark_mir` (same crate as construction)

**Optimization Passes:**

1. Dead code elimination
2. Constant folding
3. Constant propagation
4. Copy propagation
5. Common subexpression elimination
6. Inlining (small functions)
7. Tail call optimization

---

### Phase 10: Code Generation

Generates LLVM IR from optimized MIR.

**Crate:** `spark_codegen`

**Key Files:**

- `src/llvm.rs` — LLVM backend
- `src/cranelift.rs` — Cranelift backend (alternative)
- `src/layout.rs` — Type layout computation
- `src/abi.rs` — Calling convention handling
- `src/intrinsics.rs` — Built-in function implementations

**Key Mappings:**

- Spark types → LLVM types
- MIR functions → LLVM functions
- Basic blocks → LLVM basic blocks
- MIR operations → LLVM instructions
- RC operations → Runtime calls

---

## Phase Input/Output Schemas

### Schema Definitions

```rust
//=============================================================================
// spark_span - Source Locations
//=============================================================================

/// A source file identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId(pub u32);

/// A range in source code
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub file: FileId,
    pub start: u32,    // Byte offset
    pub end: u32,      // Byte offset
}

//=============================================================================
// spark_intern - Symbol Interning
//=============================================================================

/// An interned string identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Symbol(pub u32);

/// Global symbol interner (thread-local or synchronized)
pub struct Interner { /* ... */ }

impl Interner {
    pub fn intern(&mut self, s: &str) -> Symbol;
    pub fn get(&self, sym: Symbol) -> &str;
}

//=============================================================================
// spark_lexer - Lexical Analysis
//=============================================================================

/// Input: Source text
pub struct LexerInput<'a> {
    pub source: &'a str,
    pub file_id: FileId,
}

/// Output: Token stream
pub struct LexerOutput {
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<Diagnostic>,
}

pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    pub value: Option<TokenValue>,
}

//=============================================================================
// spark_parser - Parsing
//=============================================================================

/// Input: Token stream
pub struct ParserInput {
    pub tokens: Vec<Token>,
    pub file_id: FileId,
}

/// Output: Abstract Syntax Tree (unresolved)
pub struct ParserOutput {
    pub ast: Ast,
    pub diagnostics: Vec<Diagnostic>,
}

/// The AST for a single file
pub struct Ast {
    pub file_id: FileId,
    pub module_decl: Option<ModuleDecl>,
    pub using_decl: Option<UsingDecl>,
    pub imports: Vec<ImportDecl>,
    pub items: Vec<Item>,
}

pub enum Item {
    Function(FunctionDef),
    Struct(StructDef),
    Enum(EnumDef),
    Interface(InterfaceDef),
    TypeAlias(TypeAliasDef),
    TypeDef(TypeDefDef),
    Const(ConstDef),
    Binding(BindingDef),
}

//=============================================================================
// spark_modules - Module Resolution
//=============================================================================

/// Input: All parsed ASTs
pub struct ModuleResolverInput {
    pub asts: HashMap<FileId, Ast>,
    pub root_path: PathBuf,
}

/// Output: Module graph with resolved paths
pub struct ModuleResolverOutput {
    pub graph: ModuleGraph,
    pub diagnostics: Vec<Diagnostic>,
}

pub struct ModuleGraph {
    pub modules: HashMap<ModulePath, ModuleInfo>,
    pub dependencies: HashMap<ModulePath, Vec<ModulePath>>,
}

pub struct ModuleInfo {
    pub path: ModulePath,
    pub files: Vec<FileId>,              // Files in this module
    pub exports: HashSet<Symbol>,         // Public items
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModulePath(pub Vec<Symbol>);   // e.g., ["user", "auth"]

//=============================================================================
// spark_resolve - Name Resolution
//=============================================================================

/// Input: Module graph + ASTs
pub struct ResolverInput {
    pub graph: ModuleGraph,
    pub asts: HashMap<FileId, Ast>,
}

/// Output: Resolved AST with DefIds
pub struct ResolverOutput {
    pub resolved: ResolvedAst,
    pub definitions: DefinitionTable,
    pub diagnostics: Vec<Diagnostic>,
}

/// Unique identifier for any definition
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DefId(pub u32);

pub struct DefinitionTable {
    pub defs: HashMap<DefId, DefinitionInfo>,
}

pub struct DefinitionInfo {
    pub kind: DefinitionKind,
    pub name: Symbol,
    pub span: Span,
    pub visibility: Visibility,
    pub module: ModulePath,
}

pub enum DefinitionKind {
    Function,
    Struct,
    StructField(DefId),      // Parent struct
    Enum,
    EnumVariant(DefId),      // Parent enum
    Interface,
    TypeAlias,
    TypeDef,
    Const,
    Local,
    Parameter,
    GenericParam,
    LifetimeParam,
}

/// AST with all identifiers resolved to DefIds
pub struct ResolvedAst {
    pub items: Vec<ResolvedItem>,
    // All Ident nodes now have an associated DefId
}

//=============================================================================
// spark_types - Type Checking
//=============================================================================

/// Input: Resolved AST
pub struct TypeCheckerInput {
    pub resolved: ResolvedAst,
    pub definitions: DefinitionTable,
}

/// Output: Typed AST + Type information
pub struct TypeCheckerOutput {
    pub typed_ast: TypedAst,
    pub type_table: TypeTable,
    pub diagnostics: Vec<Diagnostic>,
}

/// Type identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeId(pub u32);

pub struct TypeTable {
    pub types: HashMap<TypeId, Ty>,
    pub expr_types: HashMap<ExprId, TypeId>,   // Type of each expression
    pub def_types: HashMap<DefId, TypeId>,     // Type of each definition
    pub interface_impls: HashMap<DefId, Vec<DefId>>,  // Type → interfaces it binds
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    // Primitives
    Int { size: IntSize, signed: bool },
    Float { size: FloatSize },
    Bool,
    Char,
    String,

    // Special
    Void,
    Never,
    Null,

    // Compound
    Tuple(Vec<TypeId>),
    List(TypeId),
    Map(TypeId, TypeId),
    Set(TypeId),

    // User-defined (resolved to definition)
    Struct(DefId),
    Enum(DefId),
    EnumVariant(DefId, usize),    // Specific variant
    Interface(DefId),

    // Type constructors
    Union(Vec<TypeId>),           // Normalized, sorted
    Optional(TypeId),             // Sugar for Union([T, Null])
    Mutable(TypeId),              // T!
    Ref {
        inner: TypeId,
        lifetime: LifetimeId,
        mutable: bool,
    },

    // Functions
    Function {
        params: Vec<TypeId>,
        ret: TypeId,
        is_async: bool,
    },

    // Generics
    GenericParam(DefId),          // Reference to generic parameter
    Applied {                     // Generic type with arguments
        base: DefId,
        args: Vec<TypeId>,
    },

    // Error recovery
    Error,
}

pub struct TypedAst {
    pub items: Vec<TypedItem>,
    // Every expression node has an associated TypeId
}

//=============================================================================
// spark_hir - High-Level IR
//=============================================================================

/// Input: Typed AST
pub struct HirLoweringInput {
    pub typed_ast: TypedAst,
    pub type_table: TypeTable,
}

/// Output: Desugared HIR
pub struct HirLoweringOutput {
    pub hir: Hir,
    pub type_table: TypeTable,   // Unchanged, passed through
}

pub struct Hir {
    pub functions: HashMap<DefId, HirFunction>,
    pub statics: HashMap<DefId, HirStatic>,
}

pub struct HirFunction {
    pub def_id: DefId,
    pub params: Vec<HirParam>,
    pub body: HirExpr,
    pub ret_type: TypeId,
}

/// HIR expressions - fully desugared but still tree-structured
pub enum HirExpr {
    Literal(HirLiteral),
    Local(DefId),

    // Binary/unary (only primitive operations)
    BinaryOp { op: BinaryOp, lhs: Box<HirExpr>, rhs: Box<HirExpr> },
    UnaryOp { op: UnaryOp, operand: Box<HirExpr> },

    // Control flow
    Block(Vec<HirStmt>, Option<Box<HirExpr>>),
    If { cond: Box<HirExpr>, then: Box<HirExpr>, else_: Option<Box<HirExpr>> },
    Match { scrutinee: Box<HirExpr>, arms: Vec<HirMatchArm> },
    Loop { body: Box<HirExpr> },
    Break(Option<Box<HirExpr>>),
    Continue,
    Return(Option<Box<HirExpr>>),

    // Calls
    Call { func: Box<HirExpr>, args: Vec<HirExpr> },
    MethodCall { receiver: Box<HirExpr>, method: Symbol, args: Vec<HirExpr> },

    // Struct/enum
    StructInit { def_id: DefId, fields: Vec<(Symbol, HirExpr)> },
    EnumVariant { def_id: DefId, variant: usize, fields: Vec<HirExpr> },
    FieldAccess { base: Box<HirExpr>, field: Symbol },

    // References
    Borrow { expr: Box<HirExpr>, mutable: bool },
    Deref(Box<HirExpr>),

    // Closures (desugared from partials and lambdas)
    Closure { params: Vec<HirParam>, body: Box<HirExpr>, captures: Vec<DefId> },

    // Async
    Await(Box<HirExpr>),
    Spawn(Box<HirExpr>),
}

//=============================================================================
// spark_mir - Mid-Level IR
//=============================================================================

/// Input: HIR
pub struct MirBuilderInput {
    pub hir: Hir,
    pub type_table: TypeTable,
}

/// Output: Control flow graph in SSA form
pub struct MirBuilderOutput {
    pub mir: Mir,
    pub type_table: TypeTable,
}

pub struct Mir {
    pub functions: HashMap<DefId, MirFunction>,
}

pub struct MirFunction {
    pub def_id: DefId,
    pub params: Vec<Local>,
    pub return_type: TypeId,
    pub locals: Vec<LocalDecl>,
    pub blocks: Vec<BasicBlock>,
    pub entry_block: BlockId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Local(pub u32);

pub struct LocalDecl {
    pub local: Local,
    pub ty: TypeId,
    pub name: Option<Symbol>,   // For debugging
}

pub struct BasicBlock {
    pub id: BlockId,
    pub statements: Vec<Statement>,
    pub terminator: Terminator,
}

pub enum Statement {
    Assign { place: Place, rvalue: Rvalue },
    Drop(Place),
    Nop,
}

pub struct Place {
    pub local: Local,
    pub projections: Vec<Projection>,
}

pub enum Projection {
    Field(Symbol),
    Index(Local),
    Deref,
}

pub enum Rvalue {
    Use(Operand),
    BinaryOp(BinaryOp, Operand, Operand),
    UnaryOp(UnaryOp, Operand),
    Ref(Place, bool),              // &place, mutable flag
    Aggregate(AggregateKind, Vec<Operand>),
    Discriminant(Place),           // Get enum discriminant
}

pub enum Operand {
    Copy(Place),
    Move(Place),
    Constant(Constant),
}

pub enum Terminator {
    Goto(BlockId),
    Return,
    SwitchInt {
        discriminant: Operand,
        targets: Vec<(i128, BlockId)>,
        otherwise: BlockId,
    },
    Call {
        func: Operand,
        args: Vec<Operand>,
        destination: Place,
        target: BlockId,          // Block to jump to after call
    },
    Unreachable,
}

//=============================================================================
// spark_borrow - Borrow Checking
//=============================================================================

/// Input: MIR (unchanged)
pub struct BorrowCheckInput {
    pub mir: Mir,
    pub type_table: TypeTable,
}

/// Output: Validated MIR (same as input if valid)
pub struct BorrowCheckOutput {
    pub mir: Mir,
    pub diagnostics: Vec<Diagnostic>,
}

// Borrow checker produces facts for analysis:
pub struct BorrowFacts {
    pub loans: Vec<Loan>,
    pub loan_issued_at: Vec<(Loan, Point)>,
    pub loan_invalidated_at: Vec<(Loan, Point)>,
}

//=============================================================================
// spark_codegen - Code Generation
//=============================================================================

/// Input: Optimized MIR
pub struct CodegenInput {
    pub mir: Mir,
    pub type_table: TypeTable,
    pub target: TargetSpec,
    pub opt_level: OptLevel,
}

/// Output: Object files or executable
pub struct CodegenOutput {
    pub artifacts: Vec<Artifact>,
    pub diagnostics: Vec<Diagnostic>,
}

pub enum Artifact {
    ObjectFile(PathBuf),
    LlvmIr(String),
    Assembly(String),
    Executable(PathBuf),
}

//=============================================================================
// spark_diagnostics - Error Handling
//=============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
    Note,
    Help,
}

pub struct Diagnostic {
    pub level: Level,
    pub code: Option<DiagnosticCode>,
    pub message: String,
    pub primary_span: Option<SpanLabel>,
    pub secondary_spans: Vec<SpanLabel>,
    pub notes: Vec<String>,
    pub suggestions: Vec<Suggestion>,
}

pub struct SpanLabel {
    pub span: Span,
    pub label: Option<String>,
}

pub struct Suggestion {
    pub message: String,
    pub span: Span,
    pub replacement: String,
}

/// Diagnostic codes for categorization (e.g., E0001)
#[derive(Debug, Clone, Copy)]
pub struct DiagnosticCode(pub u16);
```

---

## Runtime System

### Reference Counting

Default memory management uses atomic reference counting.

```rust
// spark_runtime/src/rc.rs

#[repr(C)]
pub struct RcBox<T> {
    header: RcHeader,
    value: T,
}

#[repr(C)]
pub struct RcHeader {
    strong: AtomicUsize,
    weak: AtomicUsize,
}

// Generated code calls these intrinsics
#[no_mangle]
pub extern "C" fn __spark_rc_retain(ptr: *mut RcHeader) {
    unsafe { (*ptr).strong.fetch_add(1, Ordering::Relaxed); }
}

#[no_mangle]
pub extern "C" fn __spark_rc_release(ptr: *mut RcHeader, drop_fn: unsafe fn(*mut u8)) {
    unsafe {
        if (*ptr).strong.fetch_sub(1, Ordering::Release) == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            drop_fn(ptr as *mut u8);
        }
    }
}
```

### Async Runtime (Implemented)

The runtime provides Tokio-backed async support:

- `__spark_spawn` — spawns a closure on Tokio's blocking thread pool, returns `SparkFuture*`
- `__spark_await` — joins a future handle, returns the inner value
- `__spark_spawn_coop` — cooperative task spawn via poll function + state pointer
- `__spark_try_poll` — polls a cooperative task, returns Ready/Pending
- `__spark_alloc_state` — allocates async state machine struct
- `__spark_future_rc_dec` — RC decrement for futures
- `SparkTask` — Rust Future adapter with thread-local waker bridge

### Standard Library Runtime

- String implementation (UTF-8, small-string optimization)
- Collection implementations (List, Map, Set with RC)
- I/O primitives
- Math functions

---

## Error Handling and Diagnostics

### Diagnostic Collection

Each phase returns diagnostics alongside its output:

```rust
pub struct PhaseResult<T> {
    pub output: Option<T>,        // None if fatal errors
    pub diagnostics: Vec<Diagnostic>,
}

impl<T> PhaseResult<T> {
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.level == Level::Error)
    }
}
```

### Error Rendering

Use `ariadne` crate for beautiful error output:

```rust
use ariadne::{Report, ReportKind, Source, Label};

fn render_diagnostic(diag: &Diagnostic, sources: &SourceMap) {
    let mut builder = Report::build(
        match diag.level {
            Level::Error => ReportKind::Error,
            Level::Warning => ReportKind::Warning,
            _ => ReportKind::Advice,
        },
        diag.primary_span.as_ref().map(|s| s.span.file),
        diag.primary_span.as_ref().map(|s| s.span.start as usize).unwrap_or(0),
    )
    .with_message(&diag.message);

    // Add labels...
    // Print to stderr
}
```

---

## Testing Strategy

### Unit Tests

Each crate has module-level tests:

```rust
// spark_lexer/src/lexer.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lex_integer_literals() {
        let tokens = lex("42 0xFF 0b1010");
        assert_eq!(tokens[0].kind, TokenKind::IntLiteral);
        // ...
    }
}
```

### Integration Tests

End-to-end compilation tests in `tests/`:

```
tests/
├── ui/                     # Error message snapshots
│   ├── type_errors/
│   └── borrow_errors/
├── run-pass/               # Should compile and run correctly
│   ├── basics/
│   ├── generics/
│   └── async/
└── compile-fail/           # Should fail compilation
    ├── type_errors/
    └── borrow_errors/
```

### Snapshot Tests

Using `insta` for snapshot testing:

```rust
#[test]
fn test_error_message() {
    let output = compile("fn f(): Int { \"not an int\" }");
    insta::assert_snapshot!(output.diagnostics_string());
}
```

---

## CLI Interface

```rust
#[derive(Parser)]
#[command(name = "spark")]
#[command(about = "The Spark programming language compiler")]
enum Commands {
    /// Compile source files
    Build {
        #[arg(required = true)]
        input: PathBuf,

        #[arg(short, long)]
        output: Option<PathBuf>,

        #[arg(short = 'O', long, default_value = "0")]
        opt_level: u8,

        #[arg(long)]
        emit_llvm: bool,

        #[arg(long)]
        emit_mir: bool,
    },

    /// Type-check without compiling
    Check {
        input: PathBuf,
    },

    /// Compile and run
    Run {
        input: PathBuf,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },

    /// Format source files
    Fmt {
        files: Vec<PathBuf>,
        #[arg(long)]
        check: bool,
    },

    /// Start interactive REPL
    Repl,

    /// Show version info
    Version,
}
```

---

## Development Roadmap

### Phase 1: Foundation ✅

**Status:** Complete

- [x] Project setup and workspace structure
- [x] `spark_span` and `spark_intern` crates (13 + 14 tests)
- [x] `spark_diagnostics` foundation (12 tests)
- [x] Lexer implementation (33 tests)
- [x] Complete parser (102 tests)
- [x] AST data structures (53 tests)

**Total:** 227 tests passing

### Phase 2: Core Language (IN PROGRESS)

**Next:** Module resolution

- [x] Complete parser (functions, structs, enums, interfaces, bindings)
- [ ] Module resolution ← **NEXT**
- [ ] Name resolution
- [ ] Basic type checking (no generics)
- [ ] Simple error messages

### Phase 3: Type System

- [ ] Union types and narrowing
- [ ] Generics and constraints
- [ ] Interface checking
- [ ] Match exhaustiveness

### Phase 4: IR and Codegen

- [ ] HIR lowering
- [ ] MIR construction
- [ ] Basic LLVM codegen
- [ ] Runtime library (RC)
- [ ] First programs running

### Phase 5: Advanced Features

- [ ] Borrow checker for `Ref` types
- [ ] Closures and partials
- [ ] Complete pattern matching
- [ ] Standard library basics

### ~~Phase 6: Concurrency~~ — Mostly Complete

- [x] Async/await/spawn runtime (Tokio — Phases 1, 2, 3v1)
- [x] Cooperative async infrastructure (state machine transform)
- [ ] Async Phase 3 v2 (segmented suspension — SPARK-021)
- [ ] `parallel each` (Rayon — SPARK-020)
- [ ] Channels and `select`

### Phase 7: Polish

- [ ] Comprehensive error messages
- [ ] Formatter
- [ ] Basic LSP server
- [ ] Documentation generator

### Phase 8: Optimization

- [ ] MIR optimization passes
- [ ] LLVM optimization integration
- [ ] Benchmark suite

---

## Key Dependencies

```toml
# Error reporting
ariadne = "0.4"

# Code generation
inkwell = "0.4"
cranelift = "0.105"
cranelift-module = "0.105"

# Data structures
rustc-hash = "1.1"       # Fast hashing
indexmap = "2.0"         # Ordered maps
la-arena = "0.3"         # Typed arenas

# String handling
string-interner = "0.17"
smol_str = "0.2"

# Parallelism
rayon = "1.8"

# CLI
clap = "4"

# Testing
insta = "1.34"

# Serialization (for incremental compilation cache)
bincode = "1.3"
serde = { version = "1.0", features = ["derive"] }
```

---

## Resources

### Compiler Design

- _Crafting Interpreters_ by Robert Nystrom
- _Engineering a Compiler_ by Cooper and Torczon
- rustc-dev-guide: https://rustc-dev-guide.rust-lang.org/

### LLVM

- LLVM Tutorial: https://llvm.org/docs/tutorial/
- inkwell examples: https://github.com/TheDan64/inkwell

### Type Systems

- _Types and Programming Languages_ by Benjamin Pierce

### Borrow Checking

- Polonius: https://github.com/rust-lang/polonius
- NLL RFC: https://rust-lang.github.io/rfcs/2094-nll.html
