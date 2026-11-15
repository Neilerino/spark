// TAG [SPARK-114]

use super::*;

const EMIT: &str = "fn emit(out: Channel[Future[Int]]): Int { if true { child: Future[Int] = spawn { work() } out.send(child) } return 0 }";

fn with_program(
    source: &str,
    run: impl FnOnce(
        &spark_mir::MirOutput,
        &Summaries,
        &Ports,
        &crate::provenance::tasks::proofs::Proofs,
    ),
) {
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
    run(&output, &summaries, &ports, &proofs);
}

fn source(body: &str, helpers: &str) -> String {
    format!("async fn work(): Int {{ return 7 }}\n{EMIT}\n{helpers}\nfn produce(out: Channel[Future[Int]]): Int {{ {body} }}\nfn main(): Int {{ return 0 }}")
}

fn check(source: &str, expected: bool) {
    with_program(source, |output, summaries, _, proofs| {
        let mut checked = 0;
        for key in summaries
            .keys()
            .filter(|key| output.definitions.get(key.function).unwrap().name.as_str() == "produce")
        {
            assert_eq!(
                proofs[key]
                    .certificate()
                    .is_some_and(|certificate| !certificate.fresh_messages.is_empty()),
                expected,
                "{source}"
            );
            checked += 1;
        }
        assert!(checked > 0);
    });
}

#[test]
fn known_helper_outputs_compose_and_repeated_or_aliased_calls_do_not() {
    check(&source("emitted: Int = emit(out) return emitted", ""), true);
    check(
        &source(
            "emitted: Int = middle(out) return emitted",
            "fn middle(out: Channel[Future[Int]]): Int { emitted: Int = emit(out) return emitted }",
        ),
        true,
    );
    check(
        &source(
            "first: Int = emit(out) second: Int = emit(out) return second",
            "",
        ),
        false,
    );
    check(&source("emitted: Int = pair(out,out) return emitted", "fn pair(left: Channel[Future[Int]], right: Channel[Future[Int]]): Int { if true { one: Future[Int] = spawn { work() } left.send(one) } if true { two: Future[Int] = spawn { work() } right.send(two) } return 0 }"),false);
    check(&source("emitted: Int = forever(out) return emitted", "fn forever(out: Channel[Future[Int]]): Int { emitted: Int = emit(out) loop { } return emitted }"),false);
    check(&source("emitted: Int = middle(out,true) return emitted", "fn middle(out: Channel[Future[Int]], flag: Bool): Int { if flag { emitted: Int = emit(out) return emitted } emitted: Int = middle(out,flag) return emitted }"),true);
    check(&source("emitted: Int = recursive(out) return emitted", "fn recursive(out: Channel[Future[Int]]): Int { emitted: Int = recursive(out) return emitted }"),false);
}

#[test]
fn helper_prefix_observation_keeps_borrow_sources_before_partitioning() {
    let source = "struct Pair { left: Ref[Int], right: Ref[Int] }\nasync fn observe(pair: Pair): Int { return 0 }\nfn emit(out: Channel[Future[Int]], pair: Pair, flag: Bool): Int { if true { child: Future[Int] = spawn { observe(pair) } out.send(child) } if flag { loop { } } return 0 }\nfn produce(out: Channel[Future[Int]], left: Ref[Int], right: Ref[Int], flag: Bool): Int { pair: Pair = Pair { left: left, right: right } emitted: Int = emit(out,pair,flag) return emitted }\nfn main(): Int { return 0 }";
    with_program(source, |output, summaries, _, proofs| {
        let mut checked = 0;
        for (key, summary) in summaries.iter().filter(|(key, _)| {
            output.definitions.get(key.function).unwrap().name.as_str() == "produce"
        }) {
            assert_eq!(proofs[key].certificate().unwrap().fresh_messages.len(), 1);
            let function = &output.mir.functions[&key.function];
            let inputs: FxHashSet<_> = function
                .locals
                .iter()
                .filter(|decl| {
                    decl.name
                        .is_some_and(|name| name.as_str() == "left" || name.as_str() == "right")
                })
                .map(|decl| decl.local)
                .collect();
            let origins: FxHashSet<_> = summary
                .queues
                .values()
                .flat_map(Value::origins)
                .filter_map(|origin| {
                    if let crate::provenance::Origin::Input { local, .. } = origin {
                        Some(local)
                    } else {
                        None
                    }
                })
                .collect();
            let recipe = proofs[key]
                .certificate()
                .unwrap()
                .fresh_messages
                .iter()
                .next()
                .unwrap();
            let record = recipe.birth.record(&summary.fresh_outputs[recipe]).unwrap();
            let normal_inputs: FxHashSet<_> = record
                .callback
                .origins()
                .into_iter()
                .filter_map(|origin| {
                    if let crate::provenance::Origin::Input { local, .. } = origin {
                        Some(local)
                    } else {
                        None
                    }
                })
                .collect();
            assert!(
                inputs.is_subset(&normal_inputs),
                "normal callback data binds both caller inputs separately"
            );
            assert!(inputs.is_subset(&origins),"the original prefix must keep both borrowed supports across the normal-return partition");
            checked += 1;
        }
        assert!(checked > 0);
    });
}

