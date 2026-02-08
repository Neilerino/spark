# Spark LSP Server — Roadmap

This document tracks the incremental path toward full Language Server Protocol support for the Spark language. Each phase builds on the previous one and maps LSP features to the core compiler crates that power them.

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

**Status:** [x] Complete

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

**Notes:** Basic token-level coloring uses only the lexer. All identifiers are classified as `variable`. Context-aware classification (distinguishing function/type/parameter names) requires resolver data and will be enhanced in later phases.

**Status:** [x] Complete

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

**Status:** [x] Complete

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

**Status:** [x] Complete

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

**Status:** [x] Complete

---

## Phase 6: Document Symbols

**Goal:** Outline view showing all top-level declarations.

**LSP methods:**
- `textDocument/documentSymbol`

**Core crate:** `spark_ast` (walk `Ast.items`)

**Symbols reported:**
- Functions → `SymbolKind::Function`
- Structs → `SymbolKind::Struct`
- Enums → `SymbolKind::Enum`
- Interfaces → `SymbolKind::Interface`
- Constants → `SymbolKind::Constant`
- Type aliases → `SymbolKind::TypeParameter`

**Status:** [ ] Not started

---

## Phase 7: Completion

**Goal:** Context-aware code completion.

**LSP methods:**
- `textDocument/completion`

**Core crates:** `spark_resolve` (scope/rib stack), `spark_types` (`TypeTable`)

**Completion contexts:**
- After `.` → suggest struct fields (look up receiver type, list fields)
- General position → suggest in-scope names from resolver
- Type position → suggest known types (built-in + user-defined)
- Include type annotations in completion detail

**Status:** [ ] Not started

---

## Phase 8: Code Actions & Quick Fixes

**Goal:** Suggest automated fixes for common errors.

**LSP methods:**
- `textDocument/codeAction`

**Examples:**
- "missing `return`" → insert `return` before trailing expression
- Unresolved name → suggest import (when multi-file support exists)
- Type mismatch → suggest `.toInt()` / `.toString()` conversions
- Unused variable → prefix with `_`

**Status:** [ ] Not started

---

## Phase 9: Rename

**Goal:** Rename a symbol and all its references.

**LSP methods:**
- `textDocument/rename`
- `textDocument/prepareRename`

**Depends on:** Phase 5 (find references)

**How it works:**
1. `prepareRename`: verify cursor is on a renameable symbol
2. `rename`: find all references + definition, produce `WorkspaceEdit` with text edits

**Status:** [ ] Not started

---

## Phase 10: Formatting

**Goal:** Auto-format Spark source code.

**LSP methods:**
- `textDocument/formatting`
- `textDocument/rangeFormatting`

**Notes:** Requires a Spark formatter. This could be a separate `tools/spark-fmt` crate that the LSP server delegates to. Formatting rules TBD.

**Status:** [ ] Not started

---

## Phase 11: VS Code Extension

**Goal:** First-class VS Code support for Spark development.

**Deliverables:**
- `tools/spark-lsp/editors/vscode/` directory
- `package.json` with:
  - Language configuration (`.spark` file association)
  - Comment tokens (`//`), bracket pairs
  - Semantic token legend matching Phase 2
- Extension TypeScript code that launches `spark-lsp` binary
- TextMate grammar for basic syntax highlighting (fallback when LSP is starting)
- Publish to VS Code marketplace

**Status:** [ ] Not started

---

## Future / Stretch Goals

These depend on core compiler improvements:

- **Incremental compilation** — only re-check changed functions (requires dependency tracking in core)
- **Multi-file / workspace support** — depends on `spark_modules` multi-file compilation
- **Inlay hints** — show inferred types inline on bindings (`textDocument/inlayHint`)
- **Signature help** — parameter hints while typing function calls (`textDocument/signatureHelp`)
- **Code lens** — show reference counts, run/test annotations (`textDocument/codeLens`)
- **Folding ranges** — collapsible regions for functions, structs, blocks
- **Selection ranges** — smart expand/shrink selection based on AST structure
- **Call hierarchy** — incoming/outgoing calls for a function
- **Workspace symbols** — search across all files in a project
