//! Module dependency graph and cycle detection.
//!
//! This module provides the core data structures for representing module
//! dependencies and algorithms for detecting cycles and computing
//! compilation order.

use std::collections::{HashMap, HashSet};

use rustc_hash::{FxHashMap, FxHashSet};
use spark_intern::Symbol;
use spark_span::{FileId, Span};

use crate::ModulePath;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// The resolved module dependency graph.
///
/// This is the primary output of the module resolution phase. It contains
/// information about all modules in the project and their dependencies.
#[derive(Debug, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ModuleGraph {
    /// All modules indexed by their path.
    pub modules: HashMap<ModulePath, ModuleInfo>,
    /// Module dependency edges (module -> modules it imports).
    pub dependencies: HashMap<ModulePath, Vec<ModulePath>>,
    /// Reverse dependencies (module -> modules that import it).
    pub dependents: HashMap<ModulePath, Vec<ModulePath>>,
    /// Topologically sorted module order for compilation.
    pub compilation_order: Vec<ModulePath>,
}

impl ModuleGraph {
    /// Creates a new empty module graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the module at the given path, if it exists.
    #[must_use]
    pub fn get_module(&self, path: &ModulePath) -> Option<&ModuleInfo> {
        self.modules.get(path)
    }

    /// Returns a mutable reference to the module at the given path.
    #[must_use]
    pub fn get_module_mut(&mut self, path: &ModulePath) -> Option<&mut ModuleInfo> {
        self.modules.get_mut(path)
    }

    /// Returns all modules that the given module depends on (imports from).
    #[must_use]
    pub fn get_dependencies(&self, path: &ModulePath) -> &[ModulePath] {
        self.dependencies
            .get(path)
            .map_or(&[], std::vec::Vec::as_slice)
    }

    /// Returns all modules that depend on (import from) the given module.
    #[must_use]
    pub fn get_dependents(&self, path: &ModulePath) -> &[ModulePath] {
        self.dependents
            .get(path)
            .map_or(&[], std::vec::Vec::as_slice)
    }

    /// Returns modules in topological order for compilation.
    #[must_use]
    pub fn compilation_order(&self) -> &[ModulePath] {
        &self.compilation_order
    }

    /// Checks if an item is exported from a module.
    #[must_use]
    pub fn is_exported(&self, module: &ModulePath, item: Symbol) -> bool {
        self.modules
            .get(module)
            .is_some_and(|m| m.exports.contains(&item))
    }

    /// Resolves a re-export to its original source.
    ///
    /// Returns the source module path and original item name if this is a
    /// re-export, or None if it's a direct export.
    #[must_use]
    pub fn resolve_re_export(
        &self,
        module: &ModulePath,
        item: Symbol,
    ) -> Option<(ModulePath, Symbol)> {
        self.modules.get(module).and_then(|m| {
            m.re_exports
                .get(&item)
                .map(|r| (r.source_module.clone(), r.original_name))
        })
    }

    /// Computes a topological sort of the modules.
    ///
    /// Returns modules ordered such that a module appears before any modules
    /// that depend on it. If there are cycles, the order may not be valid.
    #[must_use]
    pub fn topological_sort(&self) -> Vec<ModulePath> {
        let mut result = Vec::new();
        let mut visited = FxHashSet::default();
        let mut temp_visited = FxHashSet::default();

        for module_path in self.modules.keys() {
            if !visited.contains(module_path) {
                self.topo_visit(module_path, &mut visited, &mut temp_visited, &mut result);
            }
        }

        // The DFS already produces the correct order:
        // dependencies are visited and added before their dependents
        result
    }

    /// Helper for topological sort (DFS).
    fn topo_visit(
        &self,
        node: &ModulePath,
        visited: &mut FxHashSet<ModulePath>,
        temp_visited: &mut FxHashSet<ModulePath>,
        result: &mut Vec<ModulePath>,
    ) {
        if temp_visited.contains(node) {
            // Cycle detected - skip to avoid infinite loop
            return;
        }
        if visited.contains(node) {
            return;
        }

        temp_visited.insert(node.clone());

        if let Some(deps) = self.dependencies.get(node) {
            for dep in deps {
                self.topo_visit(dep, visited, temp_visited, result);
            }
        }

        temp_visited.remove(node);
        visited.insert(node.clone());
        result.push(node.clone());
    }
}

/// Information about a single module.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ModuleInfo {
    /// The module's unique path.
    pub path: ModulePath,
    /// Files that belong to this module (module decl + using decls).
    pub files: Vec<FileId>,
    /// Public exports from this module.
    pub exports: HashSet<Symbol>,
    /// Re-exports from other modules (`pub import`).
    pub re_exports: FxHashMap<Symbol, ReExport>,
    /// The span of the module declaration (for error reporting).
    pub decl_span: Option<Span>,
}

