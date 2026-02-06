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

use crate::builtins::{register_builtins, BuiltinTypes};
use crate::check_decl::collect_item;
use crate::check_expr::{check_block, check_expr};
use crate::lower::lower_type_expr;
use crate::table::TypeTable;

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

                    // Verify the body's type matches the return type
                    if !crate::check_expr::is_compatible(body_ty, ret_type, &self.table) {
                        let expected = crate::check_expr::type_name(ret_type, &self.table, &self.definitions);
                        let found = crate::check_expr::type_name(body_ty, &self.table, &self.definitions);
                        self.diagnostics.push(
                            spark_diagnostics::Diagnostic::error(format!(
                                "function `{}` body type mismatch: expected `{expected}`, found `{found}`",
                                func.name.name
                            ))
                            .with_label(spark_diagnostics::Label::primary(
                                body.span,
                                format!("expected `{expected}`"),
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
            // Other items don't have bodies to check
            _ => {}
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
