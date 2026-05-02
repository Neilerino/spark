//! Main HIR lowering orchestration.
//!
//! The [`Lowerer`] walks the resolved, typed AST and produces [`Hir`] nodes.
//! It dispatches to sub-modules for expressions, statements, and patterns.

use std::collections::HashMap;

use spark_ast::Item;
use spark_diagnostics::Diagnostics;
use spark_resolve::{DefId, DefinitionTable, ResolutionMap, ResolvedAst};
use spark_span::FileId;
use spark_types::builtins::BuiltinTypes;
use spark_types::{TypeId, TypeTable};

use crate::hir::{Hir, HirFunction, HirParam, HirStatic};
use crate::lower_expr::{lower_block, lower_expr, LowerCtx};

/// The HIR lowerer.
///
/// Holds all context needed to lower an AST into HIR.
pub struct Lowerer<'a> {
    /// The type table (read-only).
    pub(crate) table: &'a TypeTable,
    /// Definition table (mutable, for allocating synthetic `DefId`s during desugaring).
    pub(crate) definitions: &'a mut DefinitionTable,
    /// The resolved AST.
    resolved: &'a ResolvedAst,
    /// Built-in type mappings for name-to-type lookups.
    pub(crate) builtins: &'a BuiltinTypes,
    /// Accumulated diagnostics.
    pub(crate) diagnostics: Diagnostics,
}

impl<'a> Lowerer<'a> {
    /// Creates a new lowerer.
    #[must_use]
    pub fn new(
        table: &'a TypeTable,
        definitions: &'a mut DefinitionTable,
        resolved: &'a ResolvedAst,
        builtins: &'a BuiltinTypes,
    ) -> Self {
        Self {
            table,
            definitions,
            resolved,
            builtins,
            diagnostics: Diagnostics::new(),
        }
    }

    /// Lowers all files and returns the HIR.
    #[must_use]
    pub fn lower_all(mut self) -> (Hir, Diagnostics) {
        let mut hir = Hir {
            functions: HashMap::new(),
            statics: HashMap::new(),
        };

        for (file_id, ast) in &self.resolved.asts {
            let resolutions = self.resolved.resolutions.get(file_id);
            self.lower_file(*file_id, ast, resolutions, &mut hir);
        }

        (hir, self.diagnostics)
    }

    /// Lowers a single file's items.
    fn lower_file(
        &mut self,
        _file_id: FileId,
        ast: &spark_ast::Ast,
        resolutions: Option<&ResolutionMap>,
        hir: &mut Hir,
    ) {
        for item in &ast.items {
            match item {
                Item::Function(func) => {
                    if let Some(body) = &func.body {
                        self.lower_function(func, body, resolutions, hir);
                    }
                }
                Item::Const(const_def) => {
                    self.lower_const(const_def, resolutions, hir);
                }
                Item::Binding(binding) => {
                    self.lower_binding(binding, resolutions, hir);
                }
                // Struct, Enum, Interface, TypeAlias, TypeDef —
                // no runtime code, fully represented in TypeTable/DefinitionTable.
                _ => {}
            }
        }
    }

    /// Lowers a function definition.
    fn lower_function(
        &mut self,
        func: &spark_ast::FunctionDef,
        body: &spark_ast::Block,
        resolutions: Option<&ResolutionMap>,
        hir: &mut Hir,
    ) {
        let def_id = resolutions
            .and_then(|res| res.get_binding(func.name.span))
            .unwrap_or(DefId::DUMMY);

        if def_id.is_dummy() {
            return;
        }

        // Lower parameters
        let params = self.lower_params(&func.params, resolutions);

        // Get return type
        let ret_type = self
            .table
            .get_def_type(def_id)
            .and_then(|fn_ty| {
                if let spark_types::Ty::Function { ret, .. } = self.table.resolve(fn_ty) {
                    Some(*ret)
                } else {
                    None
                }
            })
            .unwrap_or(TypeId::ERROR);

        // Lower body
        let mut ctx = LowerCtx {
            table: self.table,
            definitions: &mut *self.definitions,
            resolutions,
            builtins: self.builtins,
            diagnostics: &mut self.diagnostics,
            current_fn_ret: Some(ret_type),
        };
        let hir_body = lower_block(body, &mut ctx);

        hir.functions.insert(
            def_id,
            HirFunction {
                def_id,
                params,
                body: hir_body,
                ret_type,
                is_async: func.is_async,
                span: func.span,
                captures: vec![],
            },
        );
    }

    /// Lowers a constant definition.
    fn lower_const(
        &mut self,
        const_def: &spark_ast::ConstDef,
        resolutions: Option<&ResolutionMap>,
        hir: &mut Hir,
    ) {
        let def_id = resolutions
            .and_then(|res| res.get_binding(const_def.name.span))
            .unwrap_or(DefId::DUMMY);

        if def_id.is_dummy() {
            return;
        }

        let ty = self.table.get_def_type(def_id).unwrap_or(TypeId::ERROR);

        let mut ctx = LowerCtx {
            table: self.table,
            definitions: &mut *self.definitions,
            resolutions,
            builtins: self.builtins,
            diagnostics: &mut self.diagnostics,
            current_fn_ret: None,
        };
        let value = lower_expr(&const_def.value, &mut ctx);

        hir.statics.insert(
            def_id,
            HirStatic {
                def_id,
                ty,
                value,
                span: const_def.span,
            },
        );
    }

    /// Lowers a binding definition's method bodies.
    ///
    /// Each binding method with a body is lowered as a regular function in HIR.
    fn lower_binding(
        &mut self,
        binding: &spark_ast::BindingDef,
        resolutions: Option<&ResolutionMap>,
        hir: &mut Hir,
    ) {
        for target in &binding.bindings {
            for method in &target.methods {
                if let Some(body) = &method.body {
                    // Find the method DefId — it's a BindingMethod
                    let def_id = resolutions
                        .and_then(|res| res.get_binding(method.name.span))
                        .unwrap_or(DefId::DUMMY);

                    if def_id.is_dummy() {
                        continue;
                    }

                    // Lower parameters
                    let params = self.lower_params(&method.params, resolutions);

                    // Get return type from the function type
                    let ret_type = self
                        .table
                        .get_def_type(def_id)
                        .and_then(|fn_ty| {
                            if let spark_types::Ty::Function { ret, .. } = self.table.resolve(fn_ty)
                            {
                                Some(*ret)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(TypeId::ERROR);

                    // Lower body
                    let mut ctx = LowerCtx {
                        table: self.table,
                        definitions: &mut *self.definitions,
                        resolutions,
                        builtins: self.builtins,
                        diagnostics: &mut self.diagnostics,
                        current_fn_ret: Some(ret_type),
                    };
                    let hir_body = lower_block(body, &mut ctx);

                    hir.functions.insert(
                        def_id,
                        HirFunction {
                            def_id,
                            params,
                            body: hir_body,
                            ret_type,
                            is_async: method.is_async,
                            span: method.span,
                            captures: vec![],
                        },
                    );
                }
            }
        }
    }

    /// Lowers function parameters to HIR.
    fn lower_params(
        &self,
        params: &[spark_ast::FnParam],
        resolutions: Option<&ResolutionMap>,
    ) -> Vec<HirParam> {
        params
            .iter()
            .map(|p| {
                let def_id = resolutions
                    .and_then(|res| res.get_binding(p.name.span))
                    .unwrap_or(DefId::DUMMY);
                let ty = self.table.get_def_type(def_id).unwrap_or(TypeId::ERROR);
                HirParam {
                    def_id,
                    ty,
                    span: p.span,
                }
            })
            .collect()
    }
}
