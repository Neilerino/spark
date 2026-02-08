# LSP Completion: Core Team Requirements

The LSP server needs the following from `spark_resolve` to implement scope-aware code completion (Phase 7). This should be considered when finalizing lambda/closure scope support.

---

## What's needed: A persistent `ScopeMap`

During name resolution, the `RibStack` tracks which names are visible at each scope level, but it's transient — it exists only during the resolution walk and is discarded. The LSP needs a way to query **"what names are visible at byte offset X?"** after resolution is complete.

## Proposed export: `ScopeMap`

```rust
/// A scope entry recording which names became visible at a span boundary.
pub struct ScopeEntry {
    /// The span of this scope (e.g., function body, block).
    pub span: Span,
    /// Value-namespace names visible in this scope (accumulated from all enclosing ribs).
    pub values: Vec<(Symbol, DefId)>,
    /// Type-namespace names visible in this scope (accumulated from all enclosing ribs).
    pub types: Vec<(Symbol, DefId)>,
}

/// Persistent scope map built during resolution.
/// Maps scope spans to the names visible within them.
pub struct ScopeMap {
    scopes: Vec<ScopeEntry>,
}

impl ScopeMap {
    /// Returns all names visible at a given byte offset.
    /// Finds the innermost scope containing the offset and returns its names.
    pub fn visible_at(&self, offset: u32) -> Option<&ScopeEntry>;
}
```

## How the resolver would populate it

At each `push_rib` / `pop_rib` boundary, snapshot the accumulated rib contents with the scope's span. The LSP can then binary-search for the innermost scope containing the cursor and enumerate its names.

## What this enables

1. **General completion**: suggest all in-scope values/types at cursor position
2. **Scope-filtered results**: only show names actually visible (not every local in the file)
3. **Shadowing-aware**: innermost scope wins, matching runtime semantics
4. **Lambda capture awareness**: once lambda scopes are finalized, captured names flow naturally

## What the LSP can do without this (current state)

- **Dot-completion** (struct fields, enum variants) via `TypeTable` — no scope needed
- **Type-position completion** via `DefinitionTable::iter()` filtered by `is_type()` — works but not scope-filtered
- **Top-level item completion** via `DefinitionTable::iter()` — works but includes every definition in the file

## Priority

This is a **blocking dependency** for LSP Phase 7 (Completion). Everything else in the LSP roadmap (Phases 8–11) can proceed without it.
