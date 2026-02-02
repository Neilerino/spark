//! Module discovery from parsed ASTs.
//!
//! This module handles the first phase of module resolution: discovering
//! which modules exist based on `module` declarations in the ASTs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;
use spark_ast::Ast;
use spark_diagnostics::{Diagnostic, Diagnostics, Label};
use spark_span::{FileId, Span};

use crate::{ModuleInfo, ModulePath};

/// Discovers modules from a set of parsed ASTs.
pub struct ModuleDiscovery<'a> {
    /// The ASTs to analyze.
    asts: &'a HashMap<FileId, Ast>,
    /// Mapping from `FileId` to file path.
    file_paths: &'a HashMap<FileId, PathBuf>,
    /// The project root path.
    root_path: &'a Path,
    /// Accumulated diagnostics.
    diagnostics: Diagnostics,
}

impl<'a> ModuleDiscovery<'a> {
    /// Creates a new module discovery instance.
    #[must_use] 
    pub fn new(
        asts: &'a HashMap<FileId, Ast>,
        file_paths: &'a HashMap<FileId, PathBuf>,
        root_path: &'a Path,
    ) -> Self {
        Self {
            asts,
            file_paths,
            root_path,
            diagnostics: Diagnostics::new(),
        }
    }

    /// Discovers all modules and returns the module map and file-to-module mapping.
    ///
    /// Returns:
    /// - `HashMap<ModulePath, ModuleInfo>`: All discovered modules
    /// - `FxHashMap<FileId, ModulePath>`: Mapping from file to its module
    /// - `Diagnostics`: Any errors encountered
    #[must_use]
    pub fn discover(
        mut self,
    ) -> (
        HashMap<ModulePath, ModuleInfo>,
        FxHashMap<FileId, ModulePath>,
        Diagnostics,
    ) {
        let mut modules: HashMap<ModulePath, ModuleInfo> = HashMap::new();
        let mut file_to_module: FxHashMap<FileId, ModulePath> = FxHashMap::default();

        // First pass: process `module` declarations
        for (file_id, ast) in self.asts {
            if let Some(ref module_decl) = ast.module_decl {
                let module_name = module_decl.name.name;
                let module_path = self.compute_module_path(*file_id, module_name);

                // Check for duplicate module declarations
                if let Some(existing) = modules.get(&module_path) {
                    self.emit_error(
                        format!(
                            "module `{module_path}` is already declared in another file"
                        ),
                        module_decl.span,
                        "duplicate module declaration",
                        existing.decl_span,
                    );
                    continue;
                }

                // Validate file path matches module name
                if let Some(file_path) = self.file_paths.get(file_id) {
                    self.validate_module_path(*file_id, file_path, module_name, module_decl.span);
                }

                // Register the module
                let info = ModuleInfo::with_file(module_path.clone(), *file_id, module_decl.span);
                modules.insert(module_path.clone(), info);
                file_to_module.insert(*file_id, module_path);
            }
        }

        // Second pass: handle files without explicit module declarations
        // These get an implicit module based on their file path
        for (file_id, ast) in self.asts {
            if ast.module_decl.is_none() && ast.using_decl.is_none() {
                // No module or using declaration - create implicit module from file name
                if let Some(file_path) = self.file_paths.get(file_id) {
                    let module_path = self.path_to_module_path(file_path);

                    if !modules.contains_key(&module_path) {
                        let info = ModuleInfo::new(module_path.clone());
                        modules.insert(module_path.clone(), info);
                    }

                    // Add file to module
                    if let Some(info) = modules.get_mut(&module_path) {
                        if !info.files.contains(file_id) {
                            info.files.push(*file_id);
                        }
                    }

                    file_to_module.insert(*file_id, module_path);
                }
            }
        }

        (modules, file_to_module, self.diagnostics)
    }

    /// Computes the module path for a file with a module declaration.
    ///
    /// The module path depends on the file's location relative to the root.
    /// For example, `user/auth/auth.spark` with `module auth` becomes `["user", "auth"]`.
    fn compute_module_path(&self, file_id: FileId, module_name: spark_intern::Symbol) -> ModulePath {
        if let Some(file_path) = self.file_paths.get(&file_id) {
            // Get the parent directory path
            let relative = file_path
                .strip_prefix(self.root_path)
                .unwrap_or(file_path);

            let parent = relative.parent();

            if let Some(parent_path) = parent {
                if parent_path.as_os_str().is_empty() {
                    // File is in root directory
                    return ModulePath::from_single(module_name);
                }

                // Build path from parent components plus module name
                let mut segments: Vec<spark_intern::Symbol> = Vec::new();
                for component in parent_path.components() {
                    if let std::path::Component::Normal(s) = component {
                        if let Some(s) = s.to_str() {
                            segments.push(spark_intern::sym(s));
                        }
                    }
                }

                // For nested modules like user/auth/auth.spark,
                // if the file name matches the parent dir, don't duplicate
                let file_stem = file_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(spark_intern::sym);

                if file_stem == Some(module_name) {
                    // This is the "main" file of the module
                    if segments.last() != Some(&module_name) {
                        segments.push(module_name);
                    }
                } else {
                    segments.push(module_name);
                }

                return ModulePath::new(segments);
            }
        }

        // Fallback to just the module name
        ModulePath::from_single(module_name)
    }

