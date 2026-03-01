# Spark Task Tracker

All open work items for the Spark compiler. Each task has a ticket ID used to cross-reference
code comments (`// TODO [SPARK-001]`) with this file. The TODO Tree VS Code extension picks up
these tags automatically — open the tree view to jump between tasks and their code locations.

---

## How to add a new task

1. **Pick the next ID** — find the highest `SPARK-NNN` number below and increment by 1.
2. **Add an entry** using the template at the bottom of this section.
3. **Add code comments** in every relevant file using the matching tag:
   ```rust
   // TAG [SPARK-NNN] Brief description of what needs to change here
   ```
   Where `TAG` is one of `TODO`, `FIXME`, `BUG`, `PERF`, `HACK`, `NOTE` (same tag used in the heading).
4. **Tag type** — Use the tag that best describes _why_ the comment exists:
   | Tag | Meaning |
   |--------|------------------------------------------------------|
   | `TODO` | Planned work that has not started |
   | `FIXME`| Broken behaviour that needs correcting |
   | `BUG` | User-facing correctness issue |
   | `PERF` | Performance improvement opportunity |
   | `HACK` | Temporary workaround that must be replaced |
   | `NOTE` | Context for future readers, no action required |
5. **Research / planning tasks** — if the task type is `research` or `planning`, create a file
   `tasks/research/SPARK-NNN.md` with findings or deliverables.

### Task template

```markdown
### `TAG` [SPARK-NNN] — Title

- **Type**: bug | feature | refactor | research | performance
- **Effort**: low | medium | high | very-high
- **Category**: DX | performance | maintainability
- **Status**: open | in-progress | done
- **Crates**: crate_a, crate_b
- **Files**:
  - `crates/crate_a/src/file.rs` — what needs to change
- **Description**: What and why. Link to design docs if applicable.
```

### Rules

- One task per heading. Format: ``### `TAG` [SPARK-NNN] — Title`` so TODO Tree indexes the tag.
- When a task is completed, remove its full block from the open sections and add a one-line
  summary to the **Completed** section at the bottom of this file.
- Keep tasks ordered by ID within each priority section.
- Every code comment referencing a task **must** include the ticket ID so TODO Tree can link them.

---

## Priority 1 — Bugs (user-facing correctness)

### `BUG` [SPARK-031] — Nested RC decrement only frees one level deep (memory leak)

- **Type**: bug
- **Effort**: high
- **Category**: DX
- **Status**: open
- **Crates**: spark_runtime, spark_codegen, spark_drop
- **Files**:
  - `crates/spark_runtime/src/lib.rs` — `__spark_list_rc_dec` and `__spark_map_rc_dec` only decrement
    direct child pointers' RC fields; if a child reaches RC 0, its contents are never freed
- **Description**: When a `List[List[String]]` is freed, the outer list decrements each inner list's
  RC. But if an inner list reaches RC 0, nothing frees its data buffer or decrements the strings it
  contains — they leak. Acknowledged in source: "nested RC dec is one level deep". Fix requires
  either generating typed destructor functions at compile time (similar to closure env destructors)
  or a runtime type-dispatch table so `rc_dec` can recursively free arbitrarily nested RC types.

### `BUG` [SPARK-033] — List sort only works correctly for i64 elements

- **Type**: bug
- **Effort**: medium
- **Category**: DX
- **Status**: open
- **Crates**: spark_runtime
- **Files**:
  - `crates/spark_runtime/src/lib.rs` — `__spark_list_sort` casts all elements to `i64` and sorts
    as integers
- **Description**: `__spark_list_sort` reinterprets every element as `i64` regardless of actual type.
  Sorting a `List[Float]` produces wrong ordering (IEEE 754 bit patterns ≠ numeric order for
  negatives), and sorting `List[String]` compares pointer addresses instead of string contents.
  Needs a type-aware comparison: either pass a comparator function pointer from codegen, or pass a
  type tag so the runtime can dispatch to the correct comparison logic.