#[test]
fn may_candidates_do_not_replace_a_callee_certificate() {
    with_program(
        &source("emitted: Int = emit(out) return emitted", ""),
        |output, summaries, ports, _| {
            let (key, _) = summaries
                .iter()
                .find(|(key, _)| {
                    output.definitions.get(key.function).unwrap().name.as_str() == "produce"
                })
                .unwrap();
            let function = &output.mir.functions[&key.function];
            let context = Context {
                mir: &output.mir,
                summaries,
                ports,
                proofs: None,
                definitions: Some(&output.definitions),
            };
            let mut flow = Flow::new(function, &output.table, Some(context));
            flow.aliases = key.aliases.clone();
            flow.callables = key.callables.clone();
            flow.solve(&crate::facts::extract_facts(function));
            assert!(
                !flow.summary().fresh_outputs.is_empty(),
                "bound MAY candidate vocabulary remains discoverable without a proof"
            );
            let crate::provenance::tasks::proofs::Guarantee::Returns(certificate) =
                flow.future_guarantee()
            else {
                panic!("returning helper")
            };
            assert!(
                certificate.fresh_messages.is_empty(),
                "MAY shapes alone grant no conditional output ownership"
            );
        },
    );
}

#[test]
fn helper_queue_moves_and_late_import_failures_preserve_their_distinction() {
    use crate::provenance::{
        observations::{FrameScope, Memory, Sample},
        tasks::memory::Lane,
        tasks::{lifecycle::Coverage, Role},
        State,
    };
    use spark_mir::{BlockId, Operand, Place, Terminator};
    with_program(
        &source("emitted: Int = emit(out) return emitted", ""),
        |output, summaries, ports, proofs| {
            let (key, _) = summaries
                .iter()
                .find(|(key, _)| {
                    output.definitions.get(key.function).unwrap().name.as_str() == "produce"
                })
                .unwrap();
            let function = &output.mir.functions[&key.function];
            let context = Context {
                mir: &output.mir,
                summaries,
                ports,
                proofs: Some(proofs),
                definitions: Some(&output.definitions),
            };
            let mut flow = Flow::new(function, &output.table, Some(context));
            flow.aliases = key.aliases.clone();
            flow.callables = key.callables.clone();
            flow.solve(&crate::facts::extract_facts(function));
            let (index,block)=function.blocks.iter().enumerate().find(|(_,block)|matches!(&block.terminator,Terminator::Call {func,..} if function.resolve_callee_def_id(func).is_some_and(|id|output.definitions.get(id).unwrap().name.as_str()=="emit"))).unwrap();
            let Terminator::Call {
                func,
                args,
                destination,
                ..
            } = &block.terminator
            else {
                unreachable!()
            };
            let point =
                crate::facts::Point::new(BlockId(index as u32), block.statements.len() as u32);
            let mut before = flow.entries[index].clone().unwrap();
            for (step, statement) in block.statements.iter().enumerate() {
                flow.transfer_statement(
                    statement,
                    crate::facts::Point::new(BlockId(index as u32), step as u32),
                    &mut before,
                );
            }
            let (Operand::Copy(queue) | Operand::Move(queue)) = &args[0] else {
                unreachable!()
            };
            let queue = queue.local;
            for (moved, clobber, import) in [
                (false, false, false),
                (true, false, false),
                (false, true, false),
                (true, true, false),
                (false, false, true),
                (true, false, true),
            ] {
                let mut state: State = before.clone();
                let mut args = args.clone();
                args[0] = if moved {
                    Operand::Move(Place::from_local(queue))
                } else {
                    Operand::Copy(Place::from_local(queue))
                };
                flow.transfers.remove(&point);
                flow.ensure_transfer(point, &state);
                let mut dispatch =
                    flow.call_result(func, &args, flow.place_type(destination), point, &state);
                assert_eq!(dispatch.returning.len(), 1);
                let mut outcome = dispatch.returning.pop().unwrap();
                assert_eq!(outcome.fresh_outputs.len(), 1);
                if clobber {
                    outcome.clobbered_copies.push(queue);
                }
                if import {
                    let payload = &mut outcome.fresh_outputs[0].value;
                    let (actor, view) = payload
                        .task_views
                        .get_mut(&Vec::new())
                        .unwrap()
                        .iter_mut()
                        .next()
                        .unwrap();
                    let ty = function.locals[queue.index()].ty;
                    view.memory.import(
                        std::sync::Arc::new(Lane {
                            execution: crate::provenance::effects::Execution::Current,
                            event: crate::provenance::address::OpaqueSite {
                                function: function.def_id,
                                point,
                            },
                            actor: actor.clone(),
                            location: Sample {
                                memory: Memory::Frame {
                                    scope: FrameScope::Current,
                                    function: function.def_id,
                                    local: queue,
                                    ty,
                                },
                                path: Vec::new(),
                                ty,
                            },
                        }),
                        crate::provenance::channels::allocation(function.def_id, point, ty),
                    );
                }
                flow.apply_call_outcomes(&args, destination, vec![outcome], point, &mut state);
                let conditional = state.queue_current.keys().any(|slot| {
                    matches!(
                        state.future_owners.coverage(&Role::message(slot.clone())),
                        Some(Coverage::ThroughQueue(_))
                    )
                });
                assert_eq!(
                    conditional,
                    !clobber && !import,
                    "moved {moved}, clobber {clobber}, import {import}"
                );
                assert!(state
                    .queue_current
                    .keys()
                    .all(|slot| !state.future_owners.known_role(&Role::message(slot.clone()))));
                if clobber || import {
                    assert!(
                        state.queues.values().any(|value| value
                            .futures
                            .values()
                            .flatten()
                            .any(|handle| handle.pending)),
                        "failed binding retains pending weak activity"
                    );
                }
                if import {
                    assert!(state.invalidated_copies.contains(&queue));
                }
            }
        },
    );
}

