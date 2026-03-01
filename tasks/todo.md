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
  - `crates/spark_driver/tests/run-pass/` — add `.spark` fixture files
- **Description**: Tuple types have full type-system support but no E2E tests. Quick win for
  confidence. Create fixtures exercising tuple creation, field access, and function returns.

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

## Priority 4.5 — Documentation (spec–implementation alignment)

### `TODO` [SPARK-035] — Spec: update `.length` to `.len()`

- **Type**: documentation
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: (none — spec only)
- **Files**:
  - `docs/lang-spec.md` — String Methods section (line ~303), Collections section (line ~1939)
- **Description**: The spec documents `s.length` (property) for String and `list.length` for List,
  but the implementation uses `.len()` (method call) everywhere. Update all references in the spec
  to use `.len()`.

### `TODO` [SPARK-036] — Spec: document global `print()` builtin

- **Type**: documentation
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: (none — spec only)
- **Files**:
  - `docs/lang-spec.md` — Standard Library Overview > I/O section (line ~1908)
- **Description**: The spec shows `io.print()` / `io.println()` requiring an import, but the
  implementation provides a global `print()` builtin with no import. Document `print()` as a
  language builtin in the spec. Keep `io.print` / `io.println` as explicit alternatives.

### `TODO` [SPARK-037] — Spec: document `Ref[T]` shorthand (no lifetime)

- **Type**: documentation
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: (none — spec only)
- **Files**:
  - `docs/lang-spec.md` — Memory Model section (line ~1499)
- **Description**: The spec only shows `Ref[T, A]` with an explicit lifetime parameter. The
  implementation supports `Ref[T]` (no lifetime) for the common case. Update the Memory Model
  section to present `Ref[T]` as the default form and `Ref[T, A]` as the advanced opt-in for
  lifetime relationships — consistent with the "progressive complexity" philosophy.

### `TODO` [SPARK-038] — Spec: document `Channel[T]()` constructor with explicit type arg

- **Type**: documentation
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: (none — spec only)
- **Files**:
  - `docs/lang-spec.md` — Concurrency > Channels section (line ~1678)
- **Description**: The spec shows `Channel()` without a type parameter in the constructor, but the
  implementation requires `Channel[T]()`. Update the spec to use `Channel[Int]()`, etc. This
  aligns with the language's "explicitness over implicitness" principle.

### `TODO` [SPARK-039] — Spec: document `each` / `each via` with function references

- **Type**: documentation
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: (none — spec only)
- **Files**:
  - `docs/lang-spec.md` — Control Flow > Each section (line ~782), Via section (line ~802)
- **Description**: The spec only shows `each` and `each via` with inline lambda blocks. The
  implementation also supports bare function references: `nums each show` and
  `nums each via double`. Document both forms.

### `TODO` [SPARK-040] — Spec: unify `to` section (pipeline + narrowing)

- **Type**: documentation
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: (none — spec only)
- **Files**:
  - `docs/lang-spec.md` — Control Flow > To section (line ~829), Partials in Pipelines (line ~1002)
- **Description**: `to` serves dual purpose: type narrowing with a block (`result to { ... }`) and
  pipeline application with a function (`5 to double`). The spec describes narrowing in the To
  section and buries pipeline usage in the Partials section. Create a unified To section that
  explains both uses clearly.

### `TODO` [SPARK-041] — Spec: document `spawn { expr }` expression form

- **Type**: documentation
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: (none — spec only)
- **Files**:
  - `docs/lang-spec.md` — Concurrency > Spawn section (line ~1663)
- **Description**: The spec shows `return` inside spawn blocks, but the implementation uses
  expression form where the last expression is the value (consistent with `if`, `match`, and
  `each via` blocks). Update the spec examples to use expression form.

### `TODO` [SPARK-042] — Spec: document struct field comma optionality

- **Type**: documentation
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: (none — spec only)
- **Files**:
  - `docs/lang-spec.md` — Structs section (line ~1024)
- **Description**: The spec shows struct field definitions without commas, but the implementation
  accepts both comma-separated and newline-separated fields. Document that field separators
  (commas) are optional when fields are newline-delimited.

### `TODO` [SPARK-043] — Spec: document `import spark.` prefix for stdlib