---

## Priority 2 — Architecture / Performance

### `PERF` [SPARK-005] — Reduce heavyweight Clone through pipeline

- **Type**: performance
- **Effort**: medium
- **Category**: performance
- **Status**: open
- **Crates**: spark_driver, spark_types, spark_resolve
- **Files**:
  - `crates/spark_driver/src/pipeline.rs` — `.clone()` calls on TypeTable, ModuleGraph, ASTs
  - `crates/spark_types/src/table.rs` — `#[derive(Clone)]` on TypeTable (13 HashMaps)
  - `crates/spark_resolve/src/resolved_ast.rs` — `#[derive(Clone)]` on ResolvedAst
- **Description**: `TypeTable` (13 HashMap fields + DefTable) is cloned 3×, `ModuleGraph` 2×, and
  all parsed ASTs are deep-cloned before module resolution. Use `Rc`/`Arc` wrapping or refactor
  the pipeline to pass a single `CompilationContext` by `&mut` reference. Closely related to
  SPARK-004 — consider tackling together.

### `PERF` [SPARK-030] — Map/Set O(n) linear scan → hash-based implementation

- **Type**: performance
- **Effort**: high
- **Category**: performance
- **Status**: open
- **Crates**: spark_runtime
- **Files**:
  - `crates/spark_runtime/src/lib.rs` — `SparkMap` (lines 247-583) and `SparkSet` (lines 389-643)
    use flat arrays with `memcmp` for every operation
- **Description**: `SparkMap` stores keys/values in two flat byte buffers and does a linear scan with
  `memcmp` on every insert, get, contains, and remove — O(n) per operation, O(n²) to build a map
  of n entries. `SparkSet` is literally a `SparkList` with dedup, same O(n) per operation. Replace
  both with hash-based implementations (e.g., open-addressing with FNV/SipHash). This is the single
  highest-impact runtime performance improvement — maps and sets are unusable at scale.

### `PERF` [SPARK-032] — String interpolation creates O(n²) intermediate allocations

- **Type**: performance
- **Effort**: medium
- **Category**: performance
- **Status**: open
- **Crates**: spark_runtime, spark_codegen
- **Files**:
  - `crates/spark_codegen/src/cranelift/emit_rvalue.rs` — chains left-to-right `__spark_string_concat`
    calls, creating n-1 intermediate heap strings
  - `crates/spark_runtime/src/lib.rs` — `__spark_string_concat` allocates a new buffer per call
- **Description**: String interpolation `"a{b}c{d}e"` emits a chain of `__spark_string_concat` calls:
  `concat(concat(concat(concat("a", b_str), "c"), d_str), "e")`. Each call allocates a new buffer
  and copies all preceding parts. For n parts: n-1 intermediate allocations, O(n²) total bytes
  copied. Fix: add `__spark_string_concat_many(parts: *const *mut u8, count: i64) -> *mut u8` that
  pre-computes total length, allocates once, and copies all parts in a single pass.

### `PERF` [SPARK-034] — Functional list ops always copy even when sole owner

- **Type**: performance
- **Effort**: low
- **Category**: performance
- **Status**: open
- **Crates**: spark_runtime
- **Files**:
  - `crates/spark_runtime/src/lib.rs` — `__spark_list_sort` (line 742), `__spark_list_take`
    (line 776), `__spark_list_reverse` (line 797) always allocate a new list
- **Description**: `sort`, `take`, and `reverse` unconditionally allocate a new list and copy
  elements, even when `rc == 1` (sole owner). When the list is uniquely owned, these operations
  could modify the buffer in-place and return the same pointer, avoiding a heap allocation and
  full element copy. Check `rc == 1` at the top of each function and branch to an in-place path.

---

## Priority 3 — Features (language completeness)

### `TODO` [SPARK-008] — References end-to-end (`Ref[T]`)