impl ModuleInfo {
    /// Creates a new module info with the given path.
    #[must_use]
    pub fn new(path: ModulePath) -> Self {
        Self {
            path,
            files: Vec::new(),
            exports: HashSet::new(),
            re_exports: FxHashMap::default(),
            decl_span: None,
        }
    }

    /// Creates a module info with a file and span.
    #[must_use]
    pub fn with_file(path: ModulePath, file_id: FileId, span: Span) -> Self {
        Self {
            path,
            files: vec![file_id],
            exports: HashSet::new(),
            re_exports: FxHashMap::default(),
            decl_span: Some(span),
        }
    }

    /// Returns true if this module has any public exports.
    #[must_use]
    pub fn has_exports(&self) -> bool {
        !self.exports.is_empty()
    }

    /// Returns true if the given item is exported from this module.
    #[must_use]
    pub fn exports_item(&self, name: Symbol) -> bool {
        self.exports.contains(&name)
    }
}

/// A re-exported item from another module.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ReExport {
    /// The source module path.
    pub source_module: ModulePath,
    /// The original name in the source module.
    pub original_name: Symbol,
}

/// Detects cycles in the module dependency graph using Tarjan's algorithm.
pub struct CycleDetector<'a> {
    graph: &'a ModuleGraph,
    index: usize,
    stack: Vec<ModulePath>,
    on_stack: FxHashSet<ModulePath>,
    indices: FxHashMap<ModulePath, usize>,
    low_links: FxHashMap<ModulePath, usize>,
    sccs: Vec<Vec<ModulePath>>,
}

impl<'a> CycleDetector<'a> {
    /// Creates a new cycle detector for the given graph.
    #[must_use]
    pub fn new(graph: &'a ModuleGraph) -> Self {
        Self {
            graph,
            index: 0,
            stack: Vec::new(),
            on_stack: FxHashSet::default(),
            indices: FxHashMap::default(),
            low_links: FxHashMap::default(),
            sccs: Vec::new(),
        }
    }

    /// Finds all strongly connected components with more than one node (cycles).
    #[must_use]
    pub fn find_cycles(mut self) -> Vec<Vec<ModulePath>> {
        for module_path in self.graph.modules.keys() {
            if !self.indices.contains_key(module_path) {
                self.strong_connect(module_path.clone());
            }
        }

        // Return only SCCs with more than one node (actual cycles)
        self.sccs
            .into_iter()
            .filter(|scc| scc.len() > 1)
            .collect()
    }

