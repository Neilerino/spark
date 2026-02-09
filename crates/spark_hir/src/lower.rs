//! Main HIR lowering orchestration.
//!
//! The [`Lowerer`] walks the resolved, typed AST and produces [`Hir`] nodes.
//! It dispatches to sub-modules for expressions, statements, and patterns.

use std::collections::HashMap;

use spark_ast::Item;
use spark_diagnostics::Diagnostics;
use spark_resolve::{DefId, DefinitionKind, DefinitionTable, ResolvedAst, ResolutionMap};
use spark_span::FileId;
use spark_types::{TypeId, TypeTable};

use crate::hir::{Hir, HirFunction, HirParam, HirStatic};
use crate::lower_expr::lower_expr;

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
    ) -> Self {
        Self {
            table,
            definitions,
            resolved,
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
                // Struct, Enum, Interface, TypeAlias, TypeDef, Binding —
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
        let def_id = self
            .find_def_by_name_and_kind(func.name.name, &DefinitionKind::Function)
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
        let hir_body = crate::lower_expr::lower_block(
            body,
            self.table,
            &mut *self.definitions,
            resolutions,
            &mut self.diagnostics,
            Some(ret_type),
        );

        hir.functions.insert(def_id, HirFunction {
            def_id,
            params,
            body: hir_body,
            ret_type,
            is_async: func.is_async,
            span: func.span,
        });
    }

    /// Lowers a constant definition.
    fn lower_const(
        &mut self,
        const_def: &spark_ast::ConstDef,
        resolutions: Option<&ResolutionMap>,
        hir: &mut Hir,
    ) {
        let def_id = self
            .find_def_by_name_and_kind(const_def.name.name, &DefinitionKind::Const)
            .unwrap_or(DefId::DUMMY);

        if def_id.is_dummy() {
            return;
        }

        let ty = self.table.get_def_type(def_id).unwrap_or(TypeId::ERROR);

        let value = lower_expr(
            &const_def.value,
            self.table,
            &mut *self.definitions,
            resolutions,
            &mut self.diagnostics,
            None,
        );

        hir.statics.insert(def_id, HirStatic {
            def_id,
            ty,
            value,
            span: const_def.span,
        });
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

    /// Finds a definition by name and kind.
    fn find_def_by_name_and_kind(
        &self,
        name: spark_intern::Symbol,
        kind: &DefinitionKind,
    ) -> Option<DefId> {
        self.definitions
            .iter()
            .find(|(_, info)| info.name == name && &info.kind == kind)
            .map(|(id, _)| id)
    }
}
