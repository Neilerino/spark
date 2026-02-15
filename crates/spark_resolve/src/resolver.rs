//! Main resolver implementation.
//!
//! This module contains the `Resolver` struct that orchestrates the
//! two-pass name resolution algorithm:
//!
//! 1. **Collection pass**: Collect all top-level definitions (enables forward refs)
//! 2. **Resolution pass**: Resolve all identifier/path references in bodies

use std::collections::HashMap;

use rustc_hash::FxHashMap;
use spark_ast::{
    Ast, BindingDef, Block, Expr, ExprKind, FunctionDef, GenericConstraint, Item, Pattern,
    PatternKind, Stmt, StmtKind, ToHandler, TypeExpr, TypeExprKind,
};
use spark_diagnostics::{Diagnostic, Diagnostics, Label};
use spark_intern::{kw, Symbol};
use spark_modules::{ModuleGraph, ModulePath};
use spark_span::{FileId, Span};

use crate::def::{DefId, DefinitionInfo, DefinitionKind, DefinitionTable, Visibility};
use crate::resolved_ast::ResolvedAst;
use crate::rib::{RibKind, RibStack};
use crate::{ResolverInput, ResolverOutput};

/// The main resolver struct.
///
/// Orchestrates the two-pass resolution algorithm:
/// 1. Collection pass: collect all top-level definitions
/// 2. Resolution pass: resolve all references within bodies
pub struct Resolver {
    /// The module dependency graph.
    graph: ModuleGraph,
    /// All parsed ASTs.
    asts: HashMap<FileId, Ast>,
    /// Table of all definitions.
    definitions: DefinitionTable,
    /// The resolved AST being built.
    resolved: ResolvedAst,
    /// Rib stack for lexical scoping.
    rib_stack: RibStack,
    /// Accumulated diagnostics.
    diagnostics: Diagnostics,
    /// The current file being resolved (for recording resolutions).
    current_file: FileId,
    /// Per-module collected `DefId`s (`module_path` -> [(name, `def_id`, kind)]).
    /// Used to repopulate ribs during the resolution pass.
    module_defs: FxHashMap<ModulePath, Vec<(Symbol, DefId, DefinitionKind)>>,
    /// Per-file import `DefId`s (`file_id` -> [(`local_name`, `def_id`, kind, span)]).
    file_imports: FxHashMap<FileId, Vec<(Symbol, DefId, DefinitionKind, Span)>>,
    /// Built-in function definitions to inject into every module rib.
    builtin_defs: Vec<(Symbol, DefId, DefinitionKind)>,
}

impl Resolver {
    /// Creates a new resolver from the input.
    pub fn new(input: ResolverInput) -> Self {
        let resolved = ResolvedAst::new(input.asts.clone());
        Self {
            graph: input.graph,
            asts: input.asts,
            definitions: DefinitionTable::new(),
            resolved,
            rib_stack: RibStack::new(),
            diagnostics: Diagnostics::new(),
            current_file: FileId::new(0),
            module_defs: FxHashMap::default(),
            file_imports: FxHashMap::default(),
            builtin_defs: Vec::new(),
        }
    }

    /// Runs the full resolution process.
    pub fn resolve(&mut self) {
        // Register built-in functions (e.g., print)
        self.register_builtins();

        // Pass 1: Collect all top-level definitions
        self.collect_definitions();

        // Pass 2: Resolve all references
        self.resolve_references();
    }

    /// Registers built-in functions as definitions in a prelude rib.
    ///
    /// The prelude rib sits below all module ribs, so user definitions
    /// can shadow built-in names.
    fn register_builtins(&mut self) {
        // Register `print` as a built-in function
        let info = DefinitionInfo::new(
            DefinitionKind::BuiltinFunction,
            kw::PRINT,
            Span::DUMMY,
            Visibility::Public,
            ModulePath::root(),
        );
        let def_id = self.definitions.insert(info);

        // Store the builtin DefId so it can be added to ribs during resolution
        self.builtin_defs.push((kw::PRINT, def_id, DefinitionKind::BuiltinFunction));

        // Register sort/take/reverse as built-in functions (callable via `via` or directly)
        for &fn_name in &[kw::SORT, kw::TAKE, kw::REVERSE] {
            let info = DefinitionInfo::new(
                DefinitionKind::BuiltinFunction,
                fn_name,
                Span::DUMMY,
                Visibility::Public,
                ModulePath::root(),
            );
            let fn_def_id = self.definitions.insert(info);
            self.builtin_defs.push((fn_name, fn_def_id, DefinitionKind::BuiltinFunction));
        }

        // Register built-in methods (len, append, contains, remove, add, send, recv, tryRecv, close).
        // These are NOT added to ribs (can't be called as free functions),
        // but need DefIds so MIR and codegen can reference them.
        for &method_name in &[
            kw::LEN, kw::APPEND, kw::CONTAINS, kw::REMOVE, kw::ADD,
            kw::SEND, kw::RECV, kw::TRY_RECV, kw::CLOSE,
        ] {
            let info = DefinitionInfo::new(
                DefinitionKind::BuiltinMethod,
                method_name,
                Span::DUMMY,
                Visibility::Public,
                ModulePath::root(),
            );
            self.definitions.insert(info);
        }

        // Register internal parallel runtime stubs.
        // NOT added to ribs — only called from desugared HIR/MIR.
        for &stub_name in &[kw::PARALLEL_FOREACH, kw::PARALLEL_MAP] {
            let info = DefinitionInfo::new(
                DefinitionKind::BuiltinFunction,
                stub_name,
                Span::DUMMY,
                Visibility::Public,
                ModulePath::root(),
            );
            self.definitions.insert(info);
        }

        // Register internal spawn/await runtime stubs.
        // NOT added to ribs — only called from desugared HIR/MIR.
        for &stub_name in &[kw::SPARK_SPAWN, kw::SPARK_AWAIT, kw::TRY_POLL] {
            let info = DefinitionInfo::new(
                DefinitionKind::BuiltinFunction,
                stub_name,
                Span::DUMMY,
                Visibility::Public,
                ModulePath::root(),
            );
            self.definitions.insert(info);
        }
    }

    // ========================================================================
    // Pass 1: Collection
    // ========================================================================

    /// Pass 1: Collect all top-level definitions.
    fn collect_definitions(&mut self) {
        let compilation_order = self.graph.compilation_order.clone();

        let mut files_to_process = Vec::new();
        for module_path in &compilation_order {
            if let Some(module_info) = self.graph.modules.get(module_path) {
                for &file_id in &module_info.files {
                    files_to_process.push((file_id, module_path.clone()));
                }
            }
        }

        let asts = std::mem::take(&mut self.asts);

        for (file_id, module_path) in files_to_process {
            if let Some(ast) = asts.get(&file_id) {
                self.collect_file_definitions(file_id, ast, &module_path);
            }
        }

        self.asts = asts;
    }

