//! Main type checker orchestration.
//!
//! The [`TypeChecker`] drives the two-pass type checking process:
//! 1. **Collection**: Walk all declarations to register type signatures
//! 2. **Checking**: Walk all function bodies to type-check expressions

use spark_ast::Item;
use spark_diagnostics::Diagnostics;
use spark_modules::ModuleGraph;
use spark_resolve::{DefinitionTable, ResolvedAst};

use spark_intern::kw;
use spark_resolve::DefinitionKind;

use crate::builtins::{register_builtins, BuiltinTypes};
use crate::check_decl::collect_item;
use crate::check_expr::{check_block, check_expr};
use crate::lower::lower_type_expr;
use crate::table::TypeTable;
use crate::ty::{FloatWidth, IntWidth, Ty};

/// The main type checker.
///
/// Holds all state needed for type checking across all files.
/// Borrows the resolved AST and definition table — it only reads them.
pub struct TypeChecker<'a> {
    /// The type table being built.
    table: TypeTable,
    /// Built-in type IDs.
    builtins: BuiltinTypes,
    /// Definition table from the resolver (borrowed).
    definitions: &'a DefinitionTable,
    /// Resolved ASTs with name resolution mappings (borrowed).
    resolved: &'a ResolvedAst,
    /// Module graph (for compilation order).
    #[allow(dead_code)]
    graph: ModuleGraph,
    /// Accumulated diagnostics.
    diagnostics: Diagnostics,
}

impl<'a> TypeChecker<'a> {
    /// Creates a new type checker from resolver output.
    #[must_use]
    pub fn new(
        resolved: &'a ResolvedAst,
        definitions: &'a DefinitionTable,
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
        self.check_bodies();

        TypeCheckerOutput {
            table: self.table,
            diagnostics: self.diagnostics,
        }
    }

    /// Pass 1: Collect all type signatures.
    fn collect_signatures(&mut self) {
        let resolved = self.resolved;
        let definitions = self.definitions;

        for (file_id, ast) in &resolved.asts {
            let resolutions = resolved.resolutions.get(file_id);
            for item in &ast.items {
                collect_item(
                    item,
                    &mut self.table,
                    &self.builtins,
                    definitions,
                    resolutions,
                    &mut self.diagnostics,
                );
            }
        }
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

        let definitions = self.definitions;
        for (def_id, info) in definitions.iter() {
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
            } else if info.name == kw::PARALLEL_FOREACH {
                // Internal: __parallel_foreach(list, callback) -> Void
                // Actual calling convention handled at codegen level
                let fn_type = self.table.intern(Ty::Function {
                    params: vec![list_int, int_type],
                    ret: void_type,
                    is_async: false,
                });
                self.table.set_def_type(def_id, fn_type);
            } else if info.name == kw::TRY_POLL {
                // __spark_try_poll(future_ptr: Int) -> Int (opaque ptr args)
                let fn_type = self.table.intern(Ty::Function {
                    params: vec![int_type],
                    ret: int_type,
                    is_async: false,
                });
                self.table.set_def_type(def_id, fn_type);
            } else if info.name == kw::PARALLEL_MAP {
                // Internal: __parallel_map(list, callback) -> List[Int]
                // Actual calling convention handled at codegen level
                let fn_type = self.table.intern(Ty::Function {
                    params: vec![list_int, int_type],
                    ret: list_int,
                    is_async: false,
                });
                self.table.set_def_type(def_id, fn_type);
            }
        }
    }

    /// Pass 2: Check all function bodies.
    fn check_bodies(&mut self) {
        let resolved = self.resolved;
        let definitions = self.definitions;

        for (file_id, ast) in &resolved.asts {
            let file_resolutions = resolved.resolutions.get(file_id);
            for item in &ast.items {
                check_item_body(
                    item,
                    file_resolutions,
                    &mut self.table,
                    &self.builtins,
                    definitions,
                    &mut self.diagnostics,
                );
            }
        }
    }
}

