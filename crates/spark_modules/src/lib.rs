//! # `spark_modules`
//!
//! Module resolution for the Spark compiler.
//!
//! This crate handles multi-file Spark projects by building a module dependency
//! graph from parsed ASTs. It is Phase 3 of the compiler pipeline.
//!
//! ## Responsibilities
//!
//! - **File discovery**: Map module declarations to file paths
//! - **Module correlation**: Associate files with their declared modules
//! - **Import resolution**: Resolve import paths to target modules
//! - **Dependency graph**: Build a directed graph of module dependencies
//! - **Cycle detection**: Detect and report circular imports
//! - **Export collection**: Gather public items per module
//! - **Visibility enforcement**: Verify imports only access public items
//!
//! ## Quick Start
//!
//! ```ignore
//! use spark_modules::{resolve, ModuleResolverInput};
//! use std::collections::HashMap;
//! use std::path::PathBuf;
//!
//! let input = ModuleResolverInput {
//!     asts: parsed_files,
//!     root_path: PathBuf::from("./project"),
//!     file_paths: file_path_map,
//! };
//!
//! let output = resolve(input);
//!
//! if output.diagnostics.has_errors() {
//!     // Handle errors
//! } else {
//!     // Use output.graph for name resolution phase
//!     for module_path in &output.graph.compilation_order {
//!         // Process modules in dependency order
//!     }
//! }
//! ```
//!
//! ## Module System Overview
//!
//! Spark uses a file-based module system:
//!
//! - `module user` - Declares a new module (file must be `user/user.spark`)
//! - `using user` - Joins an existing module from the same directory
//! - `import user { User }` - Imports specific items from a module
//! - `pub import` - Re-exports imported items
//! - `pub` prefix - Makes items visible outside the module
//!
//! ## Relationship to Other Crates
//!
//! ```text
//! spark_parser (produces AST)
//!       │
//!       ▼
//! spark_modules (builds module graph) ◄── THIS CRATE
//!       │
//!       ▼
//! spark_resolve (resolves names)
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod discovery;
mod exports;
mod graph;
mod path;

pub use discovery::ModuleDiscovery;
pub use exports::collect_exports;
pub use graph::{CycleDetector, ModuleGraph, ModuleInfo, ReExport};
pub use path::ModulePath;

use std::collections::HashMap;
use std::path::PathBuf;

use rustc_hash::FxHashMap;
use spark_ast::Ast;
use spark_diagnostics::Diagnostics;
use spark_intern::Symbol;
use spark_span::{FileId, Span};

/// Input to the module resolver.
#[derive(Debug)]
pub struct ModuleResolverInput {
    /// All parsed ASTs indexed by file ID.
    pub asts: HashMap<FileId, Ast>,
    /// The project root directory.
    pub root_path: PathBuf,
    /// Mapping from `FileId` to file path (relative to root).
    pub file_paths: HashMap<FileId, PathBuf>,
}

/// Output from the module resolver.
#[derive(Debug)]
pub struct ModuleResolverOutput {
    /// The constructed module graph.
    pub graph: ModuleGraph,
    /// Diagnostics accumulated during resolution.
    pub diagnostics: Diagnostics,
}

/// Resolves modules from parsed ASTs.
///
/// This is the main entry point for module resolution. It takes parsed ASTs
/// and produces a module dependency graph that can be used by the name
/// resolution phase.
///
/// # Example
///
/// ```ignore
/// use spark_modules::{resolve, ModuleResolverInput};
/// use std::collections::HashMap;
/// use std::path::PathBuf;
///
/// let input = ModuleResolverInput {
///     asts: parsed_files,
///     root_path: PathBuf::from("./project"),
///     file_paths: file_path_map,
/// };
///
/// let output = resolve(input);
/// assert!(!output.diagnostics.has_errors());
/// ```
#[must_use]
pub fn resolve(input: ModuleResolverInput) -> ModuleResolverOutput {
    let mut resolver = ModuleResolver::new(input);
    resolver.resolve();

    ModuleResolverOutput {
        graph: resolver.graph,
        diagnostics: resolver.diagnostics,
    }
}