    /// Collects definitions from a single file.
    fn collect_file_definitions(&mut self, file_id: FileId, ast: &Ast, module_path: &ModulePath) {
        for item in &ast.items {
            let (name, kind, visibility) = match item {
                Item::Function(f) => (
                    f.name.name,
                    if f.is_extern { DefinitionKind::ExternFunction } else { DefinitionKind::Function },
                    Visibility::from(f.visibility),
                ),
                Item::Struct(s) => (
                    s.name.name,
                    DefinitionKind::Struct,
                    Visibility::from(s.visibility),
                ),
                Item::Enum(e) => (
                    e.name.name,
                    DefinitionKind::Enum,
                    Visibility::from(e.visibility),
                ),
                Item::Interface(i) => (
                    i.name.name,
                    DefinitionKind::Interface,
                    Visibility::from(i.visibility),
                ),
                Item::TypeAlias(t) => (
                    t.name.name,
                    DefinitionKind::TypeAlias,
                    Visibility::from(t.visibility),
                ),
                Item::TypeDef(t) => (
                    t.name.name,
                    DefinitionKind::TypeDef,
                    Visibility::from(t.visibility),
                ),
                Item::Const(c) => (
                    c.name.name,
                    DefinitionKind::Const,
                    Visibility::from(c.visibility),
                ),
                Item::Binding(b) => {
                    self.collect_binding_methods(file_id, b, module_path);
                    continue;
                }
            };

            let span = item.span();

            let info =
                DefinitionInfo::new(kind.clone(), name, span, visibility, module_path.clone());
            let def_id = self.definitions.insert(info);

            // Track for repopulating ribs in pass 2
            self.module_defs
                .entry(module_path.clone())
                .or_default()
                .push((name, def_id, kind.clone()));

            // Record the definition binding in resolved AST
            let name_span = match item {
                Item::Function(f) => f.name.span,
                Item::Struct(s) => s.name.span,
                Item::Enum(e) => e.name.span,
                Item::Interface(i) => i.name.span,
                Item::TypeAlias(t) => t.name.span,
                Item::TypeDef(t) => t.name.span,
                Item::Const(c) => c.name.span,
                Item::Binding(_) => continue,
            };
            self.resolved
                .get_or_create_resolutions(file_id)
                .record_binding(name_span, def_id);

            // Collect struct fields and enum variants
            match item {
                Item::Struct(s) => {
                    self.collect_struct_fields(file_id, s, def_id, module_path);
                }
                Item::Enum(e) => {
                    self.collect_enum_variants(file_id, e, def_id, module_path);
                }
                _ => {}
            }
        }

        // Collect import bindings for this file
        self.collect_import_bindings(file_id, ast, module_path);
    }

    /// Registers struct field definitions.
    fn collect_struct_fields(
        &mut self,
        file_id: FileId,
        s: &spark_ast::StructDef,
        parent_id: DefId,
        module_path: &ModulePath,
    ) {
        for field in &s.fields {
            let info = DefinitionInfo::new(
                DefinitionKind::StructField(parent_id),
                field.name.name,
                field.name.span,
                Visibility::Private,
                module_path.clone(),
            );
            let field_def_id = self.definitions.insert(info);
            self.resolved
                .get_or_create_resolutions(file_id)
                .record_binding(field.name.span, field_def_id);
        }
    }

    /// Registers enum variant definitions.
    fn collect_enum_variants(
        &mut self,
        file_id: FileId,
        e: &spark_ast::EnumDef,
        parent_id: DefId,
        module_path: &ModulePath,
    ) {
        for variant in &e.variants {
            let info = DefinitionInfo::new(
                DefinitionKind::EnumVariant(parent_id),
                variant.name.name,
                variant.name.span,
                Visibility::Private,
                module_path.clone(),
            );
            let variant_def_id = self.definitions.insert(info);
            self.resolved
                .get_or_create_resolutions(file_id)
                .record_binding(variant.name.span, variant_def_id);
        }
    }

    /// Registers definitions for methods in an interface binding.
    ///
    /// For `Foo binds IBar { fn method(...) { ... } }`, allocates `DefId`s
    /// for each method as `BindingMethod(type_def_id)`. Methods are NOT
    /// added to symbol ribs — they are resolved via dot syntax in the type checker.
    fn collect_binding_methods(
        &mut self,
        file_id: FileId,
        binding: &BindingDef,
        module_path: &ModulePath,
    ) {
        // Find the DefId for the bound type by examining the type expression.
        let type_def_id = self.find_type_def_from_type_expr(&binding.ty, module_path);
        if type_def_id.is_dummy() {
            return;
        }

        for target in &binding.bindings {
            for method in &target.methods {
                let info = DefinitionInfo::new(
                    DefinitionKind::BindingMethod(type_def_id),
                    method.name.name,
                    method.name.span,
                    Visibility::Private,
                    module_path.clone(),
                );
                let method_def_id = self.definitions.insert(info);
                self.resolved
                    .get_or_create_resolutions(file_id)
                    .record_binding(method.name.span, method_def_id);

                // Also register parameter DefIds
                for param in &method.params {
                    let param_info = DefinitionInfo::new(
                        DefinitionKind::Parameter,
                        param.name.name,
                        param.name.span,
                        Visibility::Private,
                        module_path.clone(),
                    );
                    let param_def_id = self.definitions.insert(param_info);
                    self.resolved
                        .get_or_create_resolutions(file_id)
                        .record_binding(param.name.span, param_def_id);
                }
            }
        }
    }

    /// Finds the `DefId` for a type referenced in a type expression.
    ///
    /// Used during the collection pass to resolve the bound type in bindings.
    /// Only handles simple named types (e.g., `Foo`), not complex type expressions.
    fn find_type_def_from_type_expr(
        &self,
        ty: &TypeExpr,
        module_path: &ModulePath,
    ) -> DefId {
        if let TypeExprKind::Named { path, .. } = &ty.kind {
            if path.is_simple() {
                let name = path.segments[0].name;
                // Search in module defs already collected
                if let Some(defs) = self.module_defs.get(module_path) {
                    for (def_name, def_id, kind) in defs {
                        if *def_name == name && kind.is_type() {
                            return *def_id;
                        }
                    }
                }
            }
        }
        DefId::DUMMY
    }

