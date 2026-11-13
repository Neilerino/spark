// TAG [SPARK-114]

use super::*;
use crate::facts::Point;
use crate::provenance::{
    tasks::{fresh::Birth, Record},
    State,
};
use spark_mir::{BlockId, Local, Operand, Terminator};

mod conditional;
mod message_roles;

fn with_fresh_producer(run: impl FnOnce(&mut Flow<'_>, State, Local, Birth)) {
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_retained_child_memory_scoped.spark"
    ));
    let output = lower_source(source);
    assert!(!output.diagnostics.has_errors());
    let facts = output
        .mir
        .functions
        .iter()
        .map(|(id, function)| (*id, crate::facts::extract_facts(function)))
        .collect();
    let (summaries, ports, proofs) = infer_summaries_with_definitions(
        &output.mir,
        &output.table,
        &facts,
        Some(&output.definitions),
    );
    let context = Context {
        mir: &output.mir,
        summaries: &summaries,
        ports: &ports,
        definitions: Some(&output.definitions),
        proofs: Some(&proofs),
    };
    let function = output
        .mir
        .functions
        .values()
        .find(|function| {
            output
                .definitions
                .get(function.def_id)
                .unwrap()
                .name
                .as_str()
                == "retainChild"
        })
        .unwrap();
    let mut flow = Flow::new(function, &output.table, Some(context));
    flow.solve(&facts[&function.def_id]);
    let (index, child) = function
        .blocks
        .iter()
        .enumerate()
        .find_map(|(index, block)| {
            let Terminator::Call { func, args, .. } = &block.terminator else {
                return None;
            };
            let info = output
                .definitions
                .get(function.resolve_callee_def_id(func)?)?;
            if info.name != spark_intern::kw::SEND {
                return None;
            }
            let (Operand::Copy(place) | Operand::Move(place)) = args.get(1)? else {
                return None;
            };
            matches!(
                output
                    .table
                    .resolve(crate::provenance::value::strip_mutable(
                        function.locals[place.local.index()].ty,
                        &output.table
                    )),
                spark_types::Ty::Future { .. }
            )
            .then_some((index, place.local))
        })
        .unwrap();
    let block = function.blocks[index].clone();
    let mut state = flow.entries[index]
        .clone()
        .expect("actual fresh child send reachable");
    for (step, statement) in block.statements.iter().enumerate() {
        flow.transfer_statement(
            statement,
            Point::new(BlockId(index as u32), step as u32),
            &mut state,
        );
    }
    let birth = flow
        .current_leaf_birth(child, &state)
        .expect("the child's actual typed spawn must provide fresh birth provenance");
    assert_eq!(
        birth.site.function, function.def_id,
        "the child is internally born, never a formal"
    );
    assert!(state.future_copies.inputs(child).is_empty());
    assert!(
        state.current_execution_record(child).is_none(),
        "plain leaf birth must not introduce a general execution facet"
    );
    run(&mut flow, state, child, birth);
}

#[test]
fn an_internal_leaf_birth_reaches_its_message_after_the_sender_local_drops() {
    with_fresh_producer(|flow, state, child, birth| {
        check_birth_authority(flow, &state, child, birth);
        let mut checked = 0;
        for (index, block) in flow.function.blocks.clone().iter().enumerate() {
            if !matches!(block.terminator, Terminator::Return) {
                continue;
            }
            let Some(mut state) = flow.entries[index].clone() else {
                continue;
            };
            assert!(flow.transfer_block(BlockId(index as u32), &mut state));
            assert!(!state.live.contains(&child));
            let messages: Vec<_> = state
                .queue_current
                .values()
                .filter(|message| message.births.contains(&birth))
                .collect();
            assert_eq!(
                messages.len(),
                1,
                "actual send must preserve the fresh output's origin"
            );
            let message = messages[0];
            assert!(!message.value.origins().is_empty());
            let view = message.value.task_views[&Vec::new()]
                .values()
                .next()
                .unwrap();
            let callback = view.callbacks[&birth.callback_ty].value();
            assert!(!view.fallback);
            assert!(
                !callback.origins().is_empty(),
                "full callback borrow backing survives sender drop"
            );
            assert!(flow.leaf_task(&Record {
                callback,
                callback_ty: birth.callback_ty
            }));
            checked += 1;
        }
        assert!(checked > 0);
    });
}