- **Type**: feature
- **Effort**: medium
- **Category**: DX
- **Status**: open
- **Crates**: spark_parser, spark_types
- **Files**:
  - `crates/spark_parser/src/types.rs` — `Ref` type parsing (requires 2 args, should accept 1)
  - `crates/spark_types/src/check_expr.rs` — implicit borrow coercion for `Ref[T] = value`
- **Description**: Backend is fully wired (borrow checker, MIR, codegen). Needs: (a) parser support
  for `Ref[T]` with 1 arg and inferred lifetime, (b) implicit borrow coercion in the type checker
  when a `Ref[T]`-typed binding is initialized from a plain `T` value. Small surface area, high
  payoff — unlocks the entire borrow-checking story. See `docs/KNOWN_ISSUES.md`.

### `TODO` [SPARK-012] — Tuples E2E test coverage

- **Type**: feature
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: spark_driver
- **Files**:
  - `crates/spark_driver/tests/e2e.rs` — add tuple E2E tests
  - `crates/spark_driver/tests/fixtures/` — add `.spark` fixture files
- **Description**: Tuple types have full type-system support but no E2E tests. Quick win for
  confidence. Create fixtures exercising tuple creation, field access, and function returns.

### `PERF` [SPARK-014] — E2E tests serialized via mutex (per-test temp files)

- **Type**: performance
- **Effort**: low
- **Category**: performance
- **Status**: open
- **Crates**: spark_driver
- **Files**:
  - `crates/spark_driver/tests/e2e.rs` — shared `/tmp/spark_output.o` behind mutex
- **Description**: All E2E tests share a single temp path for the intermediate object file, so they
  run sequentially behind a mutex. Use `tempfile::NamedTempFile` or a per-test temp directory to
  allow parallel execution.

---

## Priority 4 — Maintainability / Long-term

### `TODO` [SPARK-015] — Centralize built-in function registry

- **Type**: refactor
- **Effort**: medium
- **Category**: maintainability
- **Status**: open
- **Crates**: spark_resolve, spark_types, spark_codegen
- **Files**:
  - `crates/spark_resolve/src/resolver.rs` — `register_builtins()`
  - `crates/spark_types/src/checker.rs` — built-in type assignments
  - `crates/spark_codegen/src/cranelift/emit_stmt.rs` — built-in dispatch
- **Description**: `print`, `sort`, `reverse`, etc. are registered by name independently in the
  resolver, typed in the type checker, and dispatched in codegen. Adding a new built-in requires
  touching 3+ crates. Create a centralized `BuiltinRegistry` (similar to the `StdlibModule`
  manifest in `docs/design-notes/standard-library.md`) that all phases consult.

### `PERF` [SPARK-016] — Arena allocation for IR nodes

- **Type**: performance
- **Effort**: high
- **Category**: performance
- **Status**: open
- **Crates**: spark_ast, spark_hir, spark_mir
- **Files**:
  - `crates/spark_ast/src/expr.rs` — `Box`/`Vec` heap allocations
  - `crates/spark_mir/src/mir.rs` — `Box`/`Vec` heap allocations
- **Description**: AST/HIR/MIR nodes use `Box`/`Vec` heap allocation. A bump arena (e.g. `bumpalo`)
  would improve cache locality, reduce allocator pressure, and eliminate per-node drop overhead.
  Consider introducing incrementally: MIR first (monomorphization clones entire function bodies),
  then HIR, then AST.

### `TODO` [SPARK-017] — Thread-safe interner for LSP / parallelism

- **Type**: refactor
- **Effort**: medium
- **Category**: maintainability
- **Status**: open
- **Crates**: spark_intern
- **Files**:
  - `crates/spark_intern/src/lib.rs` — thread-local interner, leaked strings
- **Description**: `Sym` handles are thread-local — calling `intern()` from another thread panics.
  Fine for a batch compiler but blocks future parallel compilation and is a slow memory leak for
  long-lived processes (LSP). Options: (a) global interner behind `DashMap` / `RwLock`,
  (b) per-thread interners with a merge step, (c) `lasso` crate.

