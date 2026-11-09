//! MIR monomorphization for generic functions.
//!
//! After MIR construction, this pass:
//! 1. Collects monomorphization instances recorded during type checking.
//! 2. Deduplicates by `(DefId, Vec<TypeId>)`.
//! 3. Clones each generic function body and substitutes all `TypeId`s with
//!    concrete types.
//! 4. Allocates fresh `DefId`s for monomorphized copies.
//! 5. Rewrites call sites to target monomorphized functions.
//! 6. Removes original generic function bodies from the MIR.

use std::collections::VecDeque;

use rustc_hash::FxHashMap;
use spark_diagnostics::{Diagnostic, Diagnostics, Label};
use spark_resolve::{DefId, DefinitionInfo, DefinitionKind, DefinitionTable, Visibility};
use spark_span::Span;
use spark_types::check_expr::{cast_kind, CastKind};
use spark_types::substitute::{substitute_mut, type_involves_param, SubstMap};
use spark_types::{mangle_type, Ty, TypeId, TypeTable};

use crate::mir::{
    AggregateKind, Constant, Mir, MirFunction, Operand, Projection, Rvalue, Statement, Terminator,
};

/// Specializes reachable generic calls and removes generic templates from native MIR.
pub fn monomorphize(
    mir: &mut Mir,
    table: &mut TypeTable,
    definitions: &mut DefinitionTable,
    diagnostics: &mut Diagnostics,
) {
    let templates = mir
        .functions
        .extract_if(|def, _| {
            table
                .get_generic_params(*def)
                .is_some_and(|params| !params.is_empty())
        })
        .collect();
    let calls = table.mono_instances().clone();
    let mut specializer = Specializer {
        table,
        definitions,
        diagnostics,
        templates,
        calls,
        instances: FxHashMap::default(),
        pending: VecDeque::new(),
    };
    for function in mir.functions.values_mut() {
        specializer.rewrite_calls(function, &SubstMap::default(), &[]);
    }
    while let Some(request) = specializer.pending.pop_front() {
        let mut function = specializer.templates[&request.original].clone();
        function.def_id = request.specialized;
        for local in &mut function.locals {
            local.ty = substitute_mut(local.ty, &request.subst, specializer.table);
            if let Some(slot) = unsupported_hash_slot(local.ty, specializer.table) {
                specializer.diagnostics.push(
                    Diagnostic::error(format!("unsupported {slot} type")).with_label(
                        Label::primary(
                            local.span,
                            "expected an integer or String after specialization",
                        ),
                    ),
                );
            }
        }
        for (_, ty) in &mut function.captures {
            *ty = substitute_mut(*ty, &request.subst, specializer.table);
        }
        function.return_type =
            substitute_mut(function.return_type, &request.subst, specializer.table);
        substitute_rvalues(
            &mut function,
            &request.subst,
            specializer.table,
            specializer.diagnostics,
        );
        validate_specialized_comparisons(
            &function,
            specializer.table,
            specializer.definitions,
            specializer.diagnostics,
        );
        specializer.rewrite_calls(&mut function, &request.subst, &request.ancestors);
        mir.functions.insert(request.specialized, function);
    }
}

struct Specialization {
    original: DefId,
    specialized: DefId,
    subst: SubstMap,
    ancestors: Vec<DefId>,
}

struct Specializer<'a> {
    table: &'a mut TypeTable,
    definitions: &'a mut DefinitionTable,
    diagnostics: &'a mut Diagnostics,
    templates: FxHashMap<DefId, MirFunction>,
    calls: FxHashMap<Span, (DefId, SubstMap)>,
    instances: FxHashMap<(DefId, Vec<TypeId>), DefId>,
    pending: VecDeque<Specialization>,
}

