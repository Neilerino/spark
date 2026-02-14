//! Main type checker orchestration.
//!
//! The [`TypeChecker`] drives the two-pass type checking process:
//! 1. **Collection**: Walk all declarations to register type signatures
//! 2. **Checking**: Walk all function bodies to type-check expressions

use std::collections::HashMap;

use spark_ast::{Ast, Item};
use spark_diagnostics::Diagnostics;
use spark_modules::ModuleGraph;
use spark_resolve::{DefinitionTable, ResolvedAst};
use spark_span::FileId;

use spark_intern::kw;
use spark_resolve::DefinitionKind;
use spark_stdlib::{StdlibRegistry, StdlibType};

use crate::builtins::{register_builtins, BuiltinTypes};
use crate::check_decl::collect_item;
use crate::check_expr::{check_block, check_expr};
use crate::lower::lower_type_expr;
use crate::table::TypeTable;
use crate::ty::{FloatWidth, IntWidth, Ty};

/// The main type checker.
///
/// Holds all state needed for type checking across all files.
pub struct TypeChecker {
    /// The type table being built.
    table: TypeTable,
    /// Built-in type IDs.
    builtins: BuiltinTypes,
    /// Definition table from the resolver.
    definitions: DefinitionTable,
    /// Resolved ASTs with name resolution mappings.
    resolved: ResolvedAst,
    /// Module graph (for compilation order).
    #[allow(dead_code)]
    graph: ModuleGraph,
    /// Accumulated diagnostics.
    diagnostics: Diagnostics,
}

impl TypeChecker {
    /// Creates a new type checker from resolver output.
    #[must_use]
    pub fn new(
        resolved: ResolvedAst,
        definitions: DefinitionTable,
        graph: ModuleGraph,
    ) -> Self {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);

