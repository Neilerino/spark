# Stream B: Type System Foundations — Mostly Complete

> ~~Method resolution~~ → Generics (type checking done, MIR mono remaining)

## Overview

Foundational type system features. Method resolution is complete. Generics type checking is complete; MIR monomorphization and codegen remain.

---

## ~~Item 1: Interface Validation & Method Resolution (P3.4)~~ — Complete

User-defined method resolution via `binding` declarations works end-to-end:

- `DefinitionKind::BindingMethod(DefId)` tracks binding methods
- Resolver: `collect_binding_methods()` allocates DefIds for each binding target method
- TypeTable: `type_methods` maps type DefId → `[(method_name, method_def_id)]`; `method_resolutions` maps call Span → method DefId
- Type checker pass 1: `collect_binding()` lowers method signatures, registers in `type_methods`
- Type checker pass 2: `check_user_method_call()` resolves `receiver.method()` by looking up `type_methods`
- Interface type checking validates that bindings implement all required methods with correct signatures
- HIR: `lower_binding()` lowers method bodies as regular `HirFunction`s
- MIR: `lower_method_call()` checks `method_resolutions` after builtin lookup
- Codegen: no changes needed — binding methods compile as regular functions

---

## Item 2: Generic Instantiation (P3.5) — Type Checking Complete

**Status**: Type checking phase works; MIR monomorphization and codegen pending.

### What's Done

- `Ty::Applied { base: DefId, args: Vec<TypeId> }` — instantiated generic types
- `Ty::Param(DefId)` — generic type parameters
- `substitute.rs` — `SubstMap`, `build_subst_map()`, `substitute()`/`substitute_mut()`, `infer_type_args()`
- `check_decl.rs` — `collect_generic_params()` extracts DefIds from AST generics, registers `Ty::Param` types
- `lower.rs` — `lower_named_def()` creates `Ty::Applied` when type args present, validates arity
- `check_expr.rs`:
  - `check_field_access()` handles `Ty::Applied` with type substitution
  - `check_struct_init()` infers type args from field values for generic structs
  - `check_call()` infers type args for generic function calls
- `is_compatible()`, `type_name()`, `exhaustive.rs`, `union.rs` — all handle `Ty::Applied`
- `spark_borrow` — `type_involves_ref()` handles `Ty::Applied`
- 13 new tests covering generic structs, functions, inference, arity errors, nested generics

### What's Remaining

1. **MIR monomorphization pass** (`spark_mir/src/monomorphize.rs`):
   - Collect instantiation requests from `TypeTable`
   - Deduplicate by `(DefId, Vec<TypeId>)`
   - Clone source `MirFunction`, substitute all `TypeId`s in locals, return type, rvalues
   - Allocate fresh DefIds for monomorphized functions
   - Rewrite call sites to target monomorphized DefIds
   - Remove original generic functions (they contain `Ty::Param` types codegen can't handle)

2. **Codegen for `Ty::Applied`** (`spark_codegen/src/layout.rs`):
   - Compute struct/enum layout from substituted field types
   - Handle in `emit_aggregate()`, `emit_field_access()`

3. **Constraint checking** (`T: Comparable`) — deferred until needed

---

## Key Files

| Purpose | Path |
|---------|------|
| Type definitions | `crates/spark_types/src/ty.rs` |
| Type table | `crates/spark_types/src/table.rs` |
| Type substitution | `crates/spark_types/src/substitute.rs` |
| Generic param collection | `crates/spark_types/src/check_decl.rs` |
| Type lowering (Applied creation) | `crates/spark_types/src/lower.rs` |
| Expression checking (field access, struct init, calls) | `crates/spark_types/src/check_expr.rs` |
| Method call type checking | `crates/spark_types/src/check_expr.rs` |
| Binding AST | `crates/spark_ast/src/decl.rs` (BindingDef) |
| Interface AST | `crates/spark_ast/src/decl.rs` (InterfaceDef) |
| Resolver | `crates/spark_resolve/src/lib.rs` |

## Reference Docs

- Language spec: `docs/lang-spec.md` (sections on interfaces, bindings, generics)
- Compiler architecture: `docs/compiler-architecture.md`
- Roadmap: `docs/roadmap.md` (items 3.4, 3.5)
