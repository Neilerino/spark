# Stream B: Type System Foundations

> Method resolution → Generics

## Overview

Foundational type system features. Method resolution is a prerequisite for many downstream features (notably `via` pipeline in Stream C). Generics are the highest-effort single item in the project.

**Conflicts with Stream C**: Both touch `spark_types` and `spark_resolve` heavily. If done in parallel with lambdas, coordinate carefully on type system changes (e.g., `Ty::Function` usage, method lookup paths).

**Recommended order**: Method resolution first, then generics (see roadmap "Recommended Implementation Order").

---

## Item 1: Interface Validation & Method Resolution (P3.4)

**Status**: Partial — built-in collection methods work, user-defined methods do not.

### What's Done

Built-in method dispatch for collection types via `DefinitionKind::BuiltinMethod`:
- `check_method_call()` at `spark_types/src/check_expr.rs:956-988` — routes by receiver type
- `check_list_method()` at `spark_types/src/check_expr.rs:992-1030`
- `check_map_method()` at `spark_types/src/check_expr.rs:1034-1063`
- `check_set_method()` at `spark_types/src/check_expr.rs:1067-1099`
- `check_string_method()` follows Set methods

### What's Remaining

Two parts:

#### Part A: Interface Validation

When a `binding Foo for Bar { ... }` is declared, verify that all methods required by interface `Foo` are implemented.

**Parser already handles this**:
- `parse_interface()` in `spark_parser/src/decl.rs` — parses `interface Name { methods }`
- `parse_binding()` in `spark_parser/src/decl.rs` — parses `binding Interface for Type { methods }`
- AST types: `InterfaceDef` and `BindingDef` in `spark_ast/src/decl.rs`

**What's needed**: In `spark_types`, after collecting all interface and binding definitions, check:
1. Every method in the interface has a corresponding method in the binding
2. Signatures match (parameter types, return type)
3. Report missing methods and signature mismatches as diagnostics

#### Part B: User-Defined Method Resolution

When `expr.method(args)` is encountered and the receiver type is a user-defined struct/enum:

1. Look up all bindings for the receiver type
2. Search those bindings for a method matching the name
3. Type-check the call using the found method's signature

Currently `check_method_call()` only dispatches to the built-in type-specific checkers. Extend it with a fallback path that searches binding definitions.

**Key consideration**: Method resolution must work for both direct type methods (`binding for MyStruct { fn foo() ... }`) and interface methods (`binding Printable for MyStruct { fn print() ... }`).

### Integration Points

- `spark_resolve` — binding definitions are collected during resolution. The resolver currently creates `DefinitionKind::Function` entries for binding methods; may need a new `DefinitionKind::Method` or to track the receiver type.
- `spark_types` — the type checker needs to query "what methods does type X have?"
- `spark_hir` — method calls need to resolve to specific function DefIds
- `spark_mir` — method calls lower to regular function calls (with receiver as first arg)

---

## Item 2: Generic Instantiation (P3.5)

**Status**: Type args parsed but ignored. This is the highest-effort item in the project.

### Current State

- Generics are parsed into AST:
  - `GenericParam` and `GenericConstraint` in `spark_ast/src/types.rs:124-160`
  - `FunctionDef.generics`, `StructDef.generics`, `EnumDef.generics`, `InterfaceDef.generics`, `BindingDef.generics` — all fields exist in `spark_ast/src/decl.rs`
- `Ty::Param` exists in `spark_types/src/ty.rs` but isn't used
- Type arguments on function calls and type expressions are parsed but ignored

### What's Needed

1. **Generic parameter tracking**: When type-checking a generic function/struct, register `Ty::Param` entries for each generic parameter. These act as type variables during checking.

2. **Type argument inference**: At call sites, infer type arguments from argument types, or accept explicit type annotations.

3. **Monomorphization pass**: Clone function bodies with type parameters substituted with concrete types. This happens at or after MIR — for each unique instantiation, create a new copy of the function with concrete types.

4. **Constraint checking**: If a generic has constraints (`T: Comparable`), verify the concrete type satisfies those interfaces. Requires method resolution (Item 1) to be working.

### Approach

Consider implementing in stages:
1. First: explicit type arguments only (no inference) — `foo[Int](x)`
2. Then: basic inference from argument types
3. Then: constraint checking

---

## Key Files

| Purpose | Path |
|---------|------|
| Method call type checking | `crates/spark_types/src/check_expr.rs:956+` |
| Built-in method checkers | `crates/spark_types/src/check_expr.rs:992+` |
| Interface AST | `crates/spark_ast/src/decl.rs` (InterfaceDef) |
| Binding AST | `crates/spark_ast/src/decl.rs` (BindingDef) |
| Interface parser | `crates/spark_parser/src/decl.rs` (parse_interface) |
| Binding parser | `crates/spark_parser/src/decl.rs` (parse_binding) |
| Generic param AST | `crates/spark_ast/src/types.rs:124-160` |
| Type definitions | `crates/spark_types/src/ty.rs` |
| Type table | `crates/spark_types/src/table.rs` |
| Resolver | `crates/spark_resolve/src/lib.rs` |
| Definition table | `crates/spark_resolve/src/lib.rs` (DefinitionTable) |

## Reference Docs

- Language spec: `docs/lang-spec.md` (sections on interfaces, bindings, generics)
- Compiler architecture: `docs/compiler-architecture.md`
- Roadmap: `docs/roadmap.md` (items 3.4, 3.5)