    /// Collects import bindings for a file and stores resolved `DefId`s.
    ///
    /// Detects duplicate imports: if the same local name is imported from two
    /// different modules, an `E0252` error is emitted. Re-importing the same
    /// name from the same module is silently ignored (idempotent).
    fn collect_import_bindings(&mut self, file_id: FileId, ast: &Ast, _module_path: &ModulePath) {
        // Track which names have been imported in this file to detect conflicts.
        let mut imported_names: FxHashMap<Symbol, (ModulePath, Span)> = FxHashMap::default();

        for import_decl in &ast.imports {
            let path_segments: Vec<Symbol> =
                import_decl.path.segments.iter().map(|s| s.name).collect();

            let target_module = ModulePath::new(path_segments);

            if import_decl.items.is_empty() {
                // Whole module import — not adding individual items here.
                // Whole-module imports would require namespace-level resolution.
                continue;
            }

            for import_item in &import_decl.items {
                let original_name = import_item.name.name;
                let local_name = import_item.alias.as_ref().map_or(original_name, |a| a.name);

                // Check for duplicate import of the same local name
                if let Some((prev_module, prev_span)) = imported_names.get(&local_name) {
                    if *prev_module != target_module {
                        self.diagnostics.push(
                            Diagnostic::error(format!(
                                "the name `{}` is imported from both `{}` and `{}`",
                                local_name.as_str(),
                                prev_module,
                                target_module
                            ))
                            .with_code("E0252")
                            .with_label(Label::primary(
                                import_item.span,
                                "conflicting import",
                            ))
                            .with_label(Label::secondary(
                                *prev_span,
                                "previously imported here",
                            ))
                            .with_help(format!(
                                "use an alias: `{} as {}_alias`",
                                local_name.as_str(),
                                local_name.as_str()
                            )),
                        );
                    }
                    // Either way (same or different module), skip duplicate
                    continue;
                }

                // Check if the name is exported from the target module
                if !self.graph.is_exported(&target_module, original_name) {
                    self.diagnostics.push(
                        Diagnostic::error(format!(
                            "`{}` is not exported from module `{target_module}`",
                            original_name.as_str()
                        ))
                        .with_code("E0432")
                        .with_label(Label::primary(import_item.span, "not found in module")),
                    );
                    continue;
                }

                // Find the DefId for the imported item by looking through collected defs
                if let Some(def_id) = self.find_exported_def(&target_module, original_name) {
                    let kind = self
                        .definitions
                        .get(def_id)
                        .map_or(DefinitionKind::Function, |info| info.kind.clone());
                    imported_names
                        .insert(local_name, (target_module.clone(), import_item.span));
                    self.file_imports
                        .entry(file_id)
                        .or_default()
                        .push((local_name, def_id, kind, import_item.span));
                }
            }
        }
    }

    /// Finds the `DefId` for an exported name from a module.
    fn find_exported_def(&self, module_path: &ModulePath, name: Symbol) -> Option<DefId> {
        // Look through module defs
        if let Some(defs) = self.module_defs.get(module_path) {
            for (def_name, def_id, _) in defs {
                if *def_name == name {
                    return Some(*def_id);
                }
            }
        }

        // Check re-exports
        if let Some((source_module, original_name)) =
            self.graph.resolve_re_export(module_path, name)
        {
            return self.find_exported_def(&source_module, original_name);
        }

        None
    }

    // ========================================================================
    // Pass 2: Resolution
    // ========================================================================

    /// Pass 2: Resolve all references.
    fn resolve_references(&mut self) {
        let compilation_order = self.graph.compilation_order.clone();

        for module_path in &compilation_order {
            let module_info = self.graph.modules.get(module_path).cloned();
            let Some(module_info) = module_info else {
                continue;
            };

            // Push module rib and populate with collected defs
            self.rib_stack.push_rib(RibKind::Module);

            // Inject built-in functions into the module rib
            for (name, def_id, kind) in &self.builtin_defs {
                if kind.is_value() {
                    self.rib_stack.define_value(*name, *def_id);
                }
            }

            if let Some(defs) = self.module_defs.get(module_path).cloned() {
                for (name, def_id, kind) in &defs {
                    if kind.is_type() {
                        self.rib_stack.define_type(*name, *def_id);
                    }
                    if kind.is_value() {
                        self.rib_stack.define_value(*name, *def_id);
                    }
                }
            }

            let asts = std::mem::take(&mut self.asts);

            for &file_id in &module_info.files {
                // Add imports for this file to the rib
                if let Some(imports) = self.file_imports.get(&file_id).cloned() {
                    for (local_name, def_id, kind, import_span) in &imports {
                        if kind.is_value() {
                            if let Some(prev_id) =
                                self.rib_stack.define_value(*local_name, *def_id)
                            {
                                self.warn_import_shadows_local(
                                    *local_name,
                                    *import_span,
                                    prev_id,
                                    module_path,
                                );
                            }
                        }
                        if kind.is_type() {
                            if let Some(prev_id) =
                                self.rib_stack.define_type(*local_name, *def_id)
                            {
                                // Only warn for type-only imports to avoid
                                // double warnings for names in both namespaces
                                if !kind.is_value() {
                                    self.warn_import_shadows_local(
                                        *local_name,
                                        *import_span,
                                        prev_id,
                                        module_path,
                                    );
                                }
                            }
                        }
                    }
                }

                if let Some(ast) = asts.get(&file_id) {
                    self.current_file = file_id;
                    self.resolve_file(ast);
                }
            }

            self.asts = asts;
            self.rib_stack.pop_rib();
        }
    }

    /// Resolves references in a single file.
    fn resolve_file(&mut self, ast: &Ast) {
        for item in ast.items.clone() {
            self.resolve_item(&item);
        }
    }

    /// Resolves references in a top-level item.
    fn resolve_item(&mut self, item: &Item) {
        match item {
            Item::Function(f) => self.resolve_function(f),
            Item::Struct(s) => self.resolve_struct_body(s),
            Item::Enum(e) => self.resolve_enum_body(e),
            Item::Interface(i) => self.resolve_interface_body(i),
            Item::TypeAlias(t) => {
                self.resolve_generics(&t.generics);
                self.resolve_type_expr(&t.ty);
                self.pop_generics_if_needed(&t.generics);
            }
            Item::TypeDef(t) => {
                self.resolve_generics(&t.generics);
                self.resolve_type_expr(&t.ty);
                self.pop_generics_if_needed(&t.generics);
            }
            Item::Const(c) => {
                self.resolve_type_expr(&c.ty);
                self.resolve_expr(&c.value);
            }
            Item::Binding(b) => self.resolve_binding(b),
        }
    }

    // ========================================================================
    // Function resolution
    // ========================================================================