/// Checks the body of a single item.
fn check_item_body(
    item: &Item,
    resolutions: Option<&spark_resolve::ResolutionMap>,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    diagnostics: &mut Diagnostics,
) {
    match item {
        Item::Function(func) => {
            if let Some(body) = &func.body {
                // Determine the function's return type
                let ret_type = lower_type_expr(
                    &func.ret_type,
                    table,
                    builtins,
                    definitions,
                    resolutions,
                    diagnostics,
                );

                // Register parameter types for the body scope
                for param in &func.params {
                    let param_ty = lower_type_expr(
                        &param.ty,
                        table,
                        builtins,
                        definitions,
                        resolutions,
                        diagnostics,
                    );
                    if let Some(res) = resolutions {
                        if let Some(def_id) = res.get_binding(param.name.span) {
                            table.set_def_type(def_id, param_ty);
                        }
                    }
                }

                // Check the body
                let body_ty = check_block(
                    body,
                    table,
                    builtins,
                    definitions,
                    resolutions,
                    diagnostics,
                    Some(ret_type),
                );

                // Functions require explicit `return` to return a value.
                // If the return type is non-void and the body doesn't diverge
                // (i.e. body_ty is not Never), then a return statement is missing.
                let body_resolved = table.resolve(body_ty);
                if ret_type != builtins.void
                    && !matches!(body_resolved, crate::Ty::Never)
                {
                    let ret_name = crate::check_expr::type_name(
                        ret_type,
                        table,
                        definitions,
                    );
                    diagnostics.push(
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
                table,
                builtins,
                definitions,
                resolutions,
                diagnostics,
            );

            let val_ty = check_expr(
                &const_def.value,
                table,
                builtins,
                definitions,
                resolutions,
                diagnostics,
                None,
            );

            if !crate::check_expr::is_compatible(val_ty, expected_ty, table) {
                let expected = crate::check_expr::type_name(expected_ty, table, definitions);
                let found = crate::check_expr::type_name(val_ty, table, definitions);
                diagnostics.push(
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
            check_binding_bodies(binding, resolutions, table, builtins, definitions, diagnostics);
        }
        // Other items don't have bodies to check
        _ => {}
    }
}

/// Type-checks all method bodies in a binding.
fn check_binding_bodies(
    binding: &spark_ast::BindingDef,
    resolutions: Option<&spark_resolve::ResolutionMap>,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    diagnostics: &mut Diagnostics,
) {
    for target in &binding.bindings {
        for method in &target.methods {
            if let Some(body) = &method.body {
                // Get the method's return type from its function type
                let ret_type = lower_type_expr(
                    &method.ret_type,
                    table,
                    builtins,
                    definitions,
                    resolutions,
                    diagnostics,
                );

                // Register parameter types for the body scope
                for param in &method.params {
                    let param_ty = lower_type_expr(
                        &param.ty,
                        table,
                        builtins,
                        definitions,
                        resolutions,
                        diagnostics,
                    );
                    if let Some(res) = resolutions {
                        if let Some(def_id) = res.get_binding(param.name.span) {
                            table.set_def_type(def_id, param_ty);
                        }
                    }
                }

                // Check the body
                let body_ty = check_block(
                    body,
                    table,
                    builtins,
                    definitions,
                    resolutions,
                    diagnostics,
                    Some(ret_type),
                );

                // Validate return completeness
                let body_resolved = table.resolve(body_ty);
                if ret_type != builtins.void
                    && !matches!(body_resolved, crate::Ty::Never)
                {
                    let ret_name = crate::check_expr::type_name(
                        ret_type,
                        table,
                        definitions,
                    );
                    diagnostics.push(
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

/// Output from the type checker.
#[derive(Debug)]
pub struct TypeCheckerOutput {
    /// The type table with all type information.
    pub table: TypeTable,
    /// Diagnostics accumulated during type checking.
    pub diagnostics: Diagnostics,
}
