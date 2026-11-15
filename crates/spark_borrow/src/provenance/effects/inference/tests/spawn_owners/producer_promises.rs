// TAG [SPARK-114]

use super::keeper_activity::with_source_flow;
use super::*;

fn source(before: &str, after: &str, end: &str) -> String {
    format!("struct Box {{ queue: Channel[Future[Int]] }}
async fn work(): Int {{ return 7 }}
async fn emit(out: Channel[Future[Int]]): Int {{ if true {{ child: Future[Int] = spawn {{ work() }} out.send(child) }} return 0 }}
async fn consume(out: Channel[Future[Int]]): Int {{ child: Future[Int] = out.recv() return 0 }}
fn main(): Int {{ out: Channel[Future[Int]] = Channel[Future[Int]]() {before} parent: Future[Int] = spawn {{ emit(out) }} {after} marker: Int = 0 {end} return 0 }}")
}

fn with_marker(source: &str, run: impl FnOnce(&mut Flow<'_>, State, Local, Local, Point)) {
    with_source_flow(source, |flow| {
        let named = |name: &str| {
            flow.function
                .locals
                .iter()
                .find(|decl| decl.name.is_some_and(|symbol| symbol.as_str() == name))
                .unwrap()
                .local
        };
        let marker = named("marker");
        let parent = named("parent");
        let out = named("out");
        let (index, block) = flow.function.blocks.iter().enumerate().find(|(_, block)| block.statements.iter().any(|statement| matches!(statement, Statement::Assign { place, .. } if place.local == marker))).unwrap();
        let statements = block.statements.clone();
        let mut state = flow.entries[index].clone().expect("marker is reachable");
        let mut point = Point::new(BlockId(index as u32), 0);
        for (step, statement) in statements.iter().enumerate() {
            point = Point::new(BlockId(index as u32), step as u32);
            flow.transfer_statement(statement, point, &mut state);
            if matches!(statement, Statement::Assign { place, .. } if place.local == marker) {
                break;
            }
        }
        run(flow, state, out, parent, point);
    });
}

#[test]
fn producer_promises_require_exclusive_entry_and_survive_only_accounted_access() {
    for (before, after, end, expected) in [
        ("", "", "", true),
        (
            "",
            "copy: Future[Int] = parent",
            "joined: Int = await copy",
            true,
        ),
        ("", "copy: Channel[Future[Int]] = out", "copy.close()", true),
        ("borrowed: Ref[Channel[Future[Int]]] = out", "", "", false),
        (
            "box: Box = Box { queue: out }",
            "",
            "box.queue.close()",
            false,
        ),
        (
            "consumer: Future[Int] = spawn { consume(out) }",
            "",
            "",
            false,
        ),
        ("", "out.close()", "", false),
        ("", "received: Future[Int] = out.recv()", "", false),
        ("", "second: Future[Int] = spawn { emit(out) }", "", false),
        ("", "value: String = \" x \".trim()", "", true),
        (
            "",
            "box: Box = Box { queue: out }",
            "box.queue.close()",
            false,
        ),
    ] {
        let source = source(before, after, end);
        with_marker(&source, |flow, state, out, parent, _| {
            assert_eq!(!state.queue_promises.is_empty(), expected, "{source}");
            for slot in state.queue_promises.keys() {
                assert!(state.current_output_queue(slot).is_some());
                assert!(!state.future_owners.known_role(&Role::message(slot.clone())));
                assert!(state.future_copies.produces(parent).contains(slot));
                assert!(!state.queue_current[slot].value.futures.is_empty());
            }
            if !expected && before.is_empty() {
                let target = state
                    .value(out)
                    .root_queue_handles()
                    .next()
                    .unwrap()
                    .target
                    .clone();
                assert!(state.queues.contains_key(&(
                    target,
                    match flow.table.resolve(flow.function.locals[out.index()].ty) {
                        Ty::Channel { inner } => *inner,
                        _ => panic!("typed channel"),
                    }
                )));
            }
        });
    }
}

#[test]
fn producer_promise_survives_normal_parent_join_and_exposure_archives_its_borrowed_payload() {
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_retained_child_memory_scoped.spark"
    ));
    with_source_flow(source, |flow| {
        let retained = flow
            .function
            .locals
            .iter()
            .find(|decl| decl.name.is_some_and(|name| name.as_str() == "retained"))
            .unwrap()
            .local;
        let (index, block) = flow.function.blocks.iter().enumerate().find(|(_, block)| {
            matches!(&block.terminator, Terminator::Call {func,args,..}
                if flow.function.resolve_callee_def_id(func).and_then(|id| flow.program.unwrap().definitions.unwrap().get(id)).is_some_and(|info| info.name == spark_intern::kw::RECV)
                && matches!(args.first(), Some(Operand::Copy(place) | Operand::Move(place)) if place.local == retained))
        }).unwrap();
        let statements = block.statements.clone();
        let mut state = flow.entries[index]
            .clone()
            .expect("the exact receive is reachable after normal parent await");
        for (step, statement) in statements.iter().enumerate() {
            flow.transfer_statement(
                statement,
                Point::new(BlockId(index as u32), step as u32),
                &mut state,
            );
        }
        assert_eq!(
            state.queue_promises.len(),
            1,
            "normal parent completion preserves its exact output association"
        );
        let (slot, promise) = state.queue_promises.iter().next().unwrap();
        let slot = slot.clone();
        let recipe = promise.recipe.clone();
        let target = promise.queue.clone();
        let message = state.queue_current[&slot].value.clone();
        assert!(!message.origins().is_empty());
        assert!(recipe.birth.record(&message).is_some());
        assert!(
            state.future_owners.known_role(&Role::message(slot.clone())),
            "normal producer completion discharges the exclusive output premise"
        );
        let queue = state.value(retained);
        flow.expose_queue_handles(&queue, &mut state);
        let point = Point::new(BlockId(index as u32), statements.len() as u32);
        flow.validate_queue_promises(point, &mut state);
        assert!(state.queue_promises.is_empty());
        assert!(!state.queue_current.contains_key(&slot));
        assert!(state.channel_copies.output_roles(&slot).is_empty());
        assert!(state.future_copies.producer_roles(&slot).is_empty());
        let archived = &state.queues[&(target, recipe.birth.future_ty)];
        assert!(message.origins().is_subset(&archived.origins()));
        assert!(
            recipe.birth.record(archived).is_some(),
            "cancellation preserves the full pending callback, not merely a birth label"
        );
    });
}