fn check_birth_authority(flow: &Flow<'_>, state: &State, child: Local, birth: Birth) {
    use crate::provenance::{tasks::Role, Value};
    let mut poisoned = state.clone();
    assert!(!poisoned.tasks.is_empty());
    for record in poisoned.tasks.values_mut() {
        record.callback = Value::function(DefId::new(9998));
    }
    assert_eq!(
        flow.current_leaf_birth(child, &poisoned),
        Some(birth),
        "catalog data does not supply actual callback completeness"
    );
    let fake = flow
        .function
        .locals
        .iter()
        .find(|decl| {
            decl.local != child
                && crate::provenance::value::strip_mutable(decl.ty, flow.table) == birth.future_ty
        })
        .unwrap()
        .local;
    let mut forged = state.clone();
    forged.set(fake, state.value(child));
    forged.future_copies.forget(fake);
    forged.future_owners.grant(fake, &forged.future_copies);
    assert_eq!(forged.value(fake), state.value(child));
    assert!(forged.future_owners.known(fake));
    assert!(
        flow.current_leaf_birth(fake, &forged).is_none(),
        "identical MAY values and an owner flag do not establish an actual birth"
    );
    let mut alternative = state.clone();
    let mut witness = alternative.future_copies.witness(child);
    witness.birth = None;
    alternative
        .future_copies
        .assign(child, witness, &alternative.live);
    assert_eq!(state.values, alternative.values);
    assert_eq!(
        state.future_copies.role_peers(&Role::root(child)),
        alternative.future_copies.role_peers(&Role::root(child))
    );
    let mut joined = state.clone();
    assert!(
        joined.join(&alternative),
        "birth-only disagreement must wake Flow"
    );
    assert!(flow.current_leaf_birth(child, &joined).is_none());
    let mut clobbered = state.clone();
    flow.clobber_input_owners(child, &mut clobbered);
    assert_eq!(clobbered.value(child), state.value(child));
    assert!(flow.current_leaf_birth(child, &clobbered).is_none());
    let mut copies = state.future_copies.clone();
    let mut live = state.live.clone();
    live.insert(fake);
    copies.assign(fake, copies.witness(child), &live);
    copies.seed_birth(child, birth);
    assert_eq!(copies.witness(child).birth, copies.witness(fake).birth);
    assert!(
        copies
            .witness(child)
            .peers
            .is_disjoint(&copies.witness(fake).peers),
        "reusing a constructor label is not current object equality"
    );
}