- **Type**: documentation
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: (none — spec only)
- **Files**:
  - `docs/lang-spec.md` — Modules and Visibility > Importing section (line ~1822),
    Standard Library Overview (line ~1875)
- **Description**: The spec shows `import math`, `import io` etc. but the implementation uses
  `import spark.io { ... }`, `import spark.math { ... }`. Update the spec to show the correct
  dotted-path import syntax for standard library modules.

### `TODO` [SPARK-044] — Spec: document simpler interface method syntax

- **Type**: documentation
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: (none — spec only)
- **Files**:
  - `docs/lang-spec.md` — Interfaces section (line ~1201)
- **Description**: The spec leads with the advanced `fn serialize[T: Self](item: T): String` form
  for interface methods. The implementation supports a simpler `fn display(): String` syntax
  without generic parameters. Restructure the Interfaces section to lead with the simple form and
  present `[T: Self]` as an advanced option.

### `TODO` [SPARK-045] — Spec: document `parallel each` / `parallel each via`

- **Type**: documentation
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: (none — spec only)
- **Files**:
  - `docs/lang-spec.md` — new section needed (Concurrency or Control Flow)
- **Description**: `parallel each` and `parallel each via` are fully implemented and tested but
  have zero spec coverage. Add a section documenting the syntax, semantics, COW capture behavior,
  and thread-safety restrictions (no mutable captures, no Ref captures).

### `TODO` [SPARK-046] — Spec: document closure capture-by-snapshot semantics

- **Type**: documentation
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: (none — spec only)
- **Files**:
  - `docs/lang-spec.md` — Functions > Inline Functions section (line ~952)
- **Description**: Closures capture outer variables by snapshot (copy at creation time). Mutations
  to the original variable after closure creation are not seen by the closure. This is a critical
  semantic detail that is not documented. Add a subsection with the snapshot example:
  `x: Int! = 10; f = fn() { x }; x = 42; f() // returns 10`.

### `TODO` [SPARK-047] — Spec: document `each` COW/copy semantics for captured mutables

- **Type**: documentation
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: (none — spec only)
- **Files**:
  - `docs/lang-spec.md` — Control Flow > Each section (line ~782)
- **Description**: Mutations to outer mutable variables inside `each` do not persist after iteration
  completes (`x: Int! = 0; nums each (n): Void { x = x + n }; print(x)` prints `0`). This COW
  behavior is surprising and undocumented. Add a note explaining the semantics and rationale
  (safety for potential parallelization). Consider whether the compiler should warn when `each`
  captures a mutable variable it cannot actually mutate.

### `TODO` [SPARK-048] — Spec: document struct mutable field markers

- **Type**: documentation
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: (none — spec only)
- **Files**:
  - `docs/lang-spec.md` — Structs > Mutation section (line ~1054)
- **Description**: The spec only shows mutability on variables (`user: User! = ...`). The
  implementation also supports mutable markers on struct field definitions (`x: Int!` in a struct
  body). Document the semantics: what do field-level `!` markers mean vs. variable-level `!`?
  The compile-fail test shows the variable must still be `!` to mutate fields.

---

## Priority 4.6 — Spec design decisions needed

### `FIXME` [SPARK-049] — Resolve `await` in sync context vs. `block` keyword

- **Type**: research
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: spark_codegen, spark_types
- **Files**:
  - `docs/lang-spec.md` — Concurrency > Async/Await section (line ~1618), Running Async from
    Sync Context (line ~1652)
  - `crates/spark_types/src/checker.rs` — await validation
  - `crates/spark_codegen/src/cranelift/` — await lowering
- **Description**: The spec defines a `block` keyword for running async functions from sync context,
  but the implementation allows `await` directly in non-async functions and `block` is never used.
  This needs a deliberate design decision:
  - **Option A**: Remove `block` from the spec, let `await` work everywhere (current behavior)
  - **Option B**: Enforce `block` in sync context, restrict `await` to `async fn` bodies
  Option B preserves the colored-function boundary which aids reasoning about async code, but
  Option A is simpler. Document the decision in `tasks/research/SPARK-049.md`.

### `FIXME` [SPARK-050] — I/O error handling model contradicts language philosophy

- **Type**: bug
- **Effort**: high
- **Category**: DX
- **Status**: open
- **Crates**: spark_stdlib, spark_runtime, spark_codegen
- **Files**:
  - `crates/spark_stdlib/spark/io.spark` — current API surface
  - `crates/spark_runtime/src/lib.rs` — `__spark_io_*` functions
  - `docs/lang-spec.md` — Standard Library Overview > I/O section (line ~1908)
