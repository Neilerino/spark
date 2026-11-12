// TAG [SPARK-114]

use super::*;

#[test]
fn replaced_child_kept_by_helper_has_no_pending_borrow_after_final_join() {
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_replaced_root_keeps_memory_scoped.spark"
    ));
    with_source_flow(source, |flow| {
        let named = |name: &str| {
            flow.function
                .locals
                .iter()
                .find(|decl| decl.name.is_some_and(|symbol| symbol.as_str() == name))
                .unwrap()
                .local
        };
        let cell_ref = named("cellRef");
        let origins: Origins = flow
            .entries
            .iter()
            .flatten()
            .flat_map(|state| state.value(cell_ref).origins())
            .collect();
        assert!(!origins.is_empty());
        let blocks = flow.function.blocks.clone();
        let mut surviving = Vec::new();
        let mut reachable_returns = 0;
        for (index, block) in blocks.iter().enumerate() {
            if !matches!(block.terminator, Terminator::Return) {
                continue;
            }
            let Some(mut state) = flow.entries[index].clone() else {
                continue;
            };
            assert!(flow.transfer_block(BlockId(index as u32), &mut state));
            reachable_returns += 1;
            for (storage, value) in state.rooted_activity().into_carriers() {
                if value
                    .futures
                    .values()
                    .flatten()
                    .any(|handle| handle.pending)
                    && !value.origins().is_disjoint(&origins)
                {
                    surviving.push(storage);
                }
            }
        }
        assert!(
            reachable_returns > 0,
            "the final await must return normally"
        );
        assert!(
            surviving.is_empty(),
            "a borrowed await must preserve its captured child's cleanup rights: {surviving:?}"
        );
    });
}

pub(super) fn with_source_flow(source: &str, run: impl FnOnce(&mut Flow<'_>)) {
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
                == "main"
        })
        .unwrap();
    let mut flow = Flow::new(function, &output.table, Some(context));
    flow.solve(&facts[&function.def_id]);
    run(&mut flow);
}

#[test]
fn a_nonleaf_producer_without_future_captures_has_complete_callback_backing() {
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_retained_child_memory_scoped.spark"
    ));
    with_source_flow(source, |flow| {
        let parent = flow
            .function
            .locals
            .iter()
            .find(|decl| decl.name.is_some_and(|symbol| symbol.as_str() == "parent"))
            .unwrap()
            .local;
        let (index, block) = flow
            .function
            .blocks
            .iter()
            .enumerate()
            .find(|(_, block)| {
                block.statements.iter().any(|statement|
                matches!(statement, Statement::Assign { place, .. } if place.local == parent))
            })
            .unwrap();
        let statements = block.statements.clone();
        let mut state = flow.entries[index]
            .clone()
            .expect("the producer spawn must return");
        for (step, statement) in statements.iter().enumerate() {
            flow.transfer_statement(
                statement,
                Point::new(BlockId(index as u32), step as u32),
                &mut state,
            );
            if matches!(statement, Statement::Assign { place, .. } if place.local == parent) {
                break;
            }
        }
        assert!(
            !state.future_owners.known(parent),
            "complete callback backing does not imply closed ownership of a nonleaf producer"
        );
        let (crate::provenance::demand::Storage::ExecutionCallback(anchor, _), record) = state.current_execution(parent).expect(
            "a known nonleaf producer has a current complete callback even without Future captures",
        ) else { unreachable!() };
        let targets = flow.callee_targets(&record.callback, record.callback_ty);
        assert_eq!(targets.known.len(), 1);
        assert!(!targets.open && !targets.unresolved);
        assert!(record.callback.futures.is_empty());
        assert!(!flow.leaf_task(&record));
        use crate::provenance::channels::identity::Role as ChannelRole;
        let retained = flow
            .function
            .locals
            .iter()
            .find(|decl| {
                decl.name
                    .is_some_and(|symbol| symbol.as_str() == "retained")
            })
            .unwrap()
            .local;
        let channel_ty = flow.function.locals[retained.index()].ty;
        let role = state
            .channel_copies
            .anchored(anchor)
            .find(|role| {
                matches!(role, ChannelRole::Capture { capture, callback_ty: Some(ty), .. }
                if capture.ty == channel_ty && *ty == record.callback_ty)
            })
            .expect("the producer retains the actual channel identity in its current callback");
        let witness = state.channel_copies.witness(role);
        assert!(witness.fresh);
        assert!(witness.peers.contains(&ChannelRole::Local(retained)));
    });
}