#[test]
fn late_spawn_and_send_imports_cannot_restore_fresh_birth_provenance() {
    use crate::provenance::{
        address::OpaqueSite,
        observations::{FrameScope, Memory, Sample},
        tasks::{birth as task_birth, memory::Lane},
        Value,
    };
    with_fresh_producer(|flow, before_send, child, birth| {
        let body = flow.function.blocks[birth.site.point.block.index()].clone();
        let Terminator::Call {
            func,
            args,
            destination,
            ..
        } = body.terminator
        else {
            panic!("actual birth terminator");
        };
        let Operand::Move(source) = &args[0] else {
            panic!("typed moving callback");
        };
        let source = source.local;
        let other = flow
            .function
            .locals
            .iter()
            .find(|decl| decl.name.is_some_and(|name| name.as_str() == "holding"))
            .unwrap()
            .local;
        for overwritten in [Some(source), Some(destination.local), None, Some(other)] {
            let mut state = flow.entries[birth.site.point.block.index()]
                .clone()
                .unwrap();
            for (step, statement) in body.statements.iter().enumerate() {
                flow.transfer_statement(
                    statement,
                    Point::new(birth.site.point.block, step as u32),
                    &mut state,
                );
            }
            let point = birth.site.point;
            flow.transfers.remove(&point);
            flow.ensure_transfer(point, &state);
            let ty = flow.place_type(&destination);
            let mut dispatch = flow.call_result(&func, &args, ty, point, &state);
            assert_eq!(dispatch.returning.len(), 1);
            let mut outcome = dispatch.returning.pop().unwrap();
            assert!(outcome.spawn_return.is_some());
            let actor = outcome
                .returned
                .task_views
                .get_mut(&Vec::new())
                .unwrap()
                .iter_mut()
                .next()
                .unwrap();
            let ty = flow.function.locals[overwritten.unwrap_or(destination.local).index()].ty;
            let memory =
                overwritten.map_or(Memory::Unavailable(birth.site), |local| Memory::Frame {
                    scope: FrameScope::Current,
                    function: flow.function.def_id,
                    local,
                    ty,
                });
            let payload = if overwritten == Some(source) {
                Value::function(birth.callback)
            } else if overwritten == Some(other) {
                crate::provenance::channels::allocation(flow.function.def_id, point, ty)
            } else {
                task_birth(flow.function.def_id, point, ty).1
            };
            actor.1.memory.import(
                std::sync::Arc::new(Lane {
                    event: birth.site,
                    actor: actor.0.clone(),
                    location: Sample {
                        memory,
                        path: Vec::new(),
                        ty,
                    },
                }),
                payload,
            );
            flow.apply_call_outcomes(&args, &destination, vec![outcome], point, &mut state);
            assert!(state
                .invalidated_copies
                .contains(&overwritten.unwrap_or(source)));
            assert_eq!(
                state
                    .future_copies
                    .witness(destination.local)
                    .birth
                    .is_some(),
                overwritten == Some(other),
                "denied spawn origin must not be stored behind a temporary invalidation flag"
            );
            assert_eq!(flow.current_leaf_birth(destination.local, &state).is_some(), overwritten == Some(other), "post-snapshot spawn imports must deny source/destination/unavailable, not unrelated writes");
        }
        let (index, block) = flow.function.blocks.iter().enumerate().find(|(_, block)| matches!(&block.terminator,
            Terminator::Call { args, .. } if matches!(args.get(1), Some(Operand::Copy(place) | Operand::Move(place)) if place.local == child))).unwrap();
        let block = block.clone();
        let Terminator::Call {
            func,
            args,
            destination,
            ..
        } = block.terminator
        else {
            unreachable!()
        };
        let point = Point::new(BlockId(index as u32), block.statements.len() as u32);
        let mut state = before_send;
        flow.transfers.remove(&point);
        flow.ensure_transfer(point, &state);
        let mut dispatch =
            flow.call_result(&func, &args, flow.place_type(&destination), point, &state);
        let mut outcome = dispatch.returning.pop().unwrap();
        let value = &mut outcome.queues[0].write.value;
        let (actor, view) = value
            .task_views
            .get_mut(&Vec::new())
            .unwrap()
            .iter_mut()
            .next()
            .unwrap();
        let ty = flow.function.locals[child.index()].ty;
        view.memory.import(
            std::sync::Arc::new(Lane {
                event: OpaqueSite {
                    function: flow.function.def_id,
                    point,
                },
                actor: actor.clone(),
                location: Sample {
                    memory: Memory::Frame {
                        scope: FrameScope::Current,
                        function: flow.function.def_id,
                        local: child,
                        ty,
                    },
                    path: Vec::new(),
                    ty,
                },
            }),
            task_birth(flow.function.def_id, point, ty).1,
        );
        flow.apply_call_outcomes(&args, &destination, vec![outcome], point, &mut state);
        assert!(state.invalidated_copies.contains(&child));
        let (slot, message) = state
            .queue_current
            .iter()
            .find(|(_, message)| birth.record(&message.value).is_some())
            .expect("the real queued callback backing remains present");
        assert!(
            message.births.is_empty(),
            "send origin snapshot cannot survive its source's late physical clobber"
        );
        assert!(
            state
                .future_owners
                .coverage(&crate::provenance::tasks::Role::message(slot.clone()))
                .is_none(),
            "a late clobber cannot restore even conditional coverage"
        );
        assert!(!message.value.origins().is_empty());
    });
}

#[test]
fn an_internally_born_message_preserves_conditional_owner_evidence() {
    with_fresh_producer(|flow, _, child, birth| {
        let mut checked = 0;
        for (index, block) in flow.function.blocks.clone().iter().enumerate() {
            if !matches!(block.terminator, Terminator::Return) {
                continue;
            }
            let Some(mut state) = flow.entries[index].clone() else {
                continue;
            };
            assert!(flow.transfer_block(BlockId(index as u32), &mut state));
            assert!(!state.live.contains(&child));
            let (slot, message) = state
                .queue_current
                .iter()
                .find(|(_, message)| message.births.contains(&birth))
                .unwrap();
            let role = crate::provenance::tasks::Role::message(slot.clone());
            assert!(
                !state.future_owners.known_role(&role),
                "a formal queue never supplies unconditional cleanup authority"
            );
            assert_eq!(state.future_copies.role_witness(&role).birth, Some(birth), "the actual sent child must retain its conditional output identity after the sender drops");
            let retained = flow
                .function
                .locals
                .iter()
                .find(|decl| decl.name.is_some_and(|name| name.as_str() == "retained"))
                .unwrap()
                .local;
            let input = flow
                .initial_state()
                .channel_copies
                .witness(&crate::provenance::channels::identity::Role::Local(
                    retained,
                ))
                .input
                .unwrap();
            assert_eq!(
                state.future_owners.coverage(&role),
                Some(crate::provenance::tasks::lifecycle::Coverage::ThroughQueue(
                    input
                ))
            );
            assert_eq!(
                state.future_copies.role_witness(&role).peers,
                FxHashSet::from_iter([role.clone()])
            );
            assert!(state.input_owners.tags(&role).is_empty());
            assert!(!message.value.origins().is_empty());
            assert!(!state.residuals.values().any(|value| value.futures.values().flatten().any(|handle| matches!(&handle.target, crate::provenance::tasks::Target::Birth { constructor, .. } if *constructor == birth.site))), "the represented conditional output retains activity without duplicate loss debt");
            checked += 1;
        }
        assert!(checked > 0);
    });
}

