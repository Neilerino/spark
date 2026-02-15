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

use rustc_hash::FxHashMap;
use spark_resolve::{DefId, DefinitionInfo, DefinitionKind, DefinitionTable, Visibility};
use spark_span::Span;
use spark_types::substitute::{substitute_mut, SubstMap};
use spark_types::{TypeId, TypeTable};

use crate::mir::{Constant, Mir, MirFunction, Operand, Rvalue, Statement, Terminator};

/// Runs monomorphization on the MIR.
///
/// Clones generic function bodies with concrete type substitutions,
/// rewrites call sites, and removes uninstantiated generic originals.
#[allow(clippy::too_many_lines)]
pub fn monomorphize(mir: &mut Mir, table: &mut TypeTable, definitions: &mut DefinitionTable) {
    if table.mono_instances().is_empty() {
        return;
    }

    // Phase 1: Collect and deduplicate mono instances.
    //
    // Each mono_instance maps call_span → (callee_def_id, subst_map).
    // We deduplicate by (callee_def_id, type_args) so the same generic
    // function instantiated with the same types only gets one copy.
    let mut instance_map: FxHashMap<(DefId, Vec<TypeId>), DefId> = FxHashMap::default();
    let mut mono_requests: Vec<(DefId, Vec<TypeId>, SubstMap)> = Vec::new();

    for (def_id, subst) in table.mono_instances().values() {
        let params = table
            .get_generic_params(*def_id)
            .map(<[_]>::to_vec)
            .unwrap_or_default();
        let type_args: Vec<TypeId> = params.iter().map(|p| subst[p]).collect();
        let key = (*def_id, type_args.clone());
        if let std::collections::hash_map::Entry::Vacant(e) = instance_map.entry(key) {
            // Placeholder DefId — will be filled in phase 2.
            e.insert(DefId::DUMMY);
            mono_requests.push((*def_id, type_args, subst.clone()));
        }
    }

    // Phase 2: Create monomorphized function copies.
    let mut new_functions: Vec<(DefId, MirFunction)> = Vec::new();

    for (original_def_id, type_args, subst) in &mono_requests {
        let Some(source_fn) = mir.functions.get(original_def_id) else {
            continue;
        };
        let mut mono_fn = source_fn.clone();

        // Substitute all TypeIds in locals.
        for local in &mut mono_fn.locals {
            local.ty = substitute_mut(local.ty, subst, table);
        }
        // Substitute the return type.
        mono_fn.return_type = substitute_mut(mono_fn.return_type, subst, table);

        // Substitute TypeIds in rvalues (casts, etc.).
        substitute_rvalues(&mut mono_fn, subst, table);

        // Allocate a fresh DefId for this monomorphized function.
        let original_name = definitions
            .get(*original_def_id)
            .map_or_else(|| spark_intern::sym("__mono"), |info| info.name);
        let mangled_name = mangle_name(original_name, type_args, table);
        let mono_def_id = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            mangled_name,
            mono_fn.span,
            Visibility::Private,
            spark_modules::ModulePath::root(),
        ));
        mono_fn.def_id = mono_def_id;

        instance_map.insert((*original_def_id, type_args.clone()), mono_def_id);
        new_functions.push((mono_def_id, mono_fn));
    }

    // Insert monomorphized functions into the MIR.
    for (def_id, func) in new_functions {
        mir.functions.insert(def_id, func);
    }

    // Phase 3: Build a call-site rewrite map (call_span → mono_def_id).
    let mut call_rewrites: FxHashMap<Span, DefId> = FxHashMap::default();
    for (span, (def_id, subst)) in table.mono_instances() {
        let params = table
            .get_generic_params(*def_id)
            .map(<[_]>::to_vec)
            .unwrap_or_default();
        let type_args: Vec<TypeId> = params.iter().map(|p| subst[p]).collect();
        let key = (*def_id, type_args);
        if let Some(&mono_def_id) = instance_map.get(&key) {
            if !mono_def_id.is_dummy() {
                call_rewrites.insert(*span, mono_def_id);
            }
        }
    }

    // Rewrite call sites in all functions.
    let def_ids: Vec<DefId> = mir.functions.keys().copied().collect();
    for def_id in def_ids {
        if let Some(func) = mir.functions.get_mut(&def_id) {
            rewrite_calls(func, &call_rewrites);
        }
    }

    // Phase 4: Remove original generic functions (they contain Ty::Param).
    let generic_def_ids: Vec<DefId> = mono_requests.iter().map(|(d, _, _)| *d).collect();
    for def_id in generic_def_ids {
        mir.functions.remove(&def_id);
    }
}