    /// Resolves a function definition.
    fn resolve_function(&mut self, func: &FunctionDef) {
        // Push generic rib if generics present
        self.resolve_generics(&func.generics);

        // Push parameter rib
        self.rib_stack.push_rib(RibKind::Parameter);

        // Resolve parameters (types first, then define names)
        for param in &func.params {
            self.resolve_type_expr(&param.ty);
            let info = DefinitionInfo::new(
                DefinitionKind::Parameter,
                param.name.name,
                param.name.span,
                Visibility::Private,
                ModulePath::root(),
            );
            let def_id = self.definitions.insert(info);
            self.rib_stack.define_value(param.name.name, def_id);
            self.record_binding(param.name.span, def_id);
        }

        // Resolve return type
        self.resolve_type_expr(&func.ret_type);

        // Resolve body
        if let Some(body) = &func.body {
            self.resolve_block(body);
        }

        // Pop parameter rib
        self.rib_stack.pop_rib();

        // Pop generic rib
        self.pop_generics_if_needed(&func.generics);
    }

    /// Pushes a generic rib and defines generic params if non-empty.
    fn resolve_generics(&mut self, generics: &[spark_ast::GenericParam]) {
        if generics.is_empty() {
            return;
        }

        self.rib_stack.push_type_rib(RibKind::Generic);

        for generic in generics {
            let info = DefinitionInfo::new(
                DefinitionKind::GenericParam,
                generic.name.name,
                generic.name.span,
                Visibility::Private,
                ModulePath::root(),
            );
            let def_id = self.definitions.insert(info);
            self.rib_stack.define_type(generic.name.name, def_id);
            self.record_binding(generic.name.span, def_id);

            // Resolve constraint
            if let Some(constraint) = &generic.constraint {
                match constraint {
                    GenericConstraint::Named(ty) => self.resolve_type_expr(ty),
                    GenericConstraint::Structural(fields) => {
                        for field in fields {
                            self.resolve_type_expr(&field.ty);
                        }
                    }
                }
            }
        }
    }

    /// Pops the generic rib if generics were pushed.
    fn pop_generics_if_needed(&mut self, generics: &[spark_ast::GenericParam]) {
        if !generics.is_empty() {
            self.rib_stack.pop_type_rib();
        }
    }

    // ========================================================================
    // Struct / Enum / Interface body resolution
    // ========================================================================

    /// Resolves type expressions in struct fields.
    fn resolve_struct_body(&mut self, s: &spark_ast::StructDef) {
        self.resolve_generics(&s.generics);
        for field in &s.fields {
            self.resolve_type_expr(&field.ty);
        }
        self.pop_generics_if_needed(&s.generics);
    }

    /// Resolves type expressions in enum variants.
    fn resolve_enum_body(&mut self, e: &spark_ast::EnumDef) {
        self.resolve_generics(&e.generics);
        if let Some(raw_type) = &e.raw_type {
            self.resolve_type_expr(raw_type);
        }
        for variant in &e.variants {
            match &variant.data {
                spark_ast::EnumVariantData::Unit => {}
                spark_ast::EnumVariantData::RawValue(expr) => self.resolve_expr(expr),
                spark_ast::EnumVariantData::Fields(fields) => {
                    for field in fields {
                        self.resolve_type_expr(&field.ty);
                    }
                }
            }
        }
        self.pop_generics_if_needed(&e.generics);
    }

    /// Resolves an interface body.
    fn resolve_interface_body(&mut self, i: &spark_ast::InterfaceDef) {
        self.resolve_generics(&i.generics);
        for ext in &i.extends {
            self.resolve_type_expr(ext);
        }
        for method in &i.methods {
            self.resolve_function(method);
        }
        self.pop_generics_if_needed(&i.generics);
    }

    /// Resolves an interface binding.
    fn resolve_binding(&mut self, b: &spark_ast::BindingDef) {
        self.resolve_type_expr(&b.ty);
        for target in &b.bindings {
            self.resolve_type_expr(&target.interface);
            for method in &target.methods {
                self.resolve_function(method);
            }
        }
    }

    // ========================================================================
    // Block and statement resolution
    // ========================================================================

    /// Resolves a block (pushes a new rib scope).
    fn resolve_block(&mut self, block: &Block) {
        self.rib_stack.push_rib(RibKind::Normal);
        for stmt in &block.stmts {
            self.resolve_stmt(stmt);
        }
        if let Some(expr) = &block.expr {
            self.resolve_expr(expr);
        }
        self.rib_stack.pop_rib();
    }