#[test]
fn a_fresh_queue_output_participates_in_proofs_without_future_formals() {
    with_fresh_producer(|flow, _, _, _| {
        assert!(flow
            .function
            .locals
            .iter()
            .skip(1)
            .take(flow.function.param_count as usize)
            .all(|decl| !matches!(
                flow.table
                    .resolve(crate::provenance::value::strip_mutable(decl.ty, flow.table)),
                spark_types::Ty::Future { .. }
            )));
        let proofs = flow.program.unwrap().proofs.unwrap();
        let returning: Vec<_> = proofs
            .iter()
            .filter(|(key, proof)| {
                key.function == flow.function.def_id && proof.certificate().is_some()
            })
            .collect();
        assert!(!returning.is_empty());
        for (key, proof) in returning {
            assert!(proof.has_effect(), "a normal-return fresh queue output must enter the existing proof solver even without a Future formal");
            let recipes = &proof.certificate().unwrap().fresh_messages;
            assert_eq!(recipes.len(), 1);
            let recipe = recipes.iter().next().unwrap();
            assert_eq!(recipe.send.function, flow.function.def_id);
            assert_eq!(recipe.birth.site.function, flow.function.def_id);
            assert_eq!(
                flow.function.locals[recipe.queue.input.index()]
                    .name
                    .unwrap()
                    .as_str(),
                "retained"
            );
            let payload = &flow.program.unwrap().summaries[key].fresh_outputs[recipe];
            let record = recipe
                .birth
                .record(payload)
                .expect("the guaranteed output retains its actual closed callback");
            assert!(!record.callback.origins().is_empty());
            assert!(flow.leaf_task(&record));
        }
    });
}

#[test]
fn a_known_wrapper_binds_its_producers_fresh_queue_recipe() {
    with_fresh_producer(|flow, _, _, _| {
        let context = flow.program.unwrap();
        let producer = flow.function.def_id;
        let wrappers: FxHashSet<_> = context.mir.functions.values().filter(|function| function.def_id != producer && function.blocks.iter().any(|block| matches!(&block.terminator, Terminator::Call { func, .. } if function.resolve_callee_def_id(func) == Some(producer)))).map(|function| function.def_id).collect();
        assert!(
            !wrappers.is_empty(),
            "the actual spawn callback calls the producer through a known wrapper"
        );
        let returning: Vec<_> = context
            .proofs
            .unwrap()
            .iter()
            .filter(|(key, proof)| {
                wrappers.contains(&key.function) && proof.certificate().is_some()
            })
            .collect();
        assert!(!returning.is_empty());
        for (key, proof) in returning {
            let certificate = proof.certificate().unwrap();
            assert_eq!(certificate.fresh_messages.len(), 1, "the actual known wrapper must bind and export its producer's conditional fresh queue output");
            let recipe = certificate.fresh_messages.iter().next().unwrap();
            assert!(certificate.prefix_messages.contains(recipe));
            assert_eq!(recipe.birth.site.function, producer);
            let payload = &context.summaries[key].fresh_outputs[recipe];
            let record = recipe.birth.record(payload).unwrap();
            assert!(!record.callback.origins().is_empty());
            assert!(flow.leaf_task(&record));
            let function = &context.mir.functions[&key.function];
            let mut wrapper = Flow::new(function, flow.table, Some(context));
            wrapper.aliases = key.aliases.clone();
            wrapper.callables = key.callables.clone();
            wrapper.solve(&crate::facts::extract_facts(function));
            let mut exits = 0;
            for (index, block) in function.blocks.iter().enumerate() {
                if !matches!(block.terminator, Terminator::Return) {
                    continue;
                }
                let Some(mut state) = wrapper.entries[index].clone() else {
                    continue;
                };
                assert!(wrapper.transfer_block(BlockId(index as u32), &mut state));
                let (slot, message) = state
                    .queue_current
                    .iter()
                    .find(|(slot, _)| slot.site == recipe.send)
                    .unwrap();
                let role = crate::provenance::tasks::Role::message(slot.clone());
                assert_eq!(
                    state.future_owners.coverage(&role),
                    Some(crate::provenance::tasks::lifecycle::Coverage::ThroughQueue(
                        recipe.queue.clone()
                    ))
                );
                assert!(!state.future_owners.known_role(&role));
                assert_eq!(
                    state.future_copies.role_witness(&role).birth,
                    Some(recipe.birth)
                );
                assert!(recipe.birth.record(&message.value).is_some());
                exits += 1;
            }
            assert!(exits > 0);
        }
    });
}