#[test]
fn producer_promise_metadata_meets_wake_flow_and_never_reopen_from_may_values() {
    with_marker(&source("", "", ""), |flow, state, _, parent, point| {
        let slot = state.queue_promises.keys().next().unwrap().clone();
        let mut pending = state.queue_promises.clone();
        let mut present = pending.clone();
        present.get_mut(&slot).unwrap().present = true;
        assert!(crate::provenance::channels::protocol::intersect(
            &mut present,
            &pending
        ));
        assert!(!present[&slot].present);
        let mut present = pending.clone();
        present.get_mut(&slot).unwrap().present = true;
        assert!(!crate::provenance::channels::protocol::intersect(
            &mut pending,
            &present
        ));
        assert!(!pending[&slot].present);
        for lose_queue in [false, true] {
            let mut missing = state.clone();
            if lose_queue {
                missing.channel_copies.forget_output(&slot);
            } else {
                missing.future_copies.forget_output(&slot);
            }
            assert_eq!(state.values, missing.values);
            assert_eq!(state.queue_current, missing.queue_current);
            let mut joined = state.clone();
            assert!(
                joined.join(&missing),
                "metadata-only disagreement must wake Flow"
            );
            flow.validate_queue_promises(point, &mut joined);
            assert!(joined.queue_promises.is_empty());
            assert!(joined.future_copies.produces(parent).is_empty());
            assert!(
                !joined.queues.is_empty(),
                "loss of either current relation preserves weak activity"
            );
        }
    });
}