    /// Resolves a statement.
    fn resolve_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let {
                name,
                ty,
                value,
                mutable: _,
            } => {
                // Resolve type and value first (value can't see the new binding)
                self.resolve_type_expr(ty);
                self.resolve_expr(value);

                // Now define the local
                let info = DefinitionInfo::new(
                    DefinitionKind::Local,
                    name.name,
                    name.span,
                    Visibility::Private,
                    ModulePath::root(),
                );
                let def_id = self.definitions.insert(info);
                self.rib_stack.define_value(name.name, def_id);
                self.record_binding(name.span, def_id);
            }
            StmtKind::Assign { target, value } | StmtKind::CompoundAssign { target, value, .. } => {
                self.resolve_expr(target);
                self.resolve_expr(value);
            }
            StmtKind::Expr(e) => self.resolve_expr(e),
        }
    }

    // ========================================================================
    // Expression resolution
    // ========================================================================

    /// Resolves an expression.
    #[allow(clippy::too_many_lines)]
    fn resolve_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Literal(_) | ExprKind::Error | ExprKind::Continue => {}

            ExprKind::Ident(ident) => {
                if let Some(def_id) = self.rib_stack.lookup_value(ident.name) {
                    self.record_ident(ident.span, def_id);
                } else {
                    self.error_undefined_name(ident.name, ident.span);
                }
            }

            ExprKind::Path(path) => {
                self.resolve_value_path(path);
            }

            ExprKind::List(elems) | ExprKind::Set(elems) | ExprKind::Tuple(elems) => {
                for e in elems {
                    self.resolve_expr(e);
                }
            }

            ExprKind::ListRange {
                start, end, step, ..
            } => {
                self.resolve_expr(start);
                self.resolve_expr(end);
                if let Some(s) = step {
                    self.resolve_expr(s);
                }
            }

            ExprKind::Map(entries) => {
                for entry in entries {
                    self.resolve_expr(&entry.key);
                    self.resolve_expr(&entry.value);
                }
            }

            ExprKind::Binary { lhs, rhs, .. } | ExprKind::Coalesce { lhs, rhs } => {
                self.resolve_expr(lhs);
                self.resolve_expr(rhs);
            }

            ExprKind::Unary { operand, .. } => self.resolve_expr(operand),

            ExprKind::Field { base, .. } | ExprKind::OptionalField { base, .. } => {
                // Resolve the base; field name is resolved by type checker
                self.resolve_expr(base);
            }

            ExprKind::Index { base, index } => {
                self.resolve_expr(base);
                self.resolve_expr(index);
            }

            ExprKind::Call { callee, args } => {
                self.resolve_expr(callee);
                for arg in args {
                    self.resolve_expr(&arg.value);
                }
            }

            ExprKind::MethodCall { receiver, args, .. } => {
                self.resolve_expr(receiver);
                for arg in args {
                    self.resolve_expr(&arg.value);
                }
            }

            ExprKind::StructInit {
                path,
                type_args,
                fields,
                spread,
            } => {
                self.resolve_type_path(path);
                for ta in type_args {
                    self.resolve_type_expr(ta);
                }
                for field in fields {
                    self.resolve_expr(&field.value);
                }
                if let Some(s) = spread {
                    self.resolve_expr(s);
                }
            }

            ExprKind::EnumVariant { path, fields, .. } => {
                self.resolve_type_path(path);
                for f in fields {
                    self.resolve_expr(f);
                }
            }

            ExprKind::Block(block) => self.resolve_block(block),

            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.resolve_expr(condition);
                self.resolve_expr(then_branch);
                if let Some(e) = else_branch {
                    self.resolve_expr(e);
                }
            }

            ExprKind::Match { scrutinee, arms } => {
                self.resolve_expr(scrutinee);
                for arm in arms {
                    // Each arm gets its own scope for pattern bindings
                    self.rib_stack.push_rib(RibKind::Normal);
                    self.resolve_pattern(&arm.pattern);
                    self.resolve_expr(&arm.body);
                    self.rib_stack.pop_rib();
                }
            }

            ExprKind::Loop { body } => self.resolve_expr(body),

            ExprKind::Break(e) | ExprKind::Return(e) => {
                if let Some(e) = e {
                    self.resolve_expr(e);
                }
            }

            ExprKind::Each {
                iterable,
                callback,
            }
            | ExprKind::EachVia {
                iterable,
                callback,
            }
            | ExprKind::ParallelEach {
                iterable,
                callback,
            }
            | ExprKind::ParallelEachVia {
                iterable,
                callback,
            } => {
                self.resolve_expr(iterable);
                self.resolve_expr(callback);
            }

            ExprKind::To { expr, handler } => {
                self.resolve_expr(expr);
                match handler {
                    ToHandler::Narrowing(arms) => {
                        for arm in arms {
                            self.rib_stack.push_rib(RibKind::Normal);
                            self.resolve_pattern(&arm.pattern);
                            self.resolve_expr(&arm.body);
                            self.rib_stack.pop_rib();
                        }
                    }
                    ToHandler::Pipeline { operation, args } => {
                        // Resolve the pipeline operation as a value-namespace identifier
                        if let Some(def_id) = self.rib_stack.lookup_value(operation.name) {
                            self.record_ident(operation.span, def_id);
                        } else if !kw::is_builtin_type(operation.name) {
                            self.diagnostics.push(
                                Diagnostic::error(format!(
                                    "unknown function `{}`",
                                    operation.name.as_str()
                                ))
                                .with_label(Label::primary(operation.span, "not found")),
                            );
                        }
                        for arg in args {
                            self.resolve_expr(&arg.value);
                        }
                    }
                }
            }

            ExprKind::Propagate(e) | ExprKind::Paren(e) => self.resolve_expr(e),

            ExprKind::Is { expr, ty } | ExprKind::As { expr, ty } => {
                self.resolve_expr(expr);
                self.resolve_type_expr(ty);
            }

            ExprKind::Lambda {
                params,
                ret_type,
                body,
            } => {
                self.rib_stack.push_rib(RibKind::Lambda);

                for param in params {
                    self.resolve_type_expr(&param.ty);
                    let info = DefinitionInfo::new(
                        DefinitionKind::Parameter,
                        param.name.name,
                        param.name.span,
                        Visibility::Private,
                        ModulePath::root(),
                    );
                    let def_id = self.definitions.insert(info);
                    self.rib_stack.define_value(param.name.name, def_id);
                    self.record_binding(param.name.span, def_id);
                }

                if let Some(rt) = ret_type {
                    self.resolve_type_expr(rt);
                }

                self.resolve_expr(body);
                self.rib_stack.pop_rib();
            }

            ExprKind::Partial { callee, args } => {
                self.resolve_expr(callee);
                for arg in args {
                    self.resolve_expr(&arg.value);
                }
            }

            ExprKind::Await(e) | ExprKind::BlockAwait(e) | ExprKind::Spawn(e) => {
                self.resolve_expr(e);
            }

            ExprKind::Range { start, end, .. } => {
                self.resolve_expr(start);
                self.resolve_expr(end);
            }
        }
    }

    // ========================================================================
    // Type expression resolution
    // ========================================================================

    /// Resolves a type expression.
    fn resolve_type_expr(&mut self, ty: &TypeExpr) {
        match &ty.kind {
            TypeExprKind::Named { path, type_args } => {
                self.resolve_type_path(path);
                for arg in type_args {
                    self.resolve_type_expr(arg);
                }
            }
            TypeExprKind::Tuple(elems) => {
                for e in elems {
                    self.resolve_type_expr(e);
                }
            }
            TypeExprKind::Function { params, ret } => {
                for p in params {
                    self.resolve_type_expr(p);
                }
                self.resolve_type_expr(ret);
            }
            TypeExprKind::Optional(inner) | TypeExprKind::Mutable(inner) => {
                self.resolve_type_expr(inner);
            }
            TypeExprKind::Union(types) => {
                for t in types {
                    self.resolve_type_expr(t);
                }
            }
            TypeExprKind::Ref { inner, .. } => {
                self.resolve_type_expr(inner);
                // Lifetime idents resolved later (by type checker)
            }
            TypeExprKind::Structural(fields) => {
                for f in fields {
                    self.resolve_type_expr(&f.ty);
                }
            }
            TypeExprKind::Infer | TypeExprKind::Error => {}
        }
    }

    // ========================================================================
    // Pattern resolution
    // ========================================================================

    /// Resolves a pattern, introducing bindings into the current rib.
    fn resolve_pattern(&mut self, pattern: &Pattern) {
        match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Error | PatternKind::Literal(_) => {}

            PatternKind::Ident { name, ty } => {
                // Introduces a new binding
                let info = DefinitionInfo::new(
                    DefinitionKind::Local,
                    name.name,
                    name.span,
                    Visibility::Private,
                    ModulePath::root(),
                );
                let def_id = self.definitions.insert(info);
                self.rib_stack.define_value(name.name, def_id);
                self.record_binding(name.span, def_id);

                if let Some(t) = ty {
                    self.resolve_type_expr(t);
                }
            }

            PatternKind::Variant { path, fields } => {
                self.resolve_type_path(path);
                for f in fields {
                    self.resolve_pattern(f);
                }
            }

            PatternKind::Struct { path, fields, .. } => {
                self.resolve_type_path(path);
                for field in fields {
                    if let Some(p) = &field.pattern {
                        self.resolve_pattern(p);
                    } else {
                        // Shorthand: field name introduces a binding
                        let info = DefinitionInfo::new(
                            DefinitionKind::Local,
                            field.name.name,
                            field.name.span,
                            Visibility::Private,
                            ModulePath::root(),
                        );
                        let def_id = self.definitions.insert(info);
                        self.rib_stack.define_value(field.name.name, def_id);
                        self.record_binding(field.name.span, def_id);
                    }
                }
            }

            PatternKind::Tuple(elems) => {
                for e in elems {
                    self.resolve_pattern(e);
                }
            }

            PatternKind::List { elements, rest } => {
                for e in elements {
                    self.resolve_pattern(e);
                }
                if let Some(r) = rest {
                    self.resolve_pattern(r);
                }
            }

            PatternKind::TypeNarrowing { name, ty } => {
                // Introduces a binding
                let info = DefinitionInfo::new(
                    DefinitionKind::Local,
                    name.name,
                    name.span,
                    Visibility::Private,
                    ModulePath::root(),
                );
                let def_id = self.definitions.insert(info);
                self.rib_stack.define_value(name.name, def_id);
                self.record_binding(name.span, def_id);

                self.resolve_type_expr(ty);
            }

            PatternKind::Or(patterns) => {
                for p in patterns {
                    self.resolve_pattern(p);
                }
            }

            PatternKind::Guard { pattern, guard } => {
                self.resolve_pattern(pattern);
                self.resolve_expr(guard);
            }
        }
    }

    // ========================================================================
    // Path resolution helpers
    // ========================================================================

    /// Resolves a path in the value namespace.
    fn resolve_value_path(&mut self, path: &spark_ast::Path) {
        if path.is_simple() {
            let name = path.segments[0].name;
            if let Some(def_id) = self.rib_stack.lookup_value(name) {
                self.record_path(path.span, def_id);
            } else {
                self.error_undefined_name(name, path.span);
            }
        } else {
            // Multi-segment path: try to resolve through type namespace for first segment
            // (e.g., Enum.Variant), or through module imports
            let first = path.segments[0].name;
            if let Some(def_id) = self.rib_stack.lookup_value(first) {
                self.record_path(path.span, def_id);
            } else if let Some(parent_id) = self.rib_stack.lookup_type(first) {
                // Try to resolve as Enum.Variant
                if path.segments.len() == 2 {
                    let variant_name = path.segments[1].name;
                    if let Some(variant_id) = self.find_enum_variant(parent_id, variant_name) {
                        self.record_path(path.span, variant_id);
                    } else {
                        self.record_path(path.span, parent_id);
                    }
                } else {
                    self.record_path(path.span, parent_id);
                }
            } else {
                self.error_undefined_name(first, path.segments[0].span);
            }
        }
    }

    /// Resolves a path in the type namespace.
    fn resolve_type_path(&mut self, path: &spark_ast::Path) {
        if path.is_simple() {
            let name = path.segments[0].name;
            if let Some(def_id) = self.rib_stack.lookup_type(name) {
                self.record_path(path.span, def_id);
            } else if kw::is_builtin_type(name) {
                // Built-in types don't have DefIds yet — skip silently.
                // These will be handled in spark_types.
            } else {
                self.error_undefined_type(name, path.span);
            }
        } else {
            // Multi-segment type path (e.g., Color.Red)
            let first = path.segments[0].name;
            if let Some(parent_id) = self.rib_stack.lookup_type(first) {
                // If this is a 2-segment path like Enum.Variant, try to resolve
                // the second segment as an enum variant.
                if path.segments.len() == 2 {
                    let variant_name = path.segments[1].name;
                    if let Some(variant_id) = self.find_enum_variant(parent_id, variant_name) {
                        self.record_path(path.span, variant_id);
                    } else {
                        // Fall back to recording the parent (e.g., module.Type)
                        self.record_path(path.span, parent_id);
                    }
                } else {
                    self.record_path(path.span, parent_id);
                }
            } else if kw::is_builtin_type(first) {
                // Built-in types — skip
            } else {
                self.error_undefined_type(first, path.segments[0].span);
            }
        }
    }

    // ========================================================================
    // Lookup helpers
    // ========================================================================

    /// Finds an enum variant by parent enum `DefId` and variant name.
    ///
    /// Returns `None` if the parent isn't an enum or has no such variant.
    fn find_enum_variant(&self, parent_id: DefId, variant_name: Symbol) -> Option<DefId> {
        // Verify the parent is actually an enum
        let parent_info = self.definitions.get(parent_id)?;
        if !matches!(parent_info.kind, DefinitionKind::Enum) {
            return None;
        }

        // Search for a variant with matching parent and name
        self.definitions.iter().find_map(|(id, info)| {
            if matches!(info.kind, DefinitionKind::EnumVariant(pid) if pid == parent_id)
                && info.name == variant_name
            {
                Some(id)
            } else {
                None
            }
        })
    }

    // ========================================================================
    // Resolution recording helpers
    // ========================================================================

    /// Records an identifier resolution.
    fn record_ident(&mut self, span: Span, def_id: DefId) {
        self.resolved
            .get_or_create_resolutions(self.current_file)
            .record_ident(span, def_id);
    }

    /// Records a path resolution.
    fn record_path(&mut self, span: Span, def_id: DefId) {
        self.resolved
            .get_or_create_resolutions(self.current_file)
            .record_path(span, def_id);
    }

    /// Records a binding (where a name is introduced).
    fn record_binding(&mut self, span: Span, def_id: DefId) {
        self.resolved
            .get_or_create_resolutions(self.current_file)
            .record_binding(span, def_id);
    }

    // ========================================================================
    // Error reporting
    // ========================================================================

    /// Reports a duplicate definition error.
    #[allow(dead_code)]
    fn error_duplicate_definition(&mut self, name: Symbol, new_span: Span, prev_def_id: DefId) {
        let prev_span = self
            .definitions
            .get(prev_def_id)
            .map_or(Span::DUMMY, |info| info.span);

        self.diagnostics.push(
            Diagnostic::error(format!(
                "the name `{}` is defined multiple times",
                name.as_str()
            ))
            .with_code("E0428")
            .with_label(Label::primary(new_span, "redefined here"))
            .with_label(Label::secondary(prev_span, "previous definition here")),
        );
    }

    /// Warns when an import shadows a local definition from the same module.
    fn warn_import_shadows_local(
        &mut self,
        name: Symbol,
        import_span: Span,
        prev_def_id: DefId,
        module_path: &ModulePath,
    ) {
        if let Some(prev_info) = self.definitions.get(prev_def_id) {
            if prev_info.module == *module_path {
                self.diagnostics.push(
                    Diagnostic::warning(format!(
                        "import `{}` shadows local definition",
                        name.as_str()
                    ))
                    .with_label(Label::primary(import_span, "this import"))
                    .with_label(Label::secondary(prev_info.span, "local definition here")),
                );
            }
        }
    }

    /// Reports an undefined value name error.
    fn error_undefined_name(&mut self, name: Symbol, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!(
                "cannot find value `{}` in this scope",
                name.as_str()
            ))
            .with_code("E0425")
            .with_label(Label::primary(span, "not found in this scope")),
        );
    }

    /// Reports an undefined type name error.
    fn error_undefined_type(&mut self, name: Symbol, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!(
                "cannot find type `{}` in this scope",
                name.as_str()
            ))
            .with_code("E0412")
            .with_label(Label::primary(span, "not found in this scope")),
        );
    }

    /// Converts the resolver into its output.
    #[must_use]
    pub fn into_output(self) -> ResolverOutput {
        ResolverOutput {
            resolved: self.resolved,
            definitions: self.definitions,
            diagnostics: self.diagnostics,
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use spark_modules::{ModuleGraph, ModuleInfo, ModulePath};

    /// Helper: parse source and resolve with a single-file module setup.
    fn resolve_source(source: &str) -> ResolverOutput {
        use spark_lexer::Lexer;
        use spark_parser::parse;

        let file_id = FileId::new(0);
        let mut lexer = Lexer::new(file_id, source);
        let tokens = lexer.tokenize();
        let (ast, _diags) = parse(tokens, file_id);

        let module_path = ModulePath::from_single(spark_intern::sym("test"));

        let mut graph = ModuleGraph::default();
        let mut module_info = ModuleInfo::new(module_path.clone());
        module_info.files.push(file_id);
        // Mark all items as exported for testing
        for item in &ast.items {
            let name = match item {
                Item::Function(f) => Some(f.name.name),
                Item::Struct(s) => Some(s.name.name),
                Item::Enum(e) => Some(e.name.name),
                Item::Interface(i) => Some(i.name.name),
                Item::TypeAlias(t) => Some(t.name.name),
                Item::TypeDef(t) => Some(t.name.name),
                Item::Const(c) => Some(c.name.name),
                Item::Binding(_) => None,
            };
            if let Some(n) = name {
                module_info.exports.insert(n);
            }
        }
        graph.modules.insert(module_path.clone(), module_info);
        graph.compilation_order.push(module_path);

        let mut asts = HashMap::new();
        asts.insert(file_id, ast);

        let input = ResolverInput { graph, asts };
        crate::resolve(input)
    }

    #[test]
    fn resolver_empty_input() {
        let input = ResolverInput {
            graph: ModuleGraph::default(),
            asts: HashMap::new(),
        };

        let mut resolver = Resolver::new(input);
        resolver.resolve();
        let output = resolver.into_output();

        assert!(!output.diagnostics.has_errors());
        // Built-in functions (e.g., print) are always registered
        assert!(output.definitions.len() >= 1);
    }

    #[test]
    fn resolve_single_function() {
        let output = resolve_source("fn main(): Void {}");
        assert!(!output.diagnostics.has_errors());
        assert!(output.definitions.len() >= 1);

        // Should have a Function definition for "main"
        let main_def = output
            .definitions
            .iter()
            .find(|(_, info)| info.name == spark_intern::sym("main"));
        assert!(main_def.is_some());
        assert!(matches!(main_def.unwrap().1.kind, DefinitionKind::Function));
    }

    #[test]
    fn resolve_function_with_params() {
        let output = resolve_source(
            r#"
            fn add(a: Int, b: Int): Int {
                return a + b
            }
            "#,
        );
        assert!(!output.diagnostics.has_errors());

        // Should have: add (Function), a (Parameter), b (Parameter)
        let params: Vec<_> = output
            .definitions
            .iter()
            .filter(|(_, info)| matches!(info.kind, DefinitionKind::Parameter))
            .collect();
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn resolve_variable_scoping() {
        let output = resolve_source(
            r#"
            fn test(): Void {
                x: Int = 42
                y: Int = x
            }
            "#,
        );
        // x should resolve when used in y's initializer
        assert!(!output.diagnostics.has_errors());
    }

    #[test]
    fn resolve_undefined_name_error() {
        let output = resolve_source(
            r#"
            fn test(): Void {
                y: Int = unknown_var
            }
            "#,
        );
        assert!(output.diagnostics.has_errors());
    }

    #[test]
    fn resolve_struct_definition() {
        let output = resolve_source(
            r#"
            struct User {
                id: Int
                name: String
            }
            "#,
        );
        assert!(!output.diagnostics.has_errors());

        // Should have: User (Struct) + 2 StructField defs
        let struct_defs: Vec<_> = output
            .definitions
            .iter()
            .filter(|(_, info)| matches!(info.kind, DefinitionKind::Struct))
            .collect();
        assert_eq!(struct_defs.len(), 1);

        let field_defs: Vec<_> = output
            .definitions
            .iter()
            .filter(|(_, info)| matches!(info.kind, DefinitionKind::StructField(_)))
            .collect();
        assert_eq!(field_defs.len(), 2);
    }

    #[test]
    fn resolve_enum_definition() {
        let output = resolve_source(
            r#"
            enum Direction {
                North
                South
                East
                West
            }
            "#,
        );
        assert!(!output.diagnostics.has_errors());

        let variant_defs: Vec<_> = output
            .definitions
            .iter()
            .filter(|(_, info)| matches!(info.kind, DefinitionKind::EnumVariant(_)))
            .collect();
        assert_eq!(variant_defs.len(), 4);
    }

    #[test]
    fn resolve_type_reference() {
        let output = resolve_source(
            r#"
            struct User {
                name: String
            }
            fn getUser(): User {
                return User { name: "Alice" }
            }
            "#,
        );
        // User should resolve in return type and struct init
        assert!(!output.diagnostics.has_errors());
    }

    #[test]
    fn resolve_shadowing() {
        let output = resolve_source(
            r#"
            fn test(): Void {
                x: Int = 1
                {
                    x: Int = 2
                    y: Int = x
                }
            }
            "#,
        );
        // Shadowing is allowed — no errors
        assert!(!output.diagnostics.has_errors());
    }

    #[test]
    fn resolve_block_scoping() {
        let output = resolve_source(
            r#"
            fn test(): Void {
                {
                    inner: Int = 1
                }
                y: Int = inner
            }
            "#,
        );
        // inner is not visible outside its block — should error
        assert!(output.diagnostics.has_errors());
    }

    #[test]
    fn resolve_if_expression() {
        let output = resolve_source(
            r#"
            fn test(x: Int): Int {
                if x > 0 {
                    return x
                } else {
                    return 0
                }
            }
            "#,
        );
        assert!(!output.diagnostics.has_errors());
    }

    #[test]
    fn resolve_match_pattern_bindings() {
        let output = resolve_source(
            r#"
            fn test(x: Int): Int {
                match x {
                    n if n > 0 -> n,
                    _ -> 0
                }
            }
            "#,
        );
        assert!(!output.diagnostics.has_errors());
    }

    #[test]
    fn resolve_lambda() {
        let output = resolve_source(
            r#"
            fn test(): Void {
                f: (Int): Int = (x: Int): Int { x + 1 }
            }
            "#,
        );
        assert!(!output.diagnostics.has_errors());
    }

    #[test]
    fn resolve_const_definition() {
        let output = resolve_source(
            r#"
            const MAX: Int = 100
            fn test(): Int {
                return MAX
            }
            "#,
        );
        assert!(!output.diagnostics.has_errors());
    }

    #[test]
    fn resolve_forward_reference() {
        let output = resolve_source(
            r#"
            fn caller(): Void {
                callee()
            }
            fn callee(): Void {}
            "#,
        );
        // Forward references between top-level items should work
        assert!(!output.diagnostics.has_errors());
    }

    #[test]
    fn resolve_generic_function() {
        let output = resolve_source(
            r#"
            fn identity[T](x: T): T {
                return x
            }
            "#,
        );
        assert!(!output.diagnostics.has_errors());

        let generic_defs: Vec<_> = output
            .definitions
            .iter()
            .filter(|(_, info)| matches!(info.kind, DefinitionKind::GenericParam))
            .collect();
        assert_eq!(generic_defs.len(), 1);
    }

    #[test]
    fn resolve_multiple_functions() {
        let output = resolve_source(
            r#"
            fn first(x: Int): Int { return x }
            fn second(y: Int): Int { return first(y) }
            "#,
        );
        assert!(!output.diagnostics.has_errors());
    }

    #[test]
    fn resolve_nested_blocks() {
        // Use if-blocks to test nested scoping since the parser handles
        // deeply nested bare blocks with bindings imperfectly.
        let output = resolve_source(
            r#"
            fn test(x: Int): Void {
                a: Int = 1
                if x > 0 {
                    b: Int = a
                    if b > 0 {
                        c: Int = b
                    }
                }
            }
            "#,
        );
        assert!(!output.diagnostics.has_errors());
    }

    /// Helper: set up a multi-module resolver scenario.
    ///
    /// `modules` is a list of `(module_name, source_code, exports)` for dependency modules.
    /// `main_source` is the source for the "main" module that imports from them.
    fn resolve_multi_module(
        modules: &[(&str, &str, &[&str])],
        main_source: &str,
    ) -> ResolverOutput {
        use spark_lexer::Lexer;
        use spark_parser::parse;

        let mut asts = HashMap::new();
        let mut graph = ModuleGraph::default();
        let mut file_counter = 0u32;

        for (mod_name, source, exports) in modules {
            let file_id = FileId::new(file_counter);
            file_counter += 1;

            let mut lexer = Lexer::new(file_id, source);
            let tokens = lexer.tokenize();
            let (ast, _) = parse(tokens, file_id);

            let module_path = ModulePath::from_single(spark_intern::sym(mod_name));
            let mut module_info = ModuleInfo::new(module_path.clone());
            module_info.files.push(file_id);
            for &export_name in *exports {
                module_info.exports.insert(spark_intern::sym(export_name));
            }

            graph.modules.insert(module_path.clone(), module_info);
            graph.compilation_order.push(module_path);
            asts.insert(file_id, ast);
        }

        let main_file_id = FileId::new(file_counter);
        let mut lexer = Lexer::new(main_file_id, main_source);
        let tokens = lexer.tokenize();
        let (ast, _) = parse(tokens, main_file_id);

        let main_module_path = ModulePath::from_single(spark_intern::sym("main"));
        let mut main_info = ModuleInfo::new(main_module_path.clone());
        main_info.files.push(main_file_id);

        graph
            .modules
            .insert(main_module_path.clone(), main_info);
        graph.compilation_order.push(main_module_path);
        asts.insert(main_file_id, ast);

        let input = ResolverInput { graph, asts };
        crate::resolve(input)
    }

    #[test]
    fn duplicate_import_from_different_modules_errors() {
        let output = resolve_multi_module(
            &[
                ("mod_a", "fn isError(): Bool { return false }", &["isError"]),
                ("mod_b", "fn isError(): Bool { return true }", &["isError"]),
            ],
            r#"
            import mod_a { isError }
            import mod_b { isError }

            fn main(): Void {
                isError()
            }
            "#,
        );
        assert!(output.diagnostics.has_errors());
        let errors: Vec<_> = output
            .diagnostics
            .iter()
            .filter(|d| d.is_error())
            .collect();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("imported from both"));
    }

    #[test]
    fn same_import_twice_from_same_module_no_error() {
        let output = resolve_multi_module(
            &[("mod_a", "fn helper(): Void {}", &["helper"])],
            r#"
            import mod_a { helper }
            import mod_a { helper }

            fn main(): Void {
                helper()
            }
            "#,
        );
        assert!(!output.diagnostics.has_errors());
    }

    #[test]
    fn import_shadows_local_def_warns() {
        let output = resolve_multi_module(
            &[("mod_a", "fn doStuff(): Void {}", &["doStuff"])],
            r#"
            import mod_a { doStuff }

            fn doStuff(): Void {}

            fn main(): Void {
                doStuff()
            }
            "#,
        );
        // Should not produce errors, but should produce a warning
        assert!(!output.diagnostics.has_errors());
        assert!(output.diagnostics.has_warnings());
        let warnings: Vec<_> = output
            .diagnostics
            .iter()
            .filter(|d| d.is_warning())
            .collect();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("shadows local definition"));
    }

    #[test]
    fn aliased_import_avoids_conflict() {
        let output = resolve_multi_module(
            &[
                ("mod_a", "fn isError(): Bool { return false }", &["isError"]),
                ("mod_b", "fn isError(): Bool { return true }", &["isError"]),
            ],
            r#"
            import mod_a { isError }
            import mod_b { isError as isErrorB }

            fn main(): Void {
                isError()
                isErrorB()
            }
            "#,
        );
        assert!(!output.diagnostics.has_errors());
    }
}
