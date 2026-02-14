# Spark LSP Server — Roadmap

This document tracks the incremental path toward full Language Server Protocol support for the Spark language. Each phase builds on the previous one and maps LSP features to the core compiler crates that power them.

Phases are labeled with their core crate readiness:
- **Core support: Ready** — public APIs exist today, LSP just needs to call them
- **Core support: Minor addition needed** — small API additions required in core crates
- **Core support: Not yet available** — requires building new infrastructure

---

## Phase 1: Diagnostics ✅

**Goal:** Report compiler errors and warnings in real time as the user types.

**LSP methods:**
- `textDocument/didOpen` — run analysis, publish diagnostics
- `textDocument/didChange` — re-run analysis on updated source, publish diagnostics
- `textDocument/didClose` — clear diagnostics

**Pipeline:** lex (`spark_lexer`) → parse (`spark_parser`) → module resolve (`spark_modules`) → name resolve (`spark_resolve`) → type check (`spark_types`)

**Key design decisions:**
- Full document sync (entire source on every change) — simple and correct
- Collect diagnostics from all feasible phases (don't stop at first error phase)
- Parse errors prevent resolve/type-check (AST may be malformed), but we always report what we have
- Convert `spark_diagnostics::Diagnostic` labels/spans to LSP positions using byte offset → line/character conversion

**Status:** Complete

---

## Phase 2: Semantic Tokens ✅

**Goal:** Rich syntax highlighting driven by compiler knowledge (not regex).

**LSP methods:**
- `textDocument/semanticTokens/full`

**Core crate:** `spark_lexer` (`TokenKind` → semantic token type mapping)

**Token type mapping (implemented):**
| `TokenKind` | LSP Semantic Token |
|---|---|
| `Fn`, `If`, `Else`, `Match`, `Return`, `True`, `False`, `Null`, ... | `keyword` |
| `Struct`, `Enum`, `Interface`, ... | `keyword` |
| `IntLiteral`, `FloatLiteral` | `number` |
| `StringLiteral`, `CharLiteral` | `string` |
| `DocComment` | `comment` |
| `+`, `-`, `==`, `->`, `=>`, ... | `operator` |
| `Ident` | `variable` |

**Notes:** Basic token-level coloring uses only the lexer. All identifiers are classified as `variable`. Context-aware classification (distinguishing function/type/parameter names) requires resolver data and is addressed in Phase 8.

**Status:** Complete

---

## Phase 3: Go-to-Definition ✅

**Goal:** Jump from a name usage to its definition.

**LSP methods:**
- `textDocument/definition`

**Core crates:** `spark_resolve` (`ResolutionMap`, `DefinitionTable`)

**How it works:**
1. Convert cursor LSP position to byte offset
2. Search all `ResolutionMap` entries (ident, path, binding) for the narrowest span containing the offset
3. Look up `DefId` in `DefinitionTable` → `DefinitionInfo.span`
4. Convert definition span to LSP `Location`

**Implementation notes:**
- Added `ResolutionMap::find_at_offset()` to search for the most specific span at a byte offset
- Added `Clone` derives to `ResolutionMap` and `ResolvedAst` so resolver output can be cached before type checking consumes it
- `AnalysisResult` cached in `DocumentState` on each `did_change`/`did_open`
- `position_to_offset()` helper added for reverse position conversion
- Currently single-file only; definition jumps within the same document

**Status:** Complete

---

## Phase 4: Hover Information ✅

**Goal:** Show type information and signatures on hover.

**LSP methods:**
- `textDocument/hover`

**Core crates:** `spark_resolve` (`DefinitionTable`), `spark_types` (`TypeTable`)

**Content shown:**
- Variables/bindings: type annotation (e.g., `x: Int`)
- Functions: full signature (e.g., `fn add(a: Int, b: Int): Int`)
- Struct fields: field type
- Types: kind (struct, enum, alias)

**Format:** Markdown with `spark` code blocks.

**Status:** Complete

---

## Phase 5: Find References ✅

**Goal:** Find all usages of a symbol across the file.

**LSP methods:**
- `textDocument/references`

**Core crate:** `spark_resolve` (`ResolutionMap`)

**How it works:**
1. Determine the `DefId` at the cursor (via resolution map or definition table)
2. `ResolutionMap::find_references(DefId)` scans ident and path resolutions for matching spans
3. If `include_declaration` is true, the definition's own span is appended
4. Return as `Vec<Location>`

**Implementation notes:**
- Added `ResolutionMap::find_references()` — reverse lookup from `DefId` to all usage spans
- Pattern bindings are excluded (they are definition sites, not usages)
- Follows the same handler pattern as go-to-definition and hover

**Status:** Complete

---

## Phase 6: Document Symbols ✅

**Goal:** Outline view showing all top-level declarations.

**LSP methods:**
- `textDocument/documentSymbol`

**Core crate:** `spark_ast` (walk `Ast.items`)

**Symbols reported:**
- Functions → `SymbolKind::Function`
- Structs → `SymbolKind::Struct` (with Field children)
- Enums → `SymbolKind::Enum` (with EnumMember children)
- Interfaces → `SymbolKind::Interface` (with Method children)
- Constants → `SymbolKind::Constant`
- Type aliases/defs → `SymbolKind::TypeParameter`

**Implementation notes:**
- Walks `Ast.items` directly — no resolver or type checker data needed
- Hierarchical: struct fields, enum variants, and interface methods are nested as child symbols
- Uses `selection_range` for the name span (supports "Go to Symbol" navigation)
- Bindings are skipped (no user-facing name)

**Status:** Complete

---

## Phase 7: VS Code Extension ✅

**Goal:** First-class VS Code support for Spark development.

**Deliverables:**
- `tools/spark-lsp/editors/vscode/` directory
- `package.json` with:
  - Language configuration (`.spark` file association)
  - Comment tokens (`//`), bracket pairs
  - Semantic token legend matching Phase 2
  - `spark.server.path` setting for custom binary location
- Extension TypeScript code that launches `spark-lsp` binary via stdio
- TextMate grammar for basic syntax highlighting (fallback when LSP is starting)
- Language configuration: brackets, auto-closing pairs, indentation rules
- Publish to VS Code marketplace

**Implementation:**
- Entry point: `src/extension.ts` — uses `vscode-languageclient` to spawn `spark-lsp` binary over stdio
- TextMate grammar covers: keywords (31), constants, strings, characters, numbers, comments, type names, function definitions, operators
- Configurable server path via `spark.server.path` (defaults to `spark-lsp` on PATH)

**Status:** Complete

---

## Phase 8: Context-Aware Semantic Tokens ✅

**Core support: Ready** — `DefinitionKind` (18 variants) + `ResolutionMap` exist today

**Goal:** Upgrade semantic tokens from lexer-only classification to resolver-aware classification, so identifiers are colored by their definition kind instead of all being `variable`.

**LSP methods:**
- `textDocument/semanticTokens/full` (enhanced)

**Core crates:** `spark_resolve` (`DefinitionKind`, `ResolutionMap`, `DefinitionTable`)

**Mapping from `DefinitionKind` to LSP semantic token type:**
| `DefinitionKind` | LSP Semantic Token |
|---|---|
| `Function`, `BuiltinFunction`, `StdlibFunction`, `Lambda` | `function` |
| `Struct` | `struct` |
| `Enum` | `enum` |
| `EnumVariant(_)` | `enumMember` |
| `Interface` | `interface` |
| `TypeAlias`, `TypeDef` | `type` |
| `Parameter` | `parameter` |
| `StructField(_)` | `property` |
| `Const` | `variable` + `readonly` modifier |
| `Local` | `variable` |
| `GenericParam`, `LifetimeParam` | `typeParameter` |
| `BuiltinMethod`, `BindingMethod(_)` | `method` |

**How it works:**
1. Run lexer-based classification as today (keywords, operators, literals unchanged)
2. For `Identifier` tokens, look up the token's span in the `ResolutionMap`
3. If resolved → find `DefId` → look up `DefinitionKind` in `DefinitionTable` → map to specific token type
4. If unresolved (parse errors, incomplete code) → fall back to `variable`

**Implementation notes:**
- Zero core crate changes required — all needed data is already in `AnalysisResult`
- `TOKEN_TYPES` legend expanded to 16 types (added `function`, `struct`, `enum`, `enumMember`, `interface`, `type`, `parameter`, `property`, `method`, `typeParameter`)
- Token modifiers: `readonly` (for `Const`), `declaration` (for definition sites via pattern bindings)
- `SemanticContext` struct threads `ResolutionMap` + `DefinitionTable` into `compute_semantic_tokens`
- O(1) hashmap lookups per identifier token (`get_ident` → `get_path` → `get_binding`)
- Unanalyzable files gracefully fall back to lexer-only tokens (`context: None`)

**Status:** Complete

---

## Phase 9: Multi-File Workspace Support

**Core support: Ready** — `spark_modules` + `spark_driver::pipeline` handle multi-file projects today

**Goal:** Analyze the full project instead of single files. This fixes the critical issue where imports appear as errors because the LSP only passes the current file to the module resolver.

**LSP methods:**
- `workspace/didChangeWatchedFiles` — detect new/deleted `.spark` files
- Enhanced `textDocument/didOpen`, `textDocument/didChange` — re-analyze with full project context
- Cross-file `textDocument/definition`, `textDocument/references`

**Core crates:** `spark_modules` (`ModuleDiscovery`, `ModuleGraph`), `spark_driver` (`pipeline`)

**How it works:**
1. On workspace open, discover all `.spark` files (mirrors `spark_driver`'s `CompileInput` pattern which takes `sources: Vec<(PathBuf, String)>`)
2. Lex and parse all files into `HashMap<FileId, Ast>` — same as `pipeline.rs` lines 73–90
3. Pass all ASTs to `spark_modules::resolve()` (takes `ModuleResolverInput { asts, root_path, file_paths }`)
4. Pass module graph + all ASTs to `spark_resolve` and `spark_types`
5. Cache per-file analysis results; on single-file change, re-run full pipeline (incremental later)
6. Go-to-definition and find-references now work across files using `DefinitionInfo.span` (which carries `FileId`)

**Key APIs already available:**
- `ModuleDiscovery::discover()` → discovers all modules, returns file-to-module mapping
- `ModuleGraph::compilation_order()` → topological sort for analysis ordering
- `ModuleGraph::is_exported()` → determines cross-module visibility
- `CompileInput.sources: Vec<(PathBuf, String)>` → the driver already accepts multi-file input

**Implementation notes:**
- `DocumentState` needs to become workspace-aware (store all open + discovered files)
- File watching needed for files not open in the editor
- The `FileId` → URI mapping must be maintained for cross-file location conversion
- On change, re-run full pipeline for correctness; incremental compilation is a future optimization

**Status:** Not started

---

## Phase 10: Rename

**Core support: Ready** — `find_references()` + `DefinitionTable` exist today

**Goal:** Rename a symbol and all its references.

**LSP methods:**
- `textDocument/rename`
- `textDocument/prepareRename`

**Depends on:** Phase 5 (find references), Phase 9 (multi-file for cross-file rename)

**How it works:**
1. `prepareRename`: verify cursor is on a renameable symbol (check `DefinitionKind` — reject built-ins)
2. `rename`: `find_references(DefId)` → collect all spans + definition span → build `WorkspaceEdit` with text edits grouped by document URI
3. With multi-file support (Phase 9), rename works across all files in the project

**Status:** Not started

---

## Phase 11: Code Actions & Quick Fixes

**Core support: Ready** — `spark_diagnostics::Suggestion` maps directly to LSP quick fixes

**Goal:** Suggest automated fixes for common errors.

**LSP methods:**
- `textDocument/codeAction`

**Core crate:** `spark_diagnostics` (`Suggestion`)

**How it works:**
1. Each `Diagnostic` can carry `suggestions: Vec<Suggestion>`
2. `Suggestion` has three fields: `message` (description), `span` (what to replace), `replacement` (new text)
3. Convert each `Suggestion` to a `CodeAction` with a `WorkspaceEdit` containing a single `TextEdit`
4. `Suggestion::insert()` and `Suggestion::delete()` are convenience constructors for common patterns

**Examples:**
- "missing `return`" → insert `return` before trailing expression
- Unresolved name → suggest import (requires Phase 9)
- Type mismatch → suggest `.toInt()` / `.toString()` conversions
- Unused variable → prefix with `_`

**Implementation notes:**
- Suggestions are already emitted by the resolver and type checker — the LSP just needs to surface them
- `CodeActionKind::QUICKFIX` for error-attached suggestions
- Associate actions with their parent diagnostic for "fix this" UI

**Status:** Not started

---

## Phase 12: Inlay Hints

**Core support: Ready** — `TypeTable::get_expr_type()` and `get_def_type()` exist today

**Goal:** Show inferred types inline on bindings and function return positions.

**LSP methods:**
- `textDocument/inlayHint`

**Core crates:** `spark_types` (`TypeTable`), `spark_resolve` (`DefinitionTable`)

**Hint types:**
| Context | Hint | Example |
|---|---|---|
| Local binding without annotation | `: Type` after name | `x` → `x: Int` |
| Function parameter at call site | `name:` before argument | `add(a: 1, b: 2)` |
| Enum variant fields | field name before value | `.Point(x: 1, y: 2)` |

**How it works:**
1. Walk the AST looking for binding sites and call expressions
2. For bindings: `TypeTable::get_def_type(def_id)` → format as `: TypeName`
3. For calls: look up function's `DefId` → get parameter names from AST → show as `param:` labels

**Implementation notes:**
- `TypeTable::get_def_type()` returns `Option<TypeId>` — skip hint if type unknown
- Need type display/formatting (convert `TypeId` → human-readable string)
- Respect user settings for which hint kinds to show

**Status:** Not started

---

## Phase 13: Signature Help

**Core support: Ready** — function signatures available via `DefinitionTable` + AST

**Goal:** Show parameter information while typing function call arguments.

**LSP methods:**
- `textDocument/signatureHelp`

**Core crates:** `spark_resolve` (`DefinitionTable`, `DefinitionKind`), `spark_ast` (function parameter AST nodes)

**How it works:**
1. Detect cursor is inside a call expression's argument list
2. Resolve the callee to a `DefId` → verify `DefinitionKind::Function` (or similar callable kind)
3. Look up the function's AST node to get parameter names and type annotations
4. Determine which parameter the cursor is on (count commas before cursor)
5. Return `SignatureInformation` with `ParameterInformation` for each parameter

**Trigger characters:** `(`, `,`

**Implementation notes:**
- Parameters are `DefinitionKind::Parameter` in the definition table, but their names and types are best read from the function's AST node directly
- Need to handle overloaded/generic functions (show all valid signatures)
- Retrigger on `,` to advance the active parameter index

**Status:** Not started

---

## Phase 14: Completion

**Core support: Minor addition needed** — `ScopeArena` exists but isn't exposed in `ResolverOutput`

**Goal:** Context-aware code completion.

**LSP methods:**
- `textDocument/completion`

**Core crates:** `spark_resolve` (`ScopeArena`, `DefinitionTable`), `spark_types` (`TypeTable`)

**Completion contexts:**
- After `.` → suggest struct fields (look up receiver type in `TypeTable`, list fields)
- General position → suggest in-scope names from resolver
- Type position → suggest known types (built-in + user-defined)
- Include type annotations in completion detail

**Blocked on:** `ScopeArena` is defined and exported as a type (`pub use scope::{ScopeArena, ...}`) but is not included in `ResolverOutput`. The arena holds per-scope `value_bindings` and `type_bindings` (`FxHashMap<Symbol, DefId>`) with parent-scope chaining, which is exactly what completion needs. Two additions required:
1. Add `scope_arena: ScopeArena` to `ResolverOutput`
2. Add a method to enumerate all visible names at a given scope (walk parent chain, collect bindings)

**Status:** Blocked — needs core crate additions

---

## Phase 15: Workspace Symbols

**Core support: Ready** — `DefinitionTable::iter()` + `DefinitionInfo` available today

**Goal:** Search for symbols across all files in the workspace.

**LSP methods:**
- `workspace/symbol`

**How it works:**
1. On query, iterate `DefinitionTable::iter()` across all analyzed files
2. Filter by query string (fuzzy match against `DefinitionInfo.name`)
3. Map `DefinitionKind` to `SymbolKind` (same mapping as Phase 6)
4. Return `Vec<SymbolInformation>` with file locations

**Depends on:** Phase 9 (multi-file workspace support)

**Status:** Not started

---

## Phase 16: Folding Ranges

**Core support: Ready** — AST block structures available today

**Goal:** Collapsible regions for functions, structs, blocks, and comments.

**LSP methods:**
- `textDocument/foldingRange`

**How it works:**
1. Walk the AST for block-containing nodes (functions, structs, enums, interfaces, match arms, if/else blocks)
2. Each node's span defines a folding range (from opening `{` to closing `}`)
3. Doc comment runs (consecutive `DocComment` tokens) form comment folding ranges
4. Import groups form import folding ranges

**Implementation notes:**
- Pure AST walk, no resolver or type checker needed
- Similar to Phase 6 (document symbols) but produces `FoldingRange` instead of `DocumentSymbol`

**Status:** Not started

---

## Phase 17: Selection Ranges

**Core support: Ready** — AST node spans available today

**Goal:** Smart expand/shrink selection based on AST structure.

**LSP methods:**
- `selectionRange`

**How it works:**
1. Find the innermost AST node at the cursor position
2. Build a chain of increasingly larger spans: identifier → expression → statement → block → function → file
3. Return as nested `SelectionRange` (each with a parent)

**Implementation notes:**
- Uses the AST visitor to find containing nodes at each level
- Each `SelectionRange.range` is the span of the AST node at that level
- No resolver or type checker needed

**Status:** Not started

---

## Phase 18: Formatting

**Core support: Not yet available** — requires a new `spark-fmt` tool

**Goal:** Auto-format Spark source code.

**LSP methods:**
- `textDocument/formatting`
- `textDocument/rangeFormatting`

**Notes:** Requires a Spark formatter. This could be a separate `tools/spark-fmt` crate that the LSP server delegates to. Formatting rules TBD.

**Status:** Not started — blocked on `spark-fmt`

---

## Future / Stretch Goals

These features have varying levels of core support and are candidates for later phases:

| Feature | LSP Method | Core Support | Notes |
|---|---|---|---|
| Call hierarchy | `callHierarchy/incomingCalls`, `outgoingCalls` | Minor addition needed | Requires building a call graph from resolved AST; `ResolutionMap` has the raw data |
| Code lens | `textDocument/codeLens` | Ready | Reference counts via `find_references()`, run/test annotations |
| Go-to-type-definition | `textDocument/typeDefinition` | Ready | `TypeTable::get_def_type()` → resolve type's `DefId` → `DefinitionInfo.span` |
| Go-to-implementation | `textDocument/implementation` | Minor addition needed | Needs interface → implementing-struct mapping (not currently tracked) |
| Document highlights | `textDocument/documentHighlight` | Ready | Same as find-references but within a single file, with read/write distinction |
| Linked editing ranges | `textDocument/linkedEditingRange` | Ready | For struct field + constructor field pairs |
| Incremental compilation | N/A (internal) | Not yet available | Only re-check changed functions; requires dependency tracking in core |