#[test]
fn actual_sends_have_current_storage_but_bound_helper_samples_remain_history() {
    let source = r#"
fn enqueue(channel: Channel[Ref[String]], value: Ref[String]): Int {
    channel.send(value)
    return 0
}
fn main(): Int {
    first: String = " first ".trim()
    firstRef: Ref[String] = first
    second: String = " second ".trim()
    secondRef: Ref[String] = second
    direct: Channel[Ref[String]] = Channel[Ref[String]]()
    indirect: Channel[Ref[String]] = Channel[Ref[String]]()
    direct.send(firstRef)
    sent: Int = enqueue(indirect, secondRef)
    found: Ref[String] = direct.recv()
    print(found.len())
    return 0
}
"#;
    with_source_flow(source, |flow| {
        let named = |name: &str| {
            flow.function
                .locals
                .iter()
                .find(|decl| decl.name.is_some_and(|symbol| symbol.as_str() == name))
                .unwrap()
                .local
        };
        let direct = named("direct");
        let indirect = named("indirect");
        let first = named("firstRef");
        let second = named("secondRef");
        let mut reached = false;
        for (index, block) in flow.function.blocks.clone().iter().enumerate() {
            if !matches!(block.terminator, Terminator::Return) {
                continue;
            }
            let Some(mut state) = flow.entries[index].clone() else {
                continue;
            };
            let direct_handles: Vec<_> =
                state.value(direct).root_queue_handles().cloned().collect();
            let indirect_handles: Vec<_> = state
                .value(indirect)
                .root_queue_handles()
                .cloned()
                .collect();
            let first_origins = state.value(first).origins();
            let second_origins = state.value(second).origins();
            assert!(!first_origins.is_empty() && !second_origins.is_empty());
            assert!(flow.transfer_block(BlockId(index as u32), &mut state));
            reached = true;
            assert_eq!(
                state.queue_current.len(),
                1,
                "only the actual builtin SEND is current"
            );
            let message = state.queue_current.values().next().unwrap();
            assert!(!message.value.origins().is_disjoint(&first_origins));
            assert!(direct_handles
                .iter()
                .all(|handle| message.targets.contains(&handle.target)));
            assert!(indirect_handles
                .iter()
                .all(|handle| !message.targets.contains(&handle.target)));
            let history: Origins = state.queues.values().flat_map(Value::origins).collect();
            assert!(!history.is_disjoint(&second_origins));
            assert!(history.is_disjoint(&first_origins));
        }
        assert!(reached);
    });
}

#[test]
fn a_producer_spawn_reserves_its_certified_current_message_before_weak_imports() {
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_retained_child_memory_scoped.spark"
    ));
    with_source_flow(source, |flow| {
        let named = |name: &str| {
            flow.function
                .locals
                .iter()
                .find(|decl| decl.name.is_some_and(|symbol| symbol.as_str() == name))
                .unwrap()
                .local
        };
        let parent = named("parent");
        let retained = named("retained");
        let (index, block) = flow
            .function
            .blocks
            .iter()
            .enumerate()
            .find(|(_, block)| {
                block.statements.iter().any(|statement| {
                    matches!(statement,
                Statement::Assign { place, .. } if place.local == parent)
                })
            })
            .unwrap();
        let statements = block.statements.clone();
        let mut state = flow.entries[index]
            .clone()
            .expect("the producer spawn returns");
        for (step, statement) in statements.iter().enumerate() {
            flow.transfer_statement(
                statement,
                Point::new(BlockId(index as u32), step as u32),
                &mut state,
            );
            if matches!(statement, Statement::Assign { place, .. } if place.local == parent) {
                break;
            }
        }
        let targets: FxHashSet<_> = state
            .value(retained)
            .queues
            .values()
            .flatten()
            .flat_map(|handle| flow.queue_targets(&handle.target))
            .collect();
        assert_eq!(targets.len(), 1);
        let messages: Vec<_> = state
            .queue_current
            .iter()
            .filter(|(_, message)| !message.targets.is_disjoint(&targets))
            .collect();
        assert_eq!(messages.len(), 1,
            "the certified producer's current output must be associated before sampled prefix imports become weak history");
        let (slot, message) = messages[0];
        assert!(
            !state
                .future_owners
                .known_role(&crate::provenance::tasks::Role::message(slot.clone())),
            "a pending producer has not discharged queue ownership"
        );
        assert!(message
            .value
            .futures
            .values()
            .flatten()
            .any(|handle| handle.pending));
        assert!(
            !message.value.origins().is_empty(),
            "full borrowed callback support remains backed"
        );
        assert!(
            targets
                .iter()
                .all(|target| !state.queues.keys().any(|(stored, _)| stored == target)),
            "the associated contribution must never be copied out of weak history after the fact"
        );
    });
}