/// The internal module resolver state.
struct ModuleResolver {
    /// The input ASTs.
    asts: HashMap<FileId, Ast>,
    /// The project root path.
    root_path: PathBuf,
    /// Mapping from `FileId` to file path.
    file_paths: HashMap<FileId, PathBuf>,
    /// The module graph being constructed.
    graph: ModuleGraph,
    /// Mapping from `FileId` to its module path.
    file_to_module: FxHashMap<FileId, ModulePath>,
    /// Accumulated diagnostics.
    diagnostics: Diagnostics,
}

impl ModuleResolver {
    /// Creates a new module resolver.
    fn new(input: ModuleResolverInput) -> Self {
        Self {
            asts: input.asts,
            root_path: input.root_path,
            file_paths: input.file_paths,
            graph: ModuleGraph::default(),
            file_to_module: FxHashMap::default(),
            diagnostics: Diagnostics::new(),
        }
    }

    /// Runs the full resolution process.
    fn resolve(&mut self) {
        // Phase 1: Discover modules from ASTs
        self.discover_modules();

        // Phase 2: Process `using` declarations
        self.process_using_decls();

        // Phase 3: Resolve imports and build dependency edges
        self.resolve_imports();

        // Phase 4: Collect exports from each module
        self.collect_exports();

        // Phase 5: Detect cycles
        self.detect_cycles();

        // Phase 6: Compute compilation order
        self.compute_compilation_order();
    }

    /// Phase 1: Discover module declarations and register modules.
    fn discover_modules(&mut self) {
        let discovery = ModuleDiscovery::new(&self.asts, &self.file_paths, &self.root_path);
        let (modules, file_map, diags) = discovery.discover();

        for (path, info) in modules {
            self.graph.modules.insert(path.clone(), info);
        }

        self.file_to_module = file_map;
        self.diagnostics.extend(diags);
    }

    /// Phase 2: Process `using` declarations.
    fn process_using_decls(&mut self) {
        // Collect using declarations first to avoid borrow issues
        let mut using_infos: Vec<(FileId, ModulePath, Span)> = Vec::new();
        let mut using_errors: Vec<(String, Span, String)> = Vec::new();

        for (file_id, ast) in &self.asts {
            if let Some(ref using_decl) = ast.using_decl {
                let target_path = path_to_module_path(&using_decl.path);

                if self.graph.modules.contains_key(&target_path) {
                    using_infos.push((*file_id, target_path, using_decl.span));
                } else {
                    using_errors.push((
                        format!("cannot join module `{target_path}`: module not found"),
                        using_decl.span,
                        "module not found".to_string(),
                    ));
                }
            }
        }

        // Apply collected data
        for (file_id, target_path, _span) in using_infos {
            if let Some(module_info) = self.graph.modules.get_mut(&target_path) {
                if !module_info.files.contains(&file_id) {
                    module_info.files.push(file_id);
                }
            }
            self.file_to_module.insert(file_id, target_path);
        }

        // Emit errors
        for (msg, span, label) in using_errors {
            self.emit_error(msg, span, label);
        }
    }

    /// Phase 3: Resolve imports and build dependency graph.
    fn resolve_imports(&mut self) {
        // Collect import info first to avoid borrow issues
        let mut import_edges: Vec<(ModulePath, ModulePath, Span)> = Vec::new();
        let mut import_errors: Vec<(String, Span, String)> = Vec::new();
        let mut re_exports: Vec<(ModulePath, Symbol, ReExport)> = Vec::new();

        for (file_id, ast) in &self.asts {
            let source_module = self.file_to_module.get(file_id).cloned();

            for import in &ast.imports {
                let target_path = path_to_module_path(&import.path);

                // Skip stdlib imports — they're resolved by the name resolver,
                // not the filesystem-based module graph.
                if spark_stdlib::StdlibRegistry::is_stdlib_path(target_path.segments()) {
                    continue;
                }

                // Check if target module exists
                if !self.graph.modules.contains_key(&target_path) {
                    import_errors.push((
                        format!("cannot find module `{target_path}`"),
                        import.span,
                        "module not found".to_string(),
                    ));
                    continue;
                }

                // Add dependency edge
                if let Some(ref source) = source_module {
                    import_edges.push((source.clone(), target_path.clone(), import.span));
                }

                // Handle re-exports (pub import)
                if import.is_pub {
                    if let Some(ref source) = source_module {
                        for item in &import.items {
                            let export_name =
                                item.alias.as_ref().map_or(item.name.name, |a| a.name);
                            let re_export = ReExport {
                                source_module: target_path.clone(),
                                original_name: item.name.name,
                            };
                            re_exports.push((source.clone(), export_name, re_export));
                        }
                    }
                }
            }
        }

        // Apply collected data
        for (source, target, _span) in import_edges {
            self.graph
                .dependencies
                .entry(source.clone())
                .or_default()
                .push(target.clone());

            self.graph
                .dependents
                .entry(target)
                .or_default()
                .push(source);
        }

        for (msg, span, label) in import_errors {
            self.emit_error(msg, span, label);
        }

        for (module_path, name, re_export) in re_exports {
            if let Some(module_info) = self.graph.modules.get_mut(&module_path) {
                module_info.re_exports.insert(name, re_export);
            }
        }
    }