impl Specializer<'_> {
    fn rewrite_calls(&mut self, function: &mut MirFunction, subst: &SubstMap, ancestors: &[DefId]) {
        let mut rewrites = FxHashMap::default();
        for block in &function.blocks {
            let Terminator::Call { span, .. } = &block.terminator else {
                continue;
            };
            let Some((callee, callee_subst)) = self.calls.get(span).cloned() else {
                continue;
            };
            if !self.templates.contains_key(&callee) {
                continue;
            }
            let depends_on_caller = callee_subst
                .values()
                .any(|ty| type_involves_param(*ty, self.table));
            let callee_subst = callee_subst
                .into_iter()
                .map(|(param, ty)| (param, substitute_mut(ty, subst, self.table)))
                .collect();
            if let Some(specialized) =
                self.request(callee, callee_subst, ancestors, depends_on_caller, *span)
            {
                rewrites.insert(*span, specialized);
            }
        }
        rewrite_calls(function, &rewrites);
        for block in &mut function.blocks {
            for statement in &mut block.statements {
                let Statement::Assign { rvalue, span, .. } = statement else {
                    continue;
                };
                let (Rvalue::Use(Operand::Constant(Constant::FunctionRef(def)))
                | Rvalue::Aggregate(AggregateKind::Closure { func_def_id: def }, _)) = rvalue
                else {
                    continue;
                };
                if self.templates.contains_key(def)
                    && self
                        .definitions
                        .get(*def)
                        .is_some_and(|info| info.kind == DefinitionKind::Lambda)
                {
                    if let Some(specialized) =
                        self.request(*def, subst.clone(), ancestors, true, *span)
                    {
                        *def = specialized;
                    }
                }
            }
        }
    }

    fn request(
        &mut self,
        original: DefId,
        subst: SubstMap,
        ancestors: &[DefId],
        depends_on_caller: bool,
        span: Span,
    ) -> Option<DefId> {
        let params = self.table.get_generic_params(original)?;
        let type_args: Option<Vec<TypeId>> = params.iter().map(|p| subst.get(p).copied()).collect();
        let Some(type_args) =
            type_args.filter(|args| !args.iter().any(|ty| type_involves_param(*ty, self.table)))
        else {
            self.diagnostics.push(
                Diagnostic::error("generic call remains unresolved after specialization")
                    .with_label(Label::primary(span, "concrete type arguments required")),
            );
            return None;
        };
        let key = (original, type_args);
        if let Some(&specialized) = self.instances.get(&key) {
            return Some(specialized);
        }
        // FIXME [SPARK-138]: Distinguish finite parameter transformations from expanding recursion.
        if depends_on_caller && ancestors.contains(&original) {
            self.diagnostics.push(
                Diagnostic::error("type-changing generic recursion is unsupported").with_label(
                    Label::primary(span, "recursive instantiation changes type arguments"),
                ),
            );
            return None;
        }
        let template = &self.templates[&original];
        let (original_name, kind) = self.definitions.get(original).map_or_else(
            || (spark_intern::sym("__mono"), DefinitionKind::Function),
            |info| (info.name, info.kind.clone()),
        );
        let name = mangle_name(original_name, &key.1, self.table);
        let specialized = self.definitions.insert(DefinitionInfo::new(
            kind,
            name,
            template.span,
            Visibility::Private,
            spark_modules::ModulePath::root(),
        ));
        self.instances.insert(key, specialized);
        let mut ancestors = ancestors.to_vec();
        ancestors.push(original);
        self.pending.push_back(Specialization {
            original,
            specialized,
            subst,
            ancestors,
        });
        Some(specialized)
    }
}

fn validate_specialized_comparisons(
    function: &MirFunction,
    table: &TypeTable,
    definitions: &DefinitionTable,
    diagnostics: &mut Diagnostics,
) {
    use spark_intern::kw;
    use spark_types::check_expr::{list_membership_error, tuple_comparison_error};

    let operand_type = |operand: &Operand| {
        let local = operand_local(operand)?;
        let ty = function.locals[local.index()].ty;
        Some(match table.resolve(ty) {
            Ty::Mutable(inner) => *inner,
            _ => ty,
        })
    };

    for block in &function.blocks {
        for statement in &block.statements {
            let Statement::Assign {
                rvalue: Rvalue::BinaryOp(op, left, right),
                span,
                ..
            } = statement
            else {
                continue;
            };
            if !op.is_comparison() {
                continue;
            }
            let (Some(left), Some(right)) = (operand_type(left), operand_type(right)) else {
                continue;
            };
            if let Some(message) = tuple_comparison_error(*op, left, right, table) {
                diagnostics.push(Diagnostic::error(message).with_label(Label::primary(
                    *span,
                    "unsupported comparison after specialization",
                )));
            }
        }

        let Terminator::Call {
            func, args, span, ..
        } = &block.terminator
        else {
            continue;
        };
        let Some(def) = function
            .resolve_callee_def_id(func)
            .and_then(|id| definitions.get(id))
        else {
            continue;
        };
        if def.kind != DefinitionKind::BuiltinMethod || def.name != kw::CONTAINS {
            continue;
        }
        let [receiver, _] = args.as_slice() else {
            continue;
        };
        let Some(Ty::List(element)) = operand_type(receiver).map(|ty| table.resolve(ty)) else {
            continue;
        };
        if let Some(message) = list_membership_error(*element, table) {
            diagnostics.push(Diagnostic::error(message).with_label(Label::primary(
                *span,
                "unsupported tuple comparison after specialization",
            )));
        }
    }
}