#[test]
fn a_producer_entry_requires_enumerated_aliases_and_no_prior_access() {
    for (setup, tail, expected) in [
        ("", "", true),
        ("copy: Channel[Future[Int]] = out", "copy.close()", true),
        ("borrowed: Ref[Channel[Future[Int]]] = out", "", false),
        ("box: Box = Box { queue: out }", "box.queue.close()", false),
        ("consumer: Future[Int] = spawn { consume(out) }", "", false),
    ] {
        let source = format!("struct Box {{ queue: Channel[Future[Int]] }}
async fn work(): Int {{ return 7 }}
async fn emit(out: Channel[Future[Int]]): Int {{ if true {{ child: Future[Int] = spawn {{ work() }} out.send(child) }} return 0 }}
async fn consume(out: Channel[Future[Int]]): Int {{ child: Future[Int] = out.recv() return 0 }}
fn main(): Int {{ out: Channel[Future[Int]] = Channel[Future[Int]]() {setup} parent: Future[Int] = spawn {{ emit(out) }} {tail} return 0 }}");
        with_source_flow(&source, |flow| {
            let out = flow
                .function
                .locals
                .iter()
                .find(|decl| decl.name.is_some_and(|name| name.as_str() == "out"))
                .unwrap()
                .local;
            let mut entries = Vec::new();
            let mut roots = Vec::new();
            for (index, block) in flow.function.blocks.clone().iter().enumerate() {
                let Terminator::Call { func, args, .. } = &block.terminator else {
                    continue;
                };
                let Some(info) = flow
                    .function
                    .resolve_callee_def_id(func)
                    .and_then(|id| flow.program.unwrap().definitions.unwrap().get(id))
                else {
                    continue;
                };
                if info.name != spark_intern::kw::SPARK_SPAWN {
                    continue;
                }
                let Some(mut state) = flow.entries[index].clone() else {
                    continue;
                };
                for (step, statement) in block.statements.iter().enumerate() {
                    flow.transfer_statement(
                        statement,
                        Point::new(BlockId(index as u32), step as u32),
                        &mut state,
                    );
                }
                let target = state
                    .value(out)
                    .root_queue_handles()
                    .next()
                    .unwrap()
                    .target
                    .clone();
                if flow
                    .operand(&args[0], &state)
                    .queues
                    .values()
                    .flatten()
                    .any(|handle| handle.target == target)
                {
                    roots.push(
                        state
                            .live
                            .iter()
                            .map(|local| {
                                (
                                    *local,
                                    format!(
                                        "{:?}",
                                        flow.table.resolve(flow.function.locals[local.index()].ty)
                                    ),
                                )
                            })
                            .collect::<Vec<_>>(),
                    );
                    if expected {
                        let address = *state
                            .live
                            .iter()
                            .find(|local| {
                                matches!(
                                    flow.table.resolve(flow.function.locals[local.index()].ty),
                                    spark_types::Ty::Error
                                ) && !state.value(**local).callees.is_empty()
                            })
                            .expect("the real spawn has an intrinsic function-address operand");
                        let mut contaminated = state.clone();
                        let mut address_value = state.value(address);
                        address_value.join(&Value::reference(crate::provenance::Origin::Unproven(
                            Point::new(BlockId(index as u32), 0),
                        )));
                        contaminated.set(address, address_value);
                        assert!(
                            flow.empty_queue_entry(
                                &crate::provenance::channels::identity::Role::Local(out),
                                &contaminated
                            )
                            .is_none(),
                            "a known builtin target cannot hide additional reference support"
                        );
                    }
                    entries.push(flow.empty_queue_entry(
                        &crate::provenance::channels::identity::Role::Local(out),
                        &state,
                    ));
                }
            }
            let entry = entries.pop().expect("actual producer spawn is reachable");
            assert_eq!(entry.is_some(), expected, "{source}\n{roots:?}");
            if let Some(entry) = entry {
                assert_eq!(
                    entry.queue,
                    crate::provenance::channels::identity::Role::Local(out)
                );
                assert!(entry.witness.fresh);
                assert!(matches!(
                    entry.target,
                    crate::provenance::channels::Target::Allocation { .. }
                ));
            }
        });
    }
}

#[test]
fn the_retained_child_producer_starts_with_an_enumerated_empty_queue() {
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
        let mut checked = 0;
        for (index, block) in flow.function.blocks.clone().iter().enumerate() {
            let Terminator::Call { func, args, .. } = &block.terminator else {
                continue;
            };
            let Some(info) = flow
                .function
                .resolve_callee_def_id(func)
                .and_then(|id| flow.program.unwrap().definitions.unwrap().get(id))
            else {
                continue;
            };
            if info.name != spark_intern::kw::SPARK_SPAWN {
                continue;
            }
            let Some(mut state) = flow.entries[index].clone() else {
                continue;
            };
            for (step, statement) in block.statements.iter().enumerate() {
                flow.transfer_statement(
                    statement,
                    Point::new(BlockId(index as u32), step as u32),
                    &mut state,
                );
            }
            let targets: FxHashSet<_> = state
                .value(retained)
                .root_queue_handles()
                .map(|handle| handle.target.clone())
                .collect();
            if targets.is_empty()
                || !flow
                    .operand(&args[0], &state)
                    .queues
                    .values()
                    .flatten()
                    .any(|handle| targets.contains(&handle.target))
            {
                continue;
            }
            assert!(flow.empty_queue_entry(&crate::provenance::channels::identity::Role::Local(retained), &state).is_some(),
                "the unchanged safe public producer has a closed empty queue before its first spawn");
            checked += 1;
        }
        assert_eq!(checked, 1);
    });
}