- **Description**: The spec says I/O functions return union types (`String | IoError`) with `!`
  propagation, but the implementation uses a C-style `lastError()` sentinel pattern where
  `readFile()` returns a bare `String` and errors are checked via a separate `lastError()` call.
  This contradicts the core principle "errors are values, not exceptions" and makes it easy to
  forget error checking. The implementation should be updated to return union types matching the
  spec's design.

---

## Priority 4.7 — Missing E2E test coverage

### `TODO` [SPARK-051] — E2E tests: string interpolation

- **Type**: feature
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: spark_driver
- **Files**:
  - `crates/spark_driver/tests/e2e.rs` — add test
  - `crates/spark_driver/tests/run-pass/basics/` — add fixture
- **Description**: String interpolation (`"Hello, {name}!"`) is a core language feature with no E2E
  test. Add fixtures covering: variable interpolation, expression interpolation, nested braces,
  escaped braces `\{`.

### `TODO` [SPARK-052] — E2E tests: match with guards

- **Type**: feature
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: spark_driver
- **Files**:
  - `crates/spark_driver/tests/e2e.rs` — add test
  - `crates/spark_driver/tests/run-pass/basics/` — add fixture
- **Description**: Match with guard clauses (`n if n < 13 -> "child"`) is documented in the spec
  but has no E2E test.

### `TODO` [SPARK-053] — E2E tests: enums with associated data

- **Type**: feature
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: spark_driver
- **Files**:
  - `crates/spark_driver/tests/e2e.rs` — add test
  - `crates/spark_driver/tests/run-pass/` — add fixture dir or file
- **Description**: Enums with associated data (`Result.Success(42)`) and matching on variants are
  core to the error handling story but have no E2E test coverage.

### `TODO` [SPARK-054] — E2E tests: optional chaining `?.` and null coalescing `??`

- **Type**: feature
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: spark_driver
- **Files**:
  - `crates/spark_driver/tests/e2e.rs` — add test
  - `crates/spark_driver/tests/run-pass/` — add fixture
- **Description**: Optional chaining (`user?.name`) and null coalescing (`x ?? default`) are safety
  features documented in the spec with no E2E tests.

### `TODO` [SPARK-055] — E2E tests: `as` / `is` type operators

- **Type**: feature
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: spark_driver
- **Files**:
  - `crates/spark_driver/tests/e2e.rs` — add test
  - `crates/spark_driver/tests/run-pass/` — add fixture
- **Description**: `is` (type check) and `as` (type cast) operators are in the spec but have no
  E2E test coverage.

### `TODO` [SPARK-056] — E2E tests: `const` declarations

- **Type**: feature
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: spark_driver
- **Files**:
  - `crates/spark_driver/tests/e2e.rs` — add test
  - `crates/spark_driver/tests/run-pass/basics/` — add fixture
- **Description**: `const` declarations (`const MAX: Int = 100`) are documented in the spec but
  have no E2E test coverage.

### `TODO` [SPARK-057] — E2E tests: type aliases (`alias`, `type`)

- **Type**: feature
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: spark_driver
- **Files**:
  - `crates/spark_driver/tests/e2e.rs` — add test
  - `crates/spark_driver/tests/run-pass/` — add fixture
- **Description**: Both `alias` (interchangeable) and `type` (distinct) aliases are in the spec
  but have no E2E tests.

### `TODO` [SPARK-058] — E2E tests: Set and Map operations

- **Type**: feature
- **Effort**: low
- **Category**: DX
- **Status**: open
- **Crates**: spark_driver
- **Files**:
  - `crates/spark_driver/tests/e2e.rs` — add tests
  - `crates/spark_driver/tests/run-pass/collections/` — add fixtures
- **Description**: Set literals, Set operations (add, remove, contains, union, intersection) and
  Map access/mutation are in the spec but have no E2E tests beyond `map.len()`.

---

## Priority 5 — Future features (design ready, not yet started)

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
  live variables.

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
### `PERF` [SPARK-014] — E2E tests serialized via mutex (completed 2026-03-01) Per-process temp dirs in cmd_run(); mutex removed from e2e.rs