#[test]
fn an_unrelated_complete_producer_with_the_same_may_target_cannot_route_a_message() {
    use crate::provenance::{channels::QueueEvent, demand::Storage};
    with_marker(&source("", "", ""), |flow, mut state, _, parent, point| {
        let (slot, promise) = state.queue_promises.iter().next().unwrap();
        let slot = slot.clone();
        let promise = promise.clone();
        let fake = flow
            .function
            .locals
            .iter()
            .find(|decl| decl.local != parent && decl.ty == flow.function.locals[parent.index()].ty)
            .unwrap()
            .local;
        let record = state.current_execution_record(parent).unwrap();
        let value = state.value(parent);
        state.set(fake, value.clone());
        state.future_copies.forget(fake);
        state.install_execution_callback(fake, record.callback_ty, record.callback, true);
        assert!(state.current_execution_record(fake).is_some());
        assert_eq!(state.value(fake).futures, value.futures);
        assert!(state.future_copies.produces(fake).is_empty());
        let handle = value.futures[&Vec::new()].iter().next().unwrap();
        let mut event = QueueEvent {
            site: promise.recipe.send,
            write: crate::provenance::channels::Write {
                target: promise.queue.clone(),
                ty: slot.ty,
                value: state.queue_current[&slot].value.clone(),
            },
            fence: Default::default(),
        };
        assert!(!flow.route_producer_queue_event(&Storage::Logical(fake), &[], handle, &mut event, point, &mut state),
            "complete callback data and the same static target do not provide current producer identity");
        flow.apply_queue_event(&event, point, &mut state);
        assert!(state.queue_promises.is_empty());
        assert!(state
            .queues
            .values()
            .any(|value| promise.recipe.birth.record(value).is_some()));
    });
}

#[test]
fn producer_reservations_revalidate_copy_move_and_late_imported_clobbers() {
    use crate::provenance::{
        address::OpaqueSite,
        observations::{FrameScope, Memory, Sample},
        tasks::memory::Lane,
    };
    with_source_flow(&source("", "", ""), |flow| {
        let named = |name: &str| {
            flow.function
                .locals
                .iter()
                .find(|decl| decl.name.is_some_and(|symbol| symbol.as_str() == name))
                .unwrap()
                .local
        };
        let out = named("out");
        let unrelated = named("marker");
        let (index, block) = flow.function.blocks.iter().enumerate().find(|(_, block)| {
            matches!(&block.terminator, Terminator::Call {func,..} if flow.function.resolve_callee_def_id(func).and_then(|id| flow.program.unwrap().definitions.unwrap().get(id)).is_some_and(|info| info.name == spark_intern::kw::SPARK_SPAWN))
        }).unwrap();
        let block = block.clone();
        let Terminator::Call {
            func,
            args,
            destination,
            ..
        } = &block.terminator
        else {
            unreachable!()
        };
        let (Operand::Move(callback) | Operand::Copy(callback)) = &args[0] else {
            unreachable!()
        };
        let callback = callback.clone();
        let point = Point::new(BlockId(index as u32), block.statements.len() as u32);
        let mut before = flow.entries[index].clone().unwrap();
        for (step, statement) in block.statements.iter().enumerate() {
            flow.transfer_statement(
                statement,
                Point::new(BlockId(index as u32), step as u32),
                &mut before,
            );
        }
        for moved in [false, true] {
            for (overwritten, imported) in [
                (None, false),
                (None, true),
                (Some(out), false),
                (Some(callback.local), false),
                (Some(destination.local), false),
                (Some(unrelated), false),
                (Some(out), true),
                (Some(callback.local), true),
                (Some(destination.local), true),
                (Some(unrelated), true),
            ] {
                let mut state = before.clone();
                let mut args = args.clone();
                args[0] = if moved {
                    Operand::Move(callback.clone())
                } else {
                    Operand::Copy(callback.clone())
                };
                flow.transfers.remove(&point);
                flow.ensure_transfer(point, &state);
                let mut dispatch =
                    flow.call_result(func, &args, flow.place_type(destination), point, &state);
                assert_eq!(dispatch.returning.len(), 1);
                let mut outcome = dispatch.returning.pop().unwrap();
                assert_eq!(outcome.spawn_outputs.len(), 1);
                if imported {
                    let local = overwritten.unwrap_or(destination.local);
                    let ty = flow.function.locals[local.index()].ty;
                    let payload = if local == out {
                        crate::provenance::channels::allocation(flow.function.def_id, point, ty)
                    } else if local == callback.local {
                        before.value(callback.local)
                    } else if local == destination.local {
                        crate::provenance::tasks::birth(flow.function.def_id, point, ty).1
                    } else {
                        Value::default()
                    };
                    outcome.spawn_outputs[0].visit_values(&mut |value| {
                        let Some(views) = value.task_views.get_mut(&Vec::new()) else {
                            return;
                        };
                        let Some((actor, view)) = views.iter_mut().next() else {
                            return;
                        };
                        view.memory.import(
                            std::sync::Arc::new(Lane {
                                execution: crate::provenance::effects::Execution::Current,
                                event: OpaqueSite {
                                    function: flow.function.def_id,
                                    point,
                                },
                                actor: actor.clone(),
                                location: Sample {
                                    memory: overwritten.map_or(
                                        Memory::Unavailable(OpaqueSite {
                                            function: flow.function.def_id,
                                            point,
                                        }),
                                        |local| Memory::Frame {
                                            scope: FrameScope::Current,
                                            function: flow.function.def_id,
                                            local,
                                            ty,
                                        },
                                    ),
                                    path: Vec::new(),
                                    ty,
                                },
                            }),
                            payload.clone(),
                        );
                    });
                } else if let Some(local) = overwritten {
                    outcome.clobbered_copies.push(local);
                }
                flow.apply_call_outcomes(&args, destination, vec![outcome], point, &mut state);
                let expected = moved
                    && (!imported || overwritten.is_some())
                    && overwritten.is_none_or(|local| local == unrelated);
                assert_eq!(
                    !state.queue_promises.is_empty(),
                    expected,
                    "moved {moved}, overwritten {overwritten:?}, imported {imported}"
                );
                if imported {
                    assert!(
                        state
                            .invalidated_copies
                            .contains(&overwritten.unwrap_or(callback.local)),
                        "the memory import must actually clobber its typed frame"
                    );
                }
                if !expected {
                    assert!(
                        state.queues.values().any(|value| value
                            .futures
                            .values()
                            .flatten()
                            .any(|handle| handle.pending)),
                        "failed reservations retain pending weak callback data"
                    );
                }
                assert!(state
                    .queue_current
                    .keys()
                    .all(|slot| !state.future_owners.known_role(&Role::message(slot.clone()))));
            }
        }

        let context = flow.program.unwrap();
        flow.program = Some(Context {
            proofs: None,
            ..context
        });
        let mut state = before.clone();
        flow.transfers.remove(&point);
        flow.ensure_transfer(point, &state);
        let mut dispatch =
            flow.call_result(func, args, flow.place_type(destination), point, &state);
        let outcome = dispatch.returning.pop().unwrap();
        assert!(
            outcome.spawn_outputs.is_empty(),
            "the same MAY summaries cannot reserve without a certified recipe"
        );
        flow.apply_call_outcomes(args, destination, vec![outcome], point, &mut state);
        assert!(state.queue_promises.is_empty());
        assert!(state.queues.values().any(|value| value
            .futures
            .values()
            .flatten()
            .any(|handle| handle.pending)));
    });
}