fn unsupported_hash_slot(ty: TypeId, table: &TypeTable) -> Option<&'static str> {
    use spark_types::check_expr::is_unsupported_concrete_hash_slot;

    let invalid = |slot| is_unsupported_concrete_hash_slot(slot, table);
    match table.resolve(ty) {
        Ty::Map(key, _) if invalid(*key) => Some("map key"),
        Ty::Set(element) if invalid(*element) => Some("set element"),
        Ty::Map(_, value)
        | Ty::List(value)
        | Ty::Mutable(value)
        | Ty::Optional(value)
        | Ty::Ref { inner: value, .. } => unsupported_hash_slot(*value, table),
        Ty::Tuple(types) | Ty::Union(types) | Ty::Applied { args: types, .. } => types
            .iter()
            .find_map(|&item| unsupported_hash_slot(item, table)),
        _ => None,
    }
}

/// Substitutes `TypeId`s inside rvalues (casts, etc.) within a function.
fn substitute_rvalues(
    func: &mut MirFunction,
    subst: &SubstMap,
    table: &mut TypeTable,
    diagnostics: &mut Diagnostics,
) {
    for block in &mut func.blocks {
        let mut diverging_cast = None;
        for (index, stmt) in block.statements.iter_mut().enumerate() {
            let Statement::Assign { rvalue, span, .. } = stmt else {
                continue;
            };
            match rvalue {
                Rvalue::Cast { operand, target_ty } => {
                    *target_ty = substitute_mut(*target_ty, subst, table);
                    if let Some(source) = operand_local(operand) {
                        let source_ty = func.locals[source.index()].ty;
                        match cast_kind(source_ty, *target_ty, table) {
                            Ok(CastKind::Diverging) => {
                                diverging_cast = Some(index);
                                break;
                            }
                            Ok(CastKind::Identity) => *rvalue = Rvalue::Use(operand.clone()),
                            Ok(CastKind::Numeric) => {}
                            Ok(CastKind::Deferred) => diagnostics.push(
                                Diagnostic::error("cast remains unresolved after specialization")
                                    .with_label(Label::primary(
                                        *span,
                                        "concrete cast types required",
                                    )),
                            ),
                            Err(message) => {
                                diagnostics.push(Diagnostic::error(message).with_label(
                                    Label::primary(*span, "invalid after specialization"),
                                ));
                            }
                        }
                    }
                }
                Rvalue::Aggregate(
                    AggregateKind::Union {
                        union_ty,
                        member_ty,
                    },
                    _,
                ) => {
                    *union_ty = substitute_mut(*union_ty, subst, table);
                    *member_ty = substitute_mut(*member_ty, subst, table);
                }
                Rvalue::Use(Operand::Copy(place) | Operand::Move(place)) => {
                    for projection in &mut place.projections {
                        if let Projection::UnionPayload(ty) = projection {
                            *ty = substitute_mut(*ty, subst, table);
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(index) = diverging_cast {
            block.statements.truncate(index);
            block.terminator = Terminator::Unreachable;
        }
    }
}

/// Rewrites `FunctionRef` assignments to use monomorphized `DefId`s.
///
/// For each `Terminator::Call` whose span matches a rewrite entry,
/// finds the corresponding `FunctionRef` assignment and replaces the `DefId`.
fn rewrite_calls(func: &mut MirFunction, rewrites: &FxHashMap<Span, DefId>) {
    // First pass: find which locals need rewriting based on call terminators.
    let mut local_rewrites: FxHashMap<crate::mir::Local, DefId> = FxHashMap::default();

    for block in &func.blocks {
        if let Terminator::Call {
            span,
            func: callee_op,
            ..
        } = &block.terminator
        {
            if let Some(&mono_def_id) = rewrites.get(span) {
                if let Some(local) = operand_local(callee_op) {
                    local_rewrites.insert(local, mono_def_id);
                }
            }
        }
    }

    if local_rewrites.is_empty() {
        return;
    }

    // Second pass: rewrite FunctionRef assignments for those locals.
    for block in &mut func.blocks {
        for stmt in &mut block.statements {
            if let Statement::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Constant(Constant::FunctionRef(ref mut def_id))),
                ..
            } = stmt
            {
                if place.projections.is_empty() {
                    if let Some(&mono_def_id) = local_rewrites.get(&place.local) {
                        *def_id = mono_def_id;
                    }
                }
            }
        }
    }
}

/// Extracts the local from an operand, if it's a simple local reference.
const fn operand_local(op: &Operand) -> Option<crate::mir::Local> {
    match op {
        Operand::Copy(place) | Operand::Move(place) if place.projections.is_empty() => {
            Some(place.local)
        }
        _ => None,
    }
}

/// Creates a mangled name for a monomorphized function.
///
/// E.g., `identity` with `[Int]` becomes `identity__Int`.
fn mangle_name(
    base: spark_intern::Symbol,
    type_args: &[TypeId],
    table: &TypeTable,
) -> spark_intern::Symbol {
    use std::fmt::Write;
    let mut name = base.as_str().to_owned();
    name.push_str("__");
    for (i, &arg) in type_args.iter().enumerate() {
        if i > 0 {
            name.push('_');
        }
        let _ = write!(name, "{}", mangle_type(arg, table));
    }
    spark_intern::sym(&name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::{BasicBlock, Local, LocalDecl, Place};
    use spark_types::substitute::build_subst_map;
    use spark_types::{IntWidth, Ty};
    use std::collections::HashMap;

    #[test]
    fn empty_mono_instances_noop() {
        let mut mir = Mir {
            functions: HashMap::new(),
        };
        let mut table = TypeTable::new();
        let mut definitions = DefinitionTable::new();
        monomorphize(
            &mut mir,
            &mut table,
            &mut definitions,
            &mut Diagnostics::new(),
        );
        assert!(mir.functions.is_empty());
    }

    #[test]
    fn unreachable_generic_template_is_removed() {
        let mut table = TypeTable::new();
        let mut definitions = DefinitionTable::new();

        // Create generic param T.
        let t_param_def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::GenericParam,
            spark_intern::sym("T"),
            Span::DUMMY,
            Visibility::Private,
            spark_modules::ModulePath::root(),
        ));
        let t_param_ty = table.intern(Ty::Param(t_param_def));
        let int_ty = table.intern(Ty::Int(IntWidth::I64));

        // Create the generic function: fn identity[T](x: T): T { return x }
        let identity_def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            spark_intern::sym("identity"),
            Span::DUMMY,
            Visibility::Private,
            spark_modules::ModulePath::root(),
        ));
        table.set_generic_params(identity_def, vec![t_param_def]);

        let identity_fn = MirFunction {
            def_id: identity_def,
            param_count: 1,
            return_type: t_param_ty,
            locals: vec![
                LocalDecl {
                    local: Local(0),
                    ty: t_param_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(1),
                    ty: t_param_ty,
                    name: Some(spark_intern::sym("x")),
                    is_user_variable: true,
                    mutable: false,
                    span: Span::DUMMY,
                },
            ],
            blocks: vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(1)))),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };

        let mut mir = Mir {
            functions: HashMap::new(),
        };
        mir.functions.insert(identity_def, identity_fn);

        // Record a mono instance: identity called with T = Int at some call span.
        let call_span = spark_span::Span::new(spark_span::FileId::new(0), 100, 110);
        let subst = build_subst_map(&[t_param_def], &[int_ty]);
        table.set_mono_instance(call_span, identity_def, subst);

        monomorphize(
            &mut mir,
            &mut table,
            &mut definitions,
            &mut Diagnostics::new(),
        );

        // Original generic function should be removed.
        assert!(
            !mir.functions.contains_key(&identity_def),
            "Generic function should be removed after monomorphization"
        );

        assert!(
            mir.functions.is_empty(),
            "Unreachable templates must not be instantiated"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn call_site_rewritten() {
        let mut table = TypeTable::new();
        let mut definitions = DefinitionTable::new();

        let t_param_def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::GenericParam,
            spark_intern::sym("T"),
            Span::DUMMY,
            Visibility::Private,
            spark_modules::ModulePath::root(),
        ));
        let t_param_ty = table.intern(Ty::Param(t_param_def));
        let int_ty = table.intern(Ty::Int(IntWidth::I64));

        // Generic function
        let generic_def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            spark_intern::sym("id"),
            Span::DUMMY,
            Visibility::Private,
            spark_modules::ModulePath::root(),
        ));
        table.set_generic_params(generic_def, vec![t_param_def]);

        let generic_fn = MirFunction {
            def_id: generic_def,
            param_count: 1,
            return_type: t_param_ty,
            locals: vec![
                LocalDecl {
                    local: Local(0),
                    ty: t_param_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(1),
                    ty: t_param_ty,
                    name: None,
                    is_user_variable: true,
                    mutable: false,
                    span: Span::DUMMY,
                },
            ],
            blocks: vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(1)))),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };

        // Caller function: calls id(42)
        let caller_def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            spark_intern::sym("main"),
            Span::DUMMY,
            Visibility::Private,
            spark_modules::ModulePath::root(),
        ));

        let call_span = spark_span::Span::new(spark_span::FileId::new(0), 50, 60);
        let caller_fn = MirFunction {
            def_id: caller_def,
            param_count: 0,
            return_type: int_ty,
            locals: vec![
                LocalDecl {
                    local: Local(0),
                    ty: int_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(1),
                    ty: int_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(2),
                    ty: int_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(3),
                    ty: int_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
            ],
            blocks: vec![
                BasicBlock {
                    statements: vec![
                        Statement::Assign {
                            place: Place::from_local(Local(1)),
                            rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                            span: Span::DUMMY,
                        },
                        Statement::Assign {
                            place: Place::from_local(Local(2)),
                            rvalue: Rvalue::Use(Operand::Constant(Constant::FunctionRef(
                                generic_def,
                            ))),
                            span: Span::DUMMY,
                        },
                    ],
                    terminator: Terminator::Call {
                        func: Operand::Copy(Place::from_local(Local(2))),
                        args: vec![Operand::Copy(Place::from_local(Local(1)))],
                        destination: Place::from_local(Local(3)),
                        target: crate::mir::BlockId(1),
                        span: call_span,
                    },
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(3)))),
                        span: Span::DUMMY,
                    }],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };

        let mut mir = Mir {
            functions: HashMap::new(),
        };
        mir.functions.insert(generic_def, generic_fn);
        mir.functions.insert(caller_def, caller_fn);

        let subst = build_subst_map(&[t_param_def], &[int_ty]);
        table.set_mono_instance(call_span, generic_def, subst);

        monomorphize(
            &mut mir,
            &mut table,
            &mut definitions,
            &mut Diagnostics::new(),
        );

        // Generic should be removed.
        assert!(!mir.functions.contains_key(&generic_def));

        // Caller should still exist.
        let caller = mir.functions.get(&caller_def).expect("caller should exist");

        // Find the FunctionRef assignment — it should NOT reference the generic_def anymore.
        let mut found_rewrite = false;
        for block in &caller.blocks {
            for stmt in &block.statements {
                if let Statement::Assign {
                    rvalue: Rvalue::Use(Operand::Constant(Constant::FunctionRef(def_id))),
                    ..
                } = stmt
                {
                    assert_ne!(
                        *def_id, generic_def,
                        "FunctionRef should be rewritten to monomorphized DefId"
                    );
                    found_rewrite = true;
                }
            }
        }
        assert!(found_rewrite, "Expected to find a rewritten FunctionRef");
    }
}