#[test]
fn divergent_repeated_prefixes_cannot_be_promoted_by_a_normal_recipe() {
    let helper="fn mixed(out: Channel[Future[Int]], flag: Bool): Int { if flag { if true { child: Future[Int] = spawn { work() } out.send(child) } return 0 } loop { if true { child: Future[Int] = spawn { work() } out.send(child) } } return 0 }";
    let source = source("emitted: Int = mixed(out,true) return emitted", helper);
    with_program(&source, |output, summaries, _, proofs| {
        let mut checked = 0;
        for (key, summary) in summaries.iter().filter(|(key, _)| {
            output.definitions.get(key.function).unwrap().name.as_str() == "mixed"
        }) {
            assert!(summary.normal_return);
            let certificate = proofs[key].certificate().unwrap();
            assert_eq!(certificate.fresh_messages.len(), 1);
            assert!(
                certificate.prefix_messages.is_empty(),
                "normal-return cardinality cannot certify an unbounded diverging prefix"
            );
            checked += 1;
        }
        assert!(checked > 0);
    });
    check(&source, false);
}

#[test]
fn deferred_reference_backed_queue_access_cannot_certify_a_fresh_output() {
    check(
        "struct Box { queue: Channel[Future[Int]] }
struct Shared { box: Ref[Box] }
async fn work(): Int { return 7 }
async fn consume(shared: Shared): Int { snapshot: Box = shared.box queue: Channel[Future[Int]] = snapshot.queue child: Future[Int] = queue.recv() return 0 }
fn produce(out: Channel[Future[Int]], shared: Shared): Int { if true { child: Future[Int] = spawn { work() } out.send(child) } worker: Future[Int] = spawn { consume(shared) } return 0 }
fn main(): Int { return 0 }",
        false,
    );
}

#[test]
fn returning_reference_backed_queue_access_cannot_certify_a_fresh_output() {
    check(
        "struct Box { queue: Channel[Future[Int]] }
struct Shared { box: Ref[Box] }
async fn work(): Int { return 7 }
fn produce(out: Channel[Future[Int]], shared: Shared): Shared { if true { child: Future[Int] = spawn { work() } out.send(child) } return shared }
fn main(): Int { return 0 }",
        false,
    );
}

#[test]
fn returning_reference_backed_plain_data_preserves_fresh_output_precision() {
    check(
        "struct Shared { value: Ref[Int] }
async fn work(): Int { return 7 }
fn produce(out: Channel[Future[Int]], shared: Shared): Shared { if true { child: Future[Int] = spawn { work() } out.send(child) } return shared }
fn main(): Int { return 0 }",
        true,
    );
}