/// Substitutes `TypeId`s inside rvalues (casts, etc.) within a function.
fn substitute_rvalues(func: &mut MirFunction, subst: &SubstMap, table: &mut TypeTable) {
    for block in &mut func.blocks {
        for stmt in &mut block.statements {
            if let Statement::Assign {
                rvalue: Rvalue::Cast { target_ty, .. },
                ..
            } = stmt
            {
                *target_ty = substitute_mut(*target_ty, subst, table);
            }
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
        let _ = write!(name, "{}", type_name_short(arg, table));
    }
    spark_intern::sym(&name)
}

/// Returns a short name for a type, used in mangling.
fn type_name_short(ty_id: TypeId, table: &TypeTable) -> String {
    use spark_types::Ty;
    match table.resolve(ty_id) {
        Ty::Int(w) => format!("{w:?}"),
        Ty::Float(w) => format!("{w:?}"),
        Ty::Bool => "Bool".to_owned(),
        Ty::Char => "Char".to_owned(),
        Ty::String => "String".to_owned(),
        Ty::Byte => "Byte".to_owned(),
        Ty::Void => "Void".to_owned(),
        Ty::Struct(d) | Ty::Enum(d) => format!("D{}", d.as_u32()),
        Ty::List(inner) => format!("List_{}", type_name_short(*inner, table)),
        Ty::Optional(inner) => format!("Opt_{}", type_name_short(*inner, table)),
        Ty::Applied { base, args } => {
            let mut s = format!("D{}", base.as_u32());
            for a in args {
                s.push('_');
                s.push_str(&type_name_short(*a, table));
            }
            s
        }
        _ => format!("T{ty_id:?}"),
    }
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
        monomorphize(&mut mir, &mut table, &mut definitions);
        assert!(mir.functions.is_empty());
    }

    #[test]
    fn monomorphize_identity_function() {
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

        monomorphize(&mut mir, &mut table, &mut definitions);

        // Original generic function should be removed.
        assert!(
            !mir.functions.contains_key(&identity_def),
            "Generic function should be removed after monomorphization"
        );

        // Should have exactly one monomorphized function.
        assert_eq!(mir.functions.len(), 1, "Expected one monomorphized function");

        let mono_fn = mir.functions.values().next().unwrap();
        // All locals should have concrete types (not Ty::Param).
        for local in &mono_fn.locals {
            let ty = table.resolve(local.ty);
            assert!(
                !matches!(ty, Ty::Param(_)),
                "Local {} still has Ty::Param after monomorphization",
                local.local
            );
        }
        // Return type should be Int.
        assert_eq!(mono_fn.return_type, int_ty);
    }

    #[test]
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
                LocalDecl { local: Local(0), ty: t_param_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(1), ty: t_param_ty, name: None, is_user_variable: true, mutable: false, span: Span::DUMMY },
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
                LocalDecl { local: Local(0), ty: int_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(1), ty: int_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(2), ty: int_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(3), ty: int_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
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
                            rvalue: Rvalue::Use(Operand::Constant(Constant::FunctionRef(generic_def))),
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

        monomorphize(&mut mir, &mut table, &mut definitions);

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