    /// Tarjan's strongly connected components algorithm.
    #[allow(clippy::needless_pass_by_value)]
    fn strong_connect(&mut self, v: ModulePath) {
        self.indices.insert(v.clone(), self.index);
        self.low_links.insert(v.clone(), self.index);
        self.index += 1;
        self.stack.push(v.clone());
        self.on_stack.insert(v.clone());

        if let Some(deps) = self.graph.dependencies.get(&v) {
            for w in deps.clone() {
                if !self.indices.contains_key(&w) {
                    self.strong_connect(w.clone());
                    let low_w = self.low_links[&w];
                    let low_v = self.low_links[&v];
                    self.low_links.insert(v.clone(), low_v.min(low_w));
                } else if self.on_stack.contains(&w) {
                    let idx_w = self.indices[&w];
                    let low_v = self.low_links[&v];
                    self.low_links.insert(v.clone(), low_v.min(idx_w));
                }
            }
        }

        // If v is a root node, pop the stack and generate an SCC
        if self.low_links[&v] == self.indices[&v] {
            let mut scc = Vec::new();
            loop {
                let w = self.stack.pop().expect("stack should not be empty");
                self.on_stack.remove(&w);
                scc.push(w.clone());
                if w == v {
                    break;
                }
            }
            self.sccs.push(scc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_intern::sym;

    fn make_path(s: &str) -> ModulePath {
        ModulePath::from_single(sym(s))
    }

    #[test]
    fn empty_graph() {
        let graph = ModuleGraph::new();
        assert!(graph.modules.is_empty());
        assert!(graph.dependencies.is_empty());
        assert!(graph.compilation_order.is_empty());
    }

    #[test]
    fn single_module() {
        let mut graph = ModuleGraph::new();
        let path = make_path("main");
        graph.modules.insert(path.clone(), ModuleInfo::new(path.clone()));

        let order = graph.topological_sort();
        assert_eq!(order.len(), 1);
        assert_eq!(order[0], path);
    }

    #[test]
    fn linear_dependencies() {
        let mut graph = ModuleGraph::new();

        let a = make_path("a");
        let b = make_path("b");
        let c = make_path("c");

        // a -> b -> c (a depends on b, b depends on c)
        graph.modules.insert(a.clone(), ModuleInfo::new(a.clone()));
        graph.modules.insert(b.clone(), ModuleInfo::new(b.clone()));
        graph.modules.insert(c.clone(), ModuleInfo::new(c.clone()));

        graph.dependencies.insert(a.clone(), vec![b.clone()]);
        graph.dependencies.insert(b.clone(), vec![c.clone()]);

        let order = graph.topological_sort();
        assert_eq!(order.len(), 3);

        // c should come before b, b before a
        let pos_a = order.iter().position(|p| p == &a).unwrap();
        let pos_b = order.iter().position(|p| p == &b).unwrap();
        let pos_c = order.iter().position(|p| p == &c).unwrap();

        assert!(pos_c < pos_b);
        assert!(pos_b < pos_a);
    }

    #[test]
    fn cycle_detection_no_cycle() {
        let mut graph = ModuleGraph::new();

        let a = make_path("a");
        let b = make_path("b");

        graph.modules.insert(a.clone(), ModuleInfo::new(a.clone()));
        graph.modules.insert(b.clone(), ModuleInfo::new(b.clone()));
        graph.dependencies.insert(a.clone(), vec![b.clone()]);

        let detector = CycleDetector::new(&graph);
        let cycles = detector.find_cycles();

        assert!(cycles.is_empty());
    }

    #[test]
    fn cycle_detection_simple_cycle() {
        let mut graph = ModuleGraph::new();

        let a = make_path("a");
        let b = make_path("b");

        // a -> b -> a (cycle)
        graph.modules.insert(a.clone(), ModuleInfo::new(a.clone()));
        graph.modules.insert(b.clone(), ModuleInfo::new(b.clone()));
        graph.dependencies.insert(a.clone(), vec![b.clone()]);
        graph.dependencies.insert(b.clone(), vec![a.clone()]);

        let detector = CycleDetector::new(&graph);
        let cycles = detector.find_cycles();

        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].len(), 2);
    }

    #[test]
    fn cycle_detection_triangle() {
        let mut graph = ModuleGraph::new();

        let a = make_path("a");
        let b = make_path("b");
        let c = make_path("c");

        // a -> b -> c -> a (cycle of 3)
        graph.modules.insert(a.clone(), ModuleInfo::new(a.clone()));
        graph.modules.insert(b.clone(), ModuleInfo::new(b.clone()));
        graph.modules.insert(c.clone(), ModuleInfo::new(c.clone()));
        graph.dependencies.insert(a.clone(), vec![b.clone()]);
        graph.dependencies.insert(b.clone(), vec![c.clone()]);
        graph.dependencies.insert(c.clone(), vec![a.clone()]);

        let detector = CycleDetector::new(&graph);
        let cycles = detector.find_cycles();

        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].len(), 3);
    }

    #[test]
    fn module_info_exports() {
        let mut info = ModuleInfo::new(make_path("test"));
        info.exports.insert(sym("User"));
        info.exports.insert(sym("create"));

        assert!(info.has_exports());
        assert!(info.exports_item(sym("User")));
        assert!(info.exports_item(sym("create")));
        assert!(!info.exports_item(sym("private")));
    }

    #[test]
    fn graph_exports() {
        let mut graph = ModuleGraph::new();
        let path = make_path("user");

        let mut info = ModuleInfo::new(path.clone());
        info.exports.insert(sym("User"));
        graph.modules.insert(path.clone(), info);

        assert!(graph.is_exported(&path, sym("User")));
        assert!(!graph.is_exported(&path, sym("Other")));
    }

    #[test]
    fn graph_re_exports() {
        let mut graph = ModuleGraph::new();

        let user = make_path("user");
        let api = make_path("api");

        let mut user_info = ModuleInfo::new(user.clone());
        user_info.exports.insert(sym("User"));
        graph.modules.insert(user.clone(), user_info);

        let mut api_info = ModuleInfo::new(api.clone());
        api_info.re_exports.insert(
            sym("User"),
            ReExport {
                source_module: user.clone(),
                original_name: sym("User"),
            },
        );
        graph.modules.insert(api.clone(), api_info);

        let resolved = graph.resolve_re_export(&api, sym("User"));
        assert!(resolved.is_some());
        let (source, name) = resolved.unwrap();
        assert_eq!(source, user);
        assert_eq!(name, sym("User"));
    }
}