        Self {
            table,
            builtins,
            definitions,
            resolved,
            graph,
            diagnostics: Diagnostics::new(),
        }
    }

    /// Runs type checking and returns the results.
    #[must_use]
    pub fn check(mut self) -> TypeCheckerOutput {
        self.collect_signatures();
        self.register_builtin_functions();
        self.register_stdlib_functions();
        self.check_bodies();

        TypeCheckerOutput {
            table: self.table,
            diagnostics: self.diagnostics,
        }
    }

    /// Pass 1: Collect all type signatures.
    fn collect_signatures(&mut self) {
        // Take asts out temporarily to avoid borrow conflicts
        let asts: HashMap<FileId, Ast> = std::mem::take(&mut self.resolved.asts);

        for (file_id, ast) in &asts {
            let resolutions = self.resolved.resolutions.get(file_id);
            for item in &ast.items {
                collect_item(
                    item,
                    &mut self.table,
                    &self.builtins,
                    &self.definitions,
                    resolutions,
                    &mut self.diagnostics,
                );
            }
        }

        // Put asts back
        self.resolved.asts = asts;
    }

    /// Registers type signatures for built-in functions (e.g., `print`, `sort`, `take`, `reverse`).
    ///
    /// Called after `collect_signatures()` so that built-in function types
    /// are available during body checking.
    fn register_builtin_functions(&mut self) {
        let int_type = self.table.intern(Ty::Int(IntWidth::I64));
        let float_type = self.table.intern(Ty::Float(FloatWidth::F64));
        let bool_type = self.table.intern(Ty::Bool);
        let string_type = self.builtins.string;
        let void_type = self.table.intern(Ty::Void);
        let list_int = self.table.intern(Ty::List(int_type));

        for (def_id, info) in self.definitions.iter() {
            if !matches!(info.kind, DefinitionKind::BuiltinFunction) {
                continue;
            }

            if info.name == kw::PRINT {
                let param_type = self.table.intern(Ty::Union(vec![
                    int_type, float_type, bool_type, string_type,
                ]));
                let fn_type = self.table.intern(Ty::Function {
                    params: vec![param_type],
                    ret: void_type,
                    is_async: false,
                });
                self.table.set_def_type(def_id, fn_type);
            } else if info.name == kw::SORT {
                // sort(List[Int]) -> List[Int]
                let fn_type = self.table.intern(Ty::Function {
                    params: vec![list_int],
                    ret: list_int,
                    is_async: false,
                });
                self.table.set_def_type(def_id, fn_type);
            } else if info.name == kw::TAKE {
                // take(List[Int], Int) -> List[Int]
                let fn_type = self.table.intern(Ty::Function {
                    params: vec![list_int, int_type],
                    ret: list_int,
                    is_async: false,
                });
                self.table.set_def_type(def_id, fn_type);
            } else if info.name == kw::REVERSE {
                // reverse(List[Int]) -> List[Int]
                let fn_type = self.table.intern(Ty::Function {
                    params: vec![list_int],
                    ret: list_int,
                    is_async: false,
                });
                self.table.set_def_type(def_id, fn_type);
            }
        }
    }

    /// Registers type signatures for standard library functions.
    ///
    /// Iterates over all `StdlibFunction` definitions and assigns function
    /// types by matching against the stdlib manifest.
    fn register_stdlib_functions(&mut self) {
        let registry = StdlibRegistry::new();

        // Collect stdlib defs first to avoid borrow conflict with self.definitions
        let stdlib_defs: Vec<_> = self
            .definitions
            .iter()
            .filter(|(_, info)| matches!(info.kind, DefinitionKind::StdlibFunction))
            .map(|(def_id, info)| (def_id, info.name, info.module.clone()))
            .collect();

        for (def_id, name, module_path) in stdlib_defs {
            let Some(module) = registry.find_module(module_path.segments()) else {
                continue;
            };

            let Some(entry) = module.find_entry(name) else {
                continue;
            };

            // Convert StdlibType descriptors to TypeIds
            let params: Vec<_> = entry
                .params
                .iter()
                .map(|t| self.stdlib_type_to_id(*t))
                .collect();
            let ret = self.stdlib_type_to_id(entry.ret);

            let fn_type = self.table.intern(Ty::Function {
                params,
                ret,
                is_async: false,
            });
            self.table.set_def_type(def_id, fn_type);
        }
    }

    /// Converts a [`StdlibType`] descriptor to a real `TypeId`.
    fn stdlib_type_to_id(&mut self, ty: StdlibType) -> crate::TypeId {
        match ty {
            StdlibType::Float => self.table.intern(Ty::Float(FloatWidth::F64)),
            StdlibType::Int => self.table.intern(Ty::Int(IntWidth::I64)),
            StdlibType::Bool => self.table.intern(Ty::Bool),
            StdlibType::String => self.builtins.string,
            StdlibType::Void => self.table.intern(Ty::Void),
        }
    }

    /// Pass 2: Check all function bodies.
    fn check_bodies(&mut self) {
        let asts: HashMap<FileId, Ast> = std::mem::take(&mut self.resolved.asts);
        let resolutions = std::mem::take(&mut self.resolved.resolutions);

        for (file_id, ast) in &asts {
            let file_resolutions = resolutions.get(file_id);
            for item in &ast.items {
                self.check_item_body(item, file_resolutions);
            }
        }

        self.resolved.asts = asts;
        self.resolved.resolutions = resolutions;
    }

    /// Checks the body of a single item.
    fn check_item_body(
        &mut self,
        item: &Item,
        resolutions: Option<&spark_resolve::ResolutionMap>,
    ) {
        match item {
            Item::Function(func) => {
                if let Some(body) = &func.body {
                    // Determine the function's return type
                    let ret_type = lower_type_expr(
                        &func.ret_type,
                        &mut self.table,
                        &self.builtins,
                        &self.definitions,
                        resolutions,
                        &mut self.diagnostics,
                    );

                    // Register parameter types for the body scope
                    for param in &func.params {
                        let param_ty = lower_type_expr(
                            &param.ty,
                            &mut self.table,
                            &self.builtins,
                            &self.definitions,
                            resolutions,
                            &mut self.diagnostics,
                        );
                        if let Some(res) = resolutions {
                            if let Some(def_id) = res.get_binding(param.name.span) {
                                self.table.set_def_type(def_id, param_ty);
                            }
                        }
                    }

                    // Check the body
                    let body_ty = check_block(
                        body,
                        &mut self.table,
                        &self.builtins,
                        &self.definitions,
                        resolutions,
                        &mut self.diagnostics,
                        Some(ret_type),
                    );

                    // Functions require explicit `return` to return a value.
                    // If the return type is non-void and the body doesn't diverge
                    // (i.e. body_ty is not Never), then a return statement is missing.
                    let body_resolved = self.table.resolve(body_ty);
                    if ret_type != self.builtins.void
                        && !matches!(body_resolved, crate::Ty::Never)
                    {
                        let ret_name = crate::check_expr::type_name(
                            ret_type,
                            &self.table,
                            &self.definitions,
                        );
                        self.diagnostics.push(
                            spark_diagnostics::Diagnostic::error(format!(
                                "missing `return` in function `{}` with return type `{ret_name}`",
                                func.name.name
                            ))
                            .with_label(spark_diagnostics::Label::primary(
                                body.span,
                                "function body does not return a value on all paths",
                            )),
                        );
                    }
                }
            }
            Item::Const(const_def) => {
                // Check the constant's value expression
                let expected_ty = lower_type_expr(
                    &const_def.ty,
                    &mut self.table,
                    &self.builtins,
                    &self.definitions,
                    resolutions,
                    &mut self.diagnostics,
                );

                let val_ty = check_expr(
                    &const_def.value,
                    &mut self.table,
                    &self.builtins,
                    &self.definitions,
                    resolutions,
                    &mut self.diagnostics,
                    None,
                );

                if !crate::check_expr::is_compatible(val_ty, expected_ty, &self.table) {
                    let expected = crate::check_expr::type_name(expected_ty, &self.table, &self.definitions);
                    let found = crate::check_expr::type_name(val_ty, &self.table, &self.definitions);
                    self.diagnostics.push(
                        spark_diagnostics::Diagnostic::error(format!(
                            "constant `{}` type mismatch: expected `{expected}`, found `{found}`",
                            const_def.name.name
                        ))
                        .with_label(spark_diagnostics::Label::primary(
                            const_def.value.span,
                            format!("expected `{expected}`"),
                        )),
                    );
                }
            }
            Item::Binding(binding) => {
                self.check_binding_bodies(binding, resolutions);
            }
            // Other items don't have bodies to check
            _ => {}
        }
    }

    /// Type-checks all method bodies in a binding.
    fn check_binding_bodies(
        &mut self,
        binding: &spark_ast::BindingDef,
        resolutions: Option<&spark_resolve::ResolutionMap>,
    ) {
        for target in &binding.bindings {
            for method in &target.methods {
                if let Some(body) = &method.body {
                    // Get the method's return type from its function type
                    let ret_type = lower_type_expr(
                        &method.ret_type,
                        &mut self.table,
                        &self.builtins,
                        &self.definitions,
                        resolutions,
                        &mut self.diagnostics,
                    );

                    // Register parameter types for the body scope
                    for param in &method.params {
                        let param_ty = lower_type_expr(
                            &param.ty,
                            &mut self.table,
                            &self.builtins,
                            &self.definitions,
                            resolutions,
                            &mut self.diagnostics,
                        );
                        if let Some(res) = resolutions {
                            if let Some(def_id) = res.get_binding(param.name.span) {
                                self.table.set_def_type(def_id, param_ty);
                            }
                        }
                    }

                    // Check the body
                    let body_ty = check_block(
                        body,
                        &mut self.table,
                        &self.builtins,
                        &self.definitions,
                        resolutions,
                        &mut self.diagnostics,
                        Some(ret_type),
                    );

                    // Validate return completeness
                    let body_resolved = self.table.resolve(body_ty);
                    if ret_type != self.builtins.void
                        && !matches!(body_resolved, crate::Ty::Never)
                    {
                        let ret_name = crate::check_expr::type_name(
                            ret_type,
                            &self.table,
                            &self.definitions,
                        );
                        self.diagnostics.push(
                            spark_diagnostics::Diagnostic::error(format!(
                                "missing `return` in method `{}` with return type `{ret_name}`",
                                method.name.name
                            ))
                            .with_label(spark_diagnostics::Label::primary(
                                body.span,
                                "method body does not return a value on all paths",
                            )),
                        );
                    }
                }
            }
        }
    }
}

/// Output from the type checker.
#[derive(Debug)]
pub struct TypeCheckerOutput {
    /// The type table with all type information.
    pub table: TypeTable,
    /// Diagnostics accumulated during type checking.
    pub diagnostics: Diagnostics,
}
