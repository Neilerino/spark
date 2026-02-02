# spark_modules

Module resolution for the Spark compiler.

## Overview

This crate is Phase 3 of the Spark compiler pipeline. It takes parsed ASTs and produces a module dependency graph that is used by subsequent phases (name resolution, type checking, etc.).

## Responsibilities

- **File discovery**: Map module declarations to file paths
- **Module correlation**: Associate files with their declared modules
- **Using resolution**: Handle `using` declarations that join existing modules
- **Import resolution**: Resolve import paths to target modules
- **Dependency graph**: Build a directed graph of module dependencies
- **Cycle detection**: Detect and report circular imports
- **Export collection**: Gather public items per module
- **Compilation order**: Topologically sort modules for compilation

## Quick Start

```rust
use spark_modules::{resolve, ModuleResolverInput};
use std::collections::HashMap;
use std::path::PathBuf;

let input = ModuleResolverInput {
    asts: parsed_files,           // HashMap<FileId, Ast>
    root_path: PathBuf::from("./project"),
    file_paths: file_path_map,    // HashMap<FileId, PathBuf>
};

let output = resolve(input);

if output.diagnostics.has_errors() {
    // Handle errors
} else {
    // Use output.graph for name resolution
    for module_path in &output.graph.compilation_order {
        // Process modules in dependency order
    }
}
```

## Core Types

### `ModulePath`

A module path like `["user", "auth"]` representing `user.auth`.

```rust
let path = ModulePath::new(vec![sym("user"), sym("auth")]);
assert_eq!(path.to_string(), "user.auth");
assert_eq!(path.parent().unwrap().to_string(), "user");
```

### `ModuleGraph`

The resolved module dependency graph containing:
- `modules`: All modules indexed by path
- `dependencies`: Module -> modules it imports
- `dependents`: Module -> modules that import it
- `compilation_order`: Topologically sorted for compilation

### `ModuleInfo`

Information about a single module:
- `path`: The module's unique path
- `files`: Files belonging to this module
- `exports`: Public items exported from this module
- `re_exports`: Items re-exported from other modules

## Module System

Spark uses a file-based module system:

```spark
// user/user.spark
module user

pub struct User {
    id: Int
    name: String
}

pub fn createUser(name: String): User { ... }

// user/validation.spark
using user

fn validateUser(u: User): Bool { ... }  // Can access private items

// main.spark
import user { User, createUser }

fn main(): Void {
    u: User = createUser("Alice")
}
```

### Keywords

- `module X` - Declares a new module (file must be `X/X.spark` or `X.spark`)
- `using X` - Joins an existing module from the same directory
- `import X { Y }` - Imports specific items from module X
- `pub import X { Y }` - Re-exports items from module X
- `pub` prefix - Makes items visible outside the module

## Error Messages

| Code | Error |
|------|-------|
| E0401 | Module not found |
| E0402 | Private item import |
| E0403 | Circular dependency |
| E0404 | Duplicate module declaration |
| E0405 | File/module name mismatch |
| E0406 | Using unknown module |
| E0407 | Import item not found |

## Design Decisions

### Multi-pass Resolution

Resolution happens in distinct phases:
1. Discovery - Extract module info from ASTs
2. Correlation - Map files to modules
3. Using resolution - Process `using` declarations
4. Import resolution - Build dependency edges
5. Export collection - Gather public items
6. Cycle detection - Find circular dependencies
7. Topological sort - Compute compilation order

This allows collecting all information before validation, enabling better error messages.

### Tarjan's Algorithm for Cycles

We use Tarjan's strongly connected components algorithm for cycle detection:
- O(V+E) complexity
- Finds all cycles in one pass
- Reports all nodes in each cycle

### Module Path Semantics

- `module user` in `user/user.spark` → `ModulePath(["user"])`
- `module auth` in `user/auth/auth.spark` → `ModulePath(["user", "auth"])`
- Files with `using X` join the module declared by X

## Relationship to Other Crates

```text
spark_parser (produces AST)
      │
      ▼
spark_modules (builds module graph) ◄── THIS CRATE
      │
      ▼
spark_resolve (resolves names)
```

## Testing

```bash
cargo test -p spark_modules
```

38 tests covering:
- Module path operations
- Module discovery
- Cycle detection
- Export collection
- Topological sorting
- Error cases
