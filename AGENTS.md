# Working on Spark

Use [Linear](https://linear.app/neilwaddenpersonal/project/spark-f367ea1ad80c) for scope and status. Read the issue and check the current implementation and tests before changing behavior.

Give new issues a `[SPARK-NNN]` title prefix, label, and priority. Cross-reference the issue with a `// TAG [SPARK-NNN]` comment in affected source files. Put research deliverables in `tasks/research/SPARK-NNN.md`.

## Language changes

- Read the [language reference](docs/lang-spec.md) for syntax and public contracts. Check for an existing protocol, operator, or standard-library function before adding an API. For example, maps and sets use `Iterable` with the same traversal operations as lists. Give a separate API a distinct behavior or guarantee and test the public call form.
- Read the [compiler guide](docs/compiler-architecture.md) to find the stage that owns the behavior. Keep phase boundaries explicit. Preserve value representations across phases and at ABI boundaries. Share code where it expresses a shared invariant.
- When adding an AST node, update the [visitor](crates/spark_ast/src/visitor.rs) so consumers traverse it. Write comments for rationale or invariants the code cannot express.
- Report invalid source through diagnostics and recover where possible. Preserve source spans and make parser recovery advance or terminate. Use existing pre-interned symbols when checking keywords.
- Use `#![forbid(unsafe_code)]` outside `spark_runtime`. The native runtime uses unsafe code at its ABI and memory boundaries.

## Verification

- Cover observable contracts, meaningful failure paths, and regressions. Prefer [driver regressions](crates/spark_driver/tests/e2e.rs) for behavior that crosses phases and crate tests for phase-local invariants. Run both compiler modes when optimization or lowering could change results.
- Preserve observable behavior during a refactor. Update implementation-coupled tests when the contract stays intact. Agree on changes to the public contract before updating its tests.
- Follow the [CI commands](docs/compiler-architecture.md#development-checks) before publishing. See the [README](README.md) for build, run, and install commands.