    /// Converts a file path to an implicit module path.
    fn path_to_module_path(&self, file_path: &Path) -> ModulePath {
        let relative = file_path
            .strip_prefix(self.root_path)
            .unwrap_or(file_path);

        let mut segments: Vec<spark_intern::Symbol> = Vec::new();

        // Add parent directory components
        if let Some(parent) = relative.parent() {
            for component in parent.components() {
                if let std::path::Component::Normal(s) = component {
                    if let Some(s) = s.to_str() {
                        segments.push(spark_intern::sym(s));
                    }
                }
            }
        }

        // Add the file stem as the final segment
        if let Some(stem) = relative.file_stem().and_then(|s| s.to_str()) {
            segments.push(spark_intern::sym(stem));
        }

        ModulePath::new(segments)
    }

    /// Validates that the file path matches the declared module name.
    fn validate_module_path(
        &mut self,
        _file_id: FileId,
        file_path: &Path,
        module_name: spark_intern::Symbol,
        span: Span,
    ) {
        let file_stem = file_path.file_stem().and_then(|s| s.to_str());

        if let Some(stem) = file_stem {
            if stem != module_name.as_str() {
                self.diagnostics.push(
                    Diagnostic::warning(format!(
                        "module `{}` is declared in file `{}`",
                        module_name.as_str(),
                        file_path.display()
                    ))
                    .with_label(Label::primary(
                        span,
                        format!("expected file named `{}.spark`", module_name.as_str()),
                    ))
                    .with_help("Consider renaming the file to match the module name"),
                );
            }
        }
    }

    /// Emits an error diagnostic with an optional secondary location.
    fn emit_error(
        &mut self,
        message: impl Into<String>,
        span: Span,
        label: impl Into<String>,
        secondary_span: Option<Span>,
    ) {
        let mut diag = Diagnostic::error(message.into()).with_label(Label::primary(span, label));

        if let Some(sec_span) = secondary_span {
            diag = diag.with_label(Label::secondary(sec_span, "previously declared here"));
        }

        self.diagnostics.push(diag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_ast::{Ident, ModuleDecl};
    use spark_intern::sym;

    fn make_ast(file_id: FileId, module_name: Option<&str>) -> Ast {
        let mut ast = Ast::new(file_id);
        if let Some(name) = module_name {
            ast.module_decl = Some(ModuleDecl::new(Ident::dummy(sym(name)), Span::DUMMY));
        }
        ast
    }

    #[test]
    fn discover_single_module() {
        let file_id = FileId::new(0);
        let mut asts = HashMap::new();
        asts.insert(file_id, make_ast(file_id, Some("user")));

        let mut file_paths = HashMap::new();
        file_paths.insert(file_id, PathBuf::from("user/user.spark"));

        let root = PathBuf::from("");
        let discovery = ModuleDiscovery::new(&asts, &file_paths, &root);
        let (modules, file_map, diags) = discovery.discover();

        assert!(!diags.has_errors());
        assert_eq!(modules.len(), 1);
        assert!(file_map.contains_key(&file_id));
    }

    #[test]
    fn discover_nested_module() {
        let file_id = FileId::new(0);
        let mut asts = HashMap::new();
        asts.insert(file_id, make_ast(file_id, Some("auth")));

        let mut file_paths = HashMap::new();
        file_paths.insert(file_id, PathBuf::from("user/auth/auth.spark"));

        let root = PathBuf::from("");
        let discovery = ModuleDiscovery::new(&asts, &file_paths, &root);
        let (modules, file_map, _diags) = discovery.discover();

        let module_path = file_map.get(&file_id).unwrap();
        assert_eq!(module_path.to_string(), "user.auth");
        assert!(modules.contains_key(module_path));
    }

    #[test]
    fn discover_implicit_module() {
        let file_id = FileId::new(0);
        let mut asts = HashMap::new();
        asts.insert(file_id, make_ast(file_id, None));

        let mut file_paths = HashMap::new();
        file_paths.insert(file_id, PathBuf::from("utils/helpers.spark"));

        let root = PathBuf::from("");
        let discovery = ModuleDiscovery::new(&asts, &file_paths, &root);
        let (modules, file_map, _diags) = discovery.discover();

        let module_path = file_map.get(&file_id).unwrap();
        assert_eq!(module_path.to_string(), "utils.helpers");
        assert!(modules.contains_key(module_path));
    }

    #[test]
    fn duplicate_module_error() {
        let file1 = FileId::new(0);
        let file2 = FileId::new(1);

        let mut asts = HashMap::new();
        asts.insert(file1, make_ast(file1, Some("user")));
        asts.insert(file2, make_ast(file2, Some("user")));

        // Both files in root directory declaring the same module "user"
        // This should result in both trying to claim ModulePath(["user"])
        let mut file_paths = HashMap::new();
        file_paths.insert(file1, PathBuf::from("user.spark"));
        file_paths.insert(file2, PathBuf::from("user_alt.spark"));

        let root = PathBuf::from("");
        let discovery = ModuleDiscovery::new(&asts, &file_paths, &root);
        let (_modules, _file_map, diags) = discovery.discover();

        assert!(diags.has_errors());
    }
}