### `TODO` [SPARK-018] — LSP ScopeMap for code completion

- **Type**: feature
- **Effort**: medium
- **Category**: DX
- **Status**: open
- **Crates**: spark_resolve, tools/spark-lsp
- **Files**:
  - `crates/spark_resolve/src/resolver.rs` — rib push/pop boundaries
  - `crates/spark_resolve/src/lib.rs` — export `ScopeMap` in `ResolverOutput`
- **Description**: The LSP needs a persistent `ScopeMap` to answer "what names are visible at byte
  offset X?" Currently the resolver's `RibStack` is transient. At each push/pop boundary, snapshot
  accumulated rib contents with the scope's span. Blocking dependency for LSP completion (Phase 7).
  See `docs/completion-requirements.md`.

---

## Priority 5 — Future features (design ready, not yet started)

### `TODO` [SPARK-019] — `parallel each` / `parallel each via` (Rayon)

- **Type**: feature
- **Effort**: medium
- **Category**: DX
- **Status**: open
- **Crates**: spark_intern, spark_lexer, spark_parser, spark_hir, spark_mir, spark_codegen, spark_runtime
- **Files**:
  - See `docs/design-notes/concurrency-model.md` for full implementation plan
- **Description**: CPU parallelism via Rayon. Lowest-risk concurrency feature — reuses existing
  callback ABI. Add `parallel` keyword, parse `parallel each` / `parallel each via`, dispatch to
  `__spark_parallel_map` / `__spark_parallel_foreach` runtime stubs.

### `TODO` [SPARK-020] — Standard library (`spark.*` modules)

- **Type**: feature
- **Effort**: high
- **Category**: DX
- **Status**: in-progress
- **Crates**: spark_stdlib, spark_resolve, spark_types, spark_codegen, spark_runtime
- **Files**:
  - See `docs/design-notes/standard-library.md` for full architecture
- **Description**: Namespaced, opt-in modules backed by native runtime functions. Start with
  `spark.math` and `spark.io` to validate the pipeline, then `spark.json`, `spark.net`, `spark.http`.
  Manifest-based architecture — function signatures + FFI symbol mappings.
  **Completed**: `spark.math`, `spark.io`, `spark.json`, `spark.time`, `spark.net`, `spark.http`.
  - `spark.net`: TCP (listen/accept/connect/read/write/shutdown), UDP (bind/send/recv), DNS resolve.
    Backed by `tokio::net` via `block_on`. Handle-table architecture with `Arc`-wrapped resources.
  - `spark.http`: Client (GET/POST/PUT/DELETE via `reqwest`), Server (HTTP/1.1 via `tokio::net` TCP
    with manual parsing). Handle-table for servers and pending requests.

### `TODO` [SPARK-021] — Async Phase 3 v2 (segmented suspension)

- **Type**: feature
- **Effort**: very-high
- **Category**: DX
- **Status**: open
- **Crates**: spark_mir, spark_codegen, spark_runtime
- **Files**:
  - `crates/spark_mir/src/async_transform.rs` — state machine transform
- **Description**: Complete cooperative async state machine: segmented poll functions that
  suspend/resume at each await point. v1 (complete) calls the original function synchronously.
  v2 requires splitting function bodies at await points into state machine states, saving/restoring
  live variables. See `docs/design-notes/concurrency-model.md`.

### `PERF` [SPARK-022] — Lazy thunk generation

- **Type**: performance
- **Effort**: low
- **Category**: performance
- **Status**: open
- **Crates**: spark_codegen
- **Files**:
  - `crates/spark_codegen/src/cranelift/mod.rs` — thunk generation loop
  - `crates/spark_mir/src/mir.rs` — `Constant::FunctionRef` usage analysis