#[test]
fn producer_reservations_reject_zero_many_and_nonreturning_outputs() {
    for (body, has_prefix) in [
        ("", false),
        ("out.send(child) out.send(child)", true),
        ("out.send(child) } loop {", true),
    ] {
        let source = source("", "", "").replace("out.send(child)", body);
        with_marker(&source, |_, state, _, _, _| {
            assert!(state.queue_promises.is_empty(), "{source}");
            if has_prefix {
                assert!(
                    state.queues.values().any(|value| value
                        .futures
                        .values()
                        .flatten()
                        .any(|handle| handle.pending)),
                    "executed prefixes survive without a normal output certificate"
                );
            }
        });
    }
}

#[test]
fn producer_reservations_do_not_reopen_when_a_queue_constructor_label_is_reused() {
    with_marker(&source("", "", ""), |flow, mut state, out, _, _| {
        let queue = state.value(out);
        let crate::provenance::channels::Target::Allocation { point, .. } =
            queue.root_queue_handles().next().unwrap().target
        else {
            unreachable!()
        };
        flow.transfer_statement(
            &Statement::Assign {
                place: Place::from_local(out),
                rvalue: Rvalue::Aggregate(AggregateKind::Channel, Vec::new()),
                span: Span::DUMMY,
            },
            point,
            &mut state,
        );
        assert_eq!(
            queue.queues,
            state.value(out).queues,
            "the repeated allocation label has identical MAY data"
        );
        assert!(
            state.queue_promises.is_empty(),
            "the new current queue is not the old reserved object"
        );
        assert!(
            state.queues.values().any(|value| value
                .futures
                .values()
                .flatten()
                .any(|handle| handle.pending)),
            "older activity survives constructor reuse"
        );
    });
}

