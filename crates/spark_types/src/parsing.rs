//! Compile-time selection and validation of the standard text parsing protocol.

use spark_ast::{BindingTarget, CallArg, Expr, TypeExpr};
use spark_diagnostics::{Diagnostic, Label};
use spark_intern::kw;
use spark_resolve::{DefId, DefinitionKind, DefinitionTable};
use spark_span::Span;

use crate::check_expr::{check_expr, find_callee_def_id, is_compatible, strip_mutable, ungroup};
use crate::ctx::TypeCheckCtx;
use crate::display::display_type;
use crate::lower::lower_type_expr;
use crate::{Ty, TypeId, TypeTable};

#[derive(Clone, Copy, Debug)]
pub(crate) struct TextParsing {
    pub(crate) interface: DefId,
    entry: DefId,
    error: DefId,
}

impl TextParsing {
    pub(crate) fn from_definitions(definitions: &DefinitionTable) -> Option<Self> {
        let mut interface = None;
        let mut entry = None;
        let mut error = None;
        for (id, info) in definitions.iter() {
            if info.module.segments() != [kw::SPARK, kw::TEXT] {
                continue;
            }
            match info.kind {
                DefinitionKind::Interface if info.name == kw::PARSEABLE => interface = Some(id),
                DefinitionKind::Function if info.name == kw::PARSE => entry = Some(id),
                DefinitionKind::Struct if info.name == kw::PARSE_ERROR => error = Some(id),
                _ => {}
            }
        }
        Some(Self {
            interface: interface?,
            entry: entry?,
            error: error?,
        })
    }

    pub(crate) fn result_type(self, success: TypeId, table: &mut TypeTable) -> TypeId {
        let success = strip_mutable(success, table);
        let error = table.intern(Ty::Struct(self.error));
        let mut members = vec![success, error];
        members.sort_unstable();
        table.intern(Ty::Union(members))
    }

    pub(crate) fn collect_binding(
        self,
        ty: &TypeExpr,
        owner: DefId,
        target: &BindingTarget,
        ctx: &mut TypeCheckCtx<'_>,
    ) {
        let success = lower_type_expr(ty, ctx);
        if success.is_error() {
            return;
        }
        let mut parsers = target.methods.iter().filter(|m| m.name.name == kw::PARSE);
        let Some(parser) = parsers.next() else {
            ctx.diagnostics.push(
                Diagnostic::error("missing required function `parse` from `Parseable`")
                    .with_label(Label::primary(target.interface.span, "requires a parser")),
            );
            return;
        };
        let allowed_module = ctx.definitions.get(owner).and_then(|info| match info.kind {
            DefinitionKind::BuiltinType => ctx.definitions.get(self.interface).map(|i| &i.module),
            DefinitionKind::Struct => Some(&info.module),
            _ => None,
        });
        let parser_id = ctx
            .resolutions
            .and_then(|r| r.get_binding(parser.name.span));
        let Some(parser_id) = parser_id.filter(|&id| {
            ctx.definitions
                .get(id)
                .is_some_and(|info| Some(&info.module) == allowed_module)
        }) else {
            ctx.diagnostics.push(
                Diagnostic::error("a parser must be bound in the type's defining module")
                    .with_label(Label::primary(ty.span, "type is defined elsewhere")),
            );
            return;
        };

        let params: Vec<_> = parser
            .params
            .iter()
            .map(|p| lower_type_expr(&p.ty, ctx))
            .collect();
        let ret = lower_type_expr(&parser.ret_type, ctx);
        let expected_ret = self.result_type(success, ctx.table);
        if params != [ctx.builtins.string]
            || ret != expected_ret
            || parser.is_async
            || !parser.generics.is_empty()
            || parser.body.is_none()
        {
            let name = display_type(success, ctx.table, ctx.definitions);
            ctx.diagnostics.push(
                Diagnostic::error(format!(
                    "invalid parser signature: expected `fn parse(text: String): {name} | ParseError`"
                ))
                .with_label(Label::primary(parser.span, "expected an associated parser")),
            );
            return;
        }
        if parsers.next().is_some() || ctx.table.text_parsers.contains_key(&success) {
            ctx.diagnostics.push(
                Diagnostic::error("duplicate `Parseable` binding for this type").with_label(
                    Label::primary(ty.span, "a type can bind one canonical parser"),
                ),
            );
            return;
        }
        let fn_ty = ctx.table.intern(Ty::Function {
            params,
            ret,
            is_async: false,
        });
        ctx.table.set_def_type(parser_id, fn_ty);
        ctx.table.text_parsers.insert(success, parser_id);
        ctx.table.add_interface_binding(owner, self.interface);
    }

    pub(crate) fn check_call(
        self,
        args: &[CallArg],
        span: Span,
        expected: Option<TypeId>,
        ctx: &mut TypeCheckCtx<'_>,
        current_fn_ret: Option<TypeId>,
    ) -> TypeId {
        let [arg] = args else {
            for arg in args {
                check_expr(&arg.value, ctx, current_fn_ret);
            }
            crate::diagnostics::expect_args(1, args.len(), span, ctx.diagnostics);
            return TypeId::ERROR;
        };
        let actual = check_expr(&arg.value, ctx, current_fn_ret);
        if !is_compatible(actual, ctx.builtins.string, ctx.table) {
            let actual = display_type(actual, ctx.table, ctx.definitions);
            ctx.diagnostics.push(crate::diagnostics::arg_type_mismatch(
                "String",
                &actual,
                arg.value.span,
            ));
            return TypeId::ERROR;
        }

        let error = ctx.table.intern(Ty::Struct(self.error));
        let expected_members = expected
            .map(|expected| strip_mutable(expected, ctx.table))
            .and_then(|expected| match ctx.table.resolve(expected) {
                Ty::Union(members) if members.contains(&error) => Some(members.as_slice()),
                _ => None,
            })
            .unwrap_or_default();
        let mut candidates = expected_members
            .iter()
            .copied()
            .filter(|member| *member != error);
        let Some(success) = candidates.next() else {
            ctx.diagnostics.push(
                Diagnostic::error("cannot infer target type for `parse`").with_label(
                    Label::primary(span, "bind the result with a type annotation"),
                ),
            );
            return TypeId::ERROR;
        };
        if candidates.next().is_some() || matches!(ctx.table.resolve(success), Ty::Union(_)) {
            ctx.diagnostics.push(
                Diagnostic::error("ambiguous target type for `parse`")
                    .with_label(Label::primary(span, "choose one parser target type")),
            );
            return TypeId::ERROR;
        }
        if success.is_error() {
            return TypeId::ERROR;
        }
        let Some(&parser) = ctx.table.text_parsers.get(&success) else {
            let name = display_type(success, ctx.table, ctx.definitions);
            ctx.diagnostics.push(
                Diagnostic::error(format!("type `{name}` does not bind `Parseable`"))
                    .with_label(Label::primary(span, "no parser for this target type")),
            );
            return TypeId::ERROR;
        };
        ctx.table.set_method_resolution(span, parser);
        self.result_type(success, ctx.table)
    }
}

pub(crate) fn protocol_for_call(callee: &Expr, ctx: &TypeCheckCtx<'_>) -> Option<TextParsing> {
    let protocol = ctx.table.text_parsing?;
    let callee = find_callee_def_id(ungroup(callee).span, ctx.resolutions)?;
    (callee == protocol.entry).then_some(protocol)
}