- **Description**: Thunks (`__thunk_<name>` wrappers) are generated for ALL named functions even if
  never used as values. Only generate thunks for functions actually referenced in non-call positions
  (detectable from MIR `Constant::FunctionRef` usage). Reduces object file bloat.

---

## Research / Planning

> Research deliverables live in `tasks/research/SPARK-NNN.md`.

### `NOTE` [SPARK-025] — Pipeline ownership model redesign

- **Type**: research
- **Effort**: medium
- **Category**: maintainability
- **Status**: open
- **Crates**: spark_driver, spark_types, spark_hir, spark_mir
- **Files**:
  - `tasks/research/SPARK-025.md`
- **Description**: SPARK-004, SPARK-005, and SPARK-006 share a root cause: the pipeline's ownership
  model where each phase consumes its input. Research the best approach — `CompilationContext`
  struct passed by `&mut`, `Rc`-based sharing, or salsa-style queries. Document trade-offs and a
  migration plan. Deliverables go in `tasks/research/SPARK-025.md`.

### `NOTE` [SPARK-026] — Incremental compilation feasibility

- **Type**: research
- **Effort**: low
- **Category**: performance
- **Status**: open
- **Crates**: all
- **Files**:
  - `tasks/research/SPARK-026.md`
- **Description**: Every change triggers a full re-compilation. Research feasibility of incremental
  compilation (salsa, query-based architecture). Scope the effort, identify breaking changes to
  current data structures, and estimate ROI. Deliverables go in `tasks/research/SPARK-026.md`.

---

## Completed

<!-- One-line summaries only. See git log for implementation details. -->
### `BUG` [SPARK-001] — String interpolation crashes codegen on function calls (completed 2026-02-08) Full interpolation pipeline implemented
### `BUG` [SPARK-002] — String interpolation produces empty output for variables (completed 2026-02-08) Runtime conversion/concat functions implemented
### `BUG` [SPARK-003] — Captured RC values leak when closure env is freed (completed 2026-02-17) Env header expanded with destructor pointer
### `PERF` [SPARK-004] — Eliminate double lex/parse/resolve in pipeline (completed 2026-02-17) Type checker borrows ASTs instead of consuming
### `TODO` [SPARK-006] — Replace Span-keyed expression types with ExprId (completed 2026-02-17) ExprId with atomic counter replaces Span keys
### `TODO` [SPARK-013] — Multi-file module E2E test coverage (completed 2026-02-17) multifile/ and multimod/ project fixtures with E2E tests
### `BUG` [SPARK-028] — Closure captures don't RC-inc copied RC values (completed 2026-02-17) RC inc emitted for Copy operands in emit_closure_construction
### `FIXME` [SPARK-007] — Eliminate production unwrap/panic calls (completed 2026-02-27) Replaced ~16 unwrap/panic calls with error recovery or ICE-prefixed expect messages
### `BUG` [SPARK-011] — Built-in collection methods hardcoded to [Int] (completed 2026-02-27) sort/take/reverse now work for any List[T] via call-site adaptation; contains validates elem type
### `TODO` [SPARK-009] — `via` pipeline operator (completed 2026-02-27) Desugars `expr via func{args}` to `func(expr, args)` call via PipelineApply
### `TODO` [SPARK-010] — Generic constraint validation (completed 2026-02-27) Constraints on generic type params validated at call sites and instantiation sites
### `TODO` [SPARK-027] — Operators on Ref[T] auto-deref (completed 2026-02-27) Auto-dereference through Ref[T] for arithmetic, comparison, and compound-assignment operators
### `BUG` [SPARK-029] — Assignment through mutable ref parameter crash (completed 2026-02-27) DCE now tracks base local of projected assignments as used
### `TODO` [SPARK-023] — Reborrowing and two-phase borrows (completed 2026-02-27) Borrow checker supports re-borrowing and x.method(&x) patterns
### `PERF` [SPARK-024] — O(n²) borrow conflict detection (completed 2026-02-27) Optimized active loan iteration in conflict detection