fn assert_present_message_after_parent_join(source: &str) {
    with_source_flow(source, |flow| {
        let named = |name: &str| {
            flow.function
                .locals
                .iter()
                .find(|decl| {
                    decl.name
                        .is_some_and(|name_symbol| name_symbol.as_str() == name)
                })
                .unwrap()
                .local
        };
        let retained = named("retained");
        let parent = named("parent");
        let (index, block) = flow.function.blocks.iter().enumerate().find(|(_, block)| {
            matches!(&block.terminator, Terminator::Call {func,args,..}
                if flow.function.resolve_callee_def_id(func).and_then(|id| flow.program.unwrap().definitions.unwrap().get(id)).is_some_and(|info| info.name == spark_intern::kw::RECV)
                && matches!(args.first(), Some(Operand::Copy(place) | Operand::Move(place)) if place.local == retained))
        }).unwrap();
        let statements = block.statements.clone();
        let mut state = flow.entries[index]
            .clone()
            .expect("normal parent join reaches the exact receive");
        for (step, statement) in statements.iter().enumerate() {
            flow.transfer_statement(
                statement,
                Point::new(BlockId(index as u32), step as u32),
                &mut state,
            );
        }
        if let Some(handles) = state.value(parent).futures.get(&Vec::new()) {
            assert!(
                handles.iter().all(|handle| !handle.pending),
                "the actual parent join completed"
            );
        } else {
            assert!(
                !state.live.contains(&parent),
                "a by-value join may consume its completed parent"
            );
        }
        assert_eq!(state.queue_promises.len(), 1);
        let (slot, promise) = state.queue_promises.iter().next().unwrap();
        let message = &state.queue_current[slot].value;
        assert!(!message.origins().is_empty());
        assert!(promise.recipe.birth.record(message).is_some());
        assert!(
            message.futures[&Vec::new()]
                .iter()
                .all(|handle| handle.pending),
            "parent completion never completes its queued child"
        );
        let role = Role::message(slot.clone());
        assert_eq!(
            state.future_copies.role_witness(&role).birth,
            Some(promise.recipe.birth),
            "normal producer completion must establish the actual present message's birth identity"
        );
        assert!(
            state.future_owners.known_role(&role),
            "the validated normal producer join discharges this message's exclusive-queue premise"
        );
        assert!(
            !state.future_owners.known(parent),
            "discharging the output lease does not grant blanket ownership to the nonleaf parent"
        );
    });
}

#[test]
fn a_normal_producer_join_establishes_present_message_owner_coverage() {
    assert_present_message_after_parent_join(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_retained_child_memory_scoped.spark"
    )));
}

#[test]
fn a_normal_helper_join_establishes_present_message_owner_coverage() {
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_retained_child_memory_scoped.spark"
    ));
    let source = source.replace("joined: Int = await parent", "joined: Int = join(parent)")
        + "\nfn join(task: Future[Int]): Int { joined: Int = await task return joined }\n";
    assert_present_message_after_parent_join(&source);
    let transitive = source.replace("joined: Int = await task", "joined: Int = joinAgain(task)")
        + "\nfn joinAgain(task: Future[Int]): Int { joined: Int = await task return joined }\n";
    assert_present_message_after_parent_join(&transitive);
}