    /// Phase 4: Collect exports from each module.
    fn collect_exports(&mut self) {
        for (module_path, module_info) in &mut self.graph.modules {
            let mut exports = std::collections::HashSet::new();

            for file_id in &module_info.files {
                if let Some(ast) = self.asts.get(file_id) {
                    for item in &ast.items {
                        if let Some((name, is_pub)) = exports::item_export_info(item) {
                            if is_pub {
                                exports.insert(name);
                            }
                        }
                    }
                }
            }

            // Add re-exports to the export set
            for name in module_info.re_exports.keys() {
                exports.insert(*name);
            }

            module_info.exports = exports;

            // Log for debugging (this won't be in release builds)
            #[cfg(debug_assertions)]
            if !module_info.exports.is_empty() {
                let _ = module_path; // Suppress unused warning
            }
        }
    }

    /// Phase 5: Detect circular dependencies.
    fn detect_cycles(&mut self) {
        let detector = CycleDetector::new(&self.graph);
        let cycles = detector.find_cycles();

        for cycle in cycles {
            let cycle_str = cycle
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join(" -> ");

            // Get span from first module in cycle if available
            let span = cycle
                .first()
                .and_then(|p| self.graph.modules.get(p))
                .and_then(|m| m.decl_span)
                .unwrap_or(Span::DUMMY);

            self.emit_error(
                format!("circular module dependency: {cycle_str}"),
                span,
                "cycle detected",
            );
        }
    }

    /// Phase 6: Compute topological compilation order.
    fn compute_compilation_order(&mut self) {
        self.graph.compilation_order = self.graph.topological_sort();
    }

    /// Emits an error diagnostic.
    fn emit_error(&mut self, message: impl Into<String>, span: Span, label: impl Into<String>) {
        use spark_diagnostics::{Diagnostic, Label};
        self.diagnostics
            .push(Diagnostic::error(message.into()).with_label(Label::primary(span, label.into())));
    }
}

/// Converts an AST Path to a `ModulePath`.
fn path_to_module_path(path: &spark_ast::Path) -> ModulePath {
    ModulePath::new(path.segments.iter().map(|s| s.name).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_intern::sym;

    #[test]
    fn path_to_module_path_single() {
        use spark_ast::{Ident, Path};

        let path = Path::from_ident(Ident::dummy(sym("user")));
        let module_path = path_to_module_path(&path);

        assert_eq!(module_path.len(), 1);
        assert_eq!(module_path.name(), Some(sym("user")));
    }

    #[test]
    fn path_to_module_path_multi() {
        use spark_ast::{Ident, Path};

        let path = Path::new(
            vec![Ident::dummy(sym("user")), Ident::dummy(sym("auth"))],
            Span::DUMMY,
        );
        let module_path = path_to_module_path(&path);

        assert_eq!(module_path.len(), 2);
        assert_eq!(module_path.to_string(), "user.auth");
    }

    #[test]
    fn empty_input() {
        let input = ModuleResolverInput {
            asts: HashMap::new(),
            root_path: PathBuf::from("."),
            file_paths: HashMap::new(),
        };

        let output = resolve(input);

        assert!(!output.diagnostics.has_errors());
        assert!(output.graph.modules.is_empty());
    }
}