#[test]
fn a_helper_completion_cannot_hide_another_queue_consumer() {
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_retained_child_memory_scoped.spark"
    ));
    let source = source.replace("joined: Int = await parent", "joined: Int = join(parent, retained)")
        + "\nasync fn steal(out: Channel[Future[Int]]): Int { child: Future[Int] = out.recv() return 0 }\nfn join(task: Future[Int], out: Channel[Future[Int]]): Int { consumer: Future[Int] = spawn { steal(out) } joined: Int = await task return joined }\n";
    with_source_flow(&source, |flow| {
        let parent = flow
            .function
            .locals
            .iter()
            .find(|decl| decl.name.is_some_and(|symbol| symbol.as_str() == "parent"))
            .unwrap()
            .local;
        let (index, block) = flow.function.blocks.iter().enumerate().find(|(_, block)| {
            matches!(&block.terminator, Terminator::Call {func,..} if flow.function.resolve_callee_def_id(func).and_then(|id| flow.program.unwrap().definitions.unwrap().get(id)).is_some_and(|info| info.name.as_str() == "join"))
        }).unwrap();
        let block = block.clone();
        let mut state = flow.entries[index]
            .clone()
            .expect("known join helper is reachable");
        for (step, statement) in block.statements.iter().enumerate() {
            flow.transfer_statement(
                statement,
                Point::new(BlockId(index as u32), step as u32),
                &mut state,
            );
        }
        assert_eq!(
            state.queue_promises.len(),
            1,
            "the real producer reservation exists before this helper"
        );
        let Terminator::Call {
            func,
            args,
            destination,
            ..
        } = &block.terminator
        else {
            unreachable!()
        };
        let point = Point::new(BlockId(index as u32), block.statements.len() as u32);
        flow.ensure_transfer(point, &state);
        let dispatch = flow.call_result(func, args, flow.place_type(destination), point, &state);
        assert!(
            dispatch.returning.iter().any(|outcome| outcome
                .future_proof
                .completed
                .iter()
                .any(|witness| witness.contains_local(parent))),
            "the helper really has the existing normal completion guarantee"
        );
        flow.apply_call_outcomes(args, destination, dispatch.returning, point, &mut state);
        assert!(
            state.queue_promises.is_empty(),
            "completion does not authorize the helper's independent consumer access"
        );
        assert!(state
            .queue_current
            .keys()
            .all(|slot| !state.future_owners.known_role(&Role::message(slot.clone()))));
        assert!(
            state.queues.values().any(|value| value
                .futures
                .values()
                .flatten()
                .any(|handle| handle.pending)),
            "unsafe-to-promote prefix data remains conservative"
        );
    });
}

#[test]
fn a_pure_helper_join_requires_the_matching_completion_proof() {
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_retained_child_memory_scoped.spark"
    ));
    let source = source.replace("joined: Int = await parent", "joined: Int = join(parent)")
        + "\nfn join(task: Future[Int]): Int { joined: Int = await task return joined }\n";
    with_source_flow(&source, |flow| {
        let (index, block) = flow.function.blocks.iter().enumerate().find(|(_, block)| {
            matches!(&block.terminator, Terminator::Call {func,..} if flow.function.resolve_callee_def_id(func).and_then(|id| flow.program.unwrap().definitions.unwrap().get(id)).is_some_and(|info| info.name.as_str() == "join"))
        }).unwrap();
        let block = block.clone();
        let mut state = flow.entries[index]
            .clone()
            .expect("known join helper is reachable");
        for (step, statement) in block.statements.iter().enumerate() {
            flow.transfer_statement(
                statement,
                Point::new(BlockId(index as u32), step as u32),
                &mut state,
            );
        }
        assert_eq!(
            state.queue_promises.len(),
            1,
            "the real producer reservation exists before this helper"
        );
        let Terminator::Call {
            func,
            args,
            destination,
            ..
        } = &block.terminator
        else {
            unreachable!()
        };
        let point = Point::new(BlockId(index as u32), block.statements.len() as u32);
        flow.ensure_transfer(point, &state);
        flow.program = Some(Context {
            proofs: None,
            ..flow.program.unwrap()
        });
        let dispatch = flow.call_result(func, args, flow.place_type(destination), point, &state);
        assert!(!dispatch.returning.is_empty());
        assert!(
            dispatch.returning.iter().all(|outcome| {
                outcome.future_proof.completed.is_empty() && outcome.queue_accesses.unknown
            }),
            "MAY summary access cannot substitute for the matching completion proof"
        );
        flow.apply_call_outcomes(args, destination, dispatch.returning, point, &mut state);
        assert!(state.queue_promises.is_empty());
        assert!(state.queues.values().any(|value| value
            .futures
            .values()
            .flatten()
            .any(|handle| handle.pending)));
    });
}
