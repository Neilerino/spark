// TAG [SPARK-114]

use super::keeper_activity::with_source_flow;
use super::*;

#[test]
fn a_final_nonreturning_parent_drop_stops_before_its_borrowed_cell_owner_drop() {
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/build-pass/future_task_nonreturning_child_memory_scoped.spark"
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
        let cell = named("cell");
        let text = named("text");
        let drops = |statement: &Statement, local| matches!(statement, Statement::Drop {place,..} if place.local == local && place.projections.is_empty());
        let (index, block, parent_drop) = flow
            .function
            .blocks
            .iter()
            .enumerate()
            .find_map(|(index, block)| {
                block
                    .statements
                    .iter()
                    .position(|statement| drops(statement, parent))
                    .map(|step| (index, block.clone(), step))
            })
            .unwrap();
        let cell_drop = block
            .statements
            .iter()
            .position(|statement| drops(statement, cell))
            .expect("the borrowed Cell owner drops in this scope");
        let text_drop = block
            .statements
            .iter()
            .position(|statement| drops(statement, text))
            .expect("the later String binding retains its earlier LIFO drop");
        assert!(
            text_drop < parent_drop && parent_drop < cell_drop,
            "model the actual LIFO order without reordering owners"
        );
        let entry = flow.entries[index]
            .clone()
            .expect("the parent's real final drop is reachable");
        let mut before = entry.clone();
        for (step, statement) in block.statements.iter().take(parent_drop).enumerate() {
            flow.transfer_statement(
                statement,
                Point::new(BlockId(index as u32), step as u32),
                &mut before,
            );
        }
        assert!(before.value(parent).futures[&Vec::new()]
            .iter()
            .all(|handle| handle.pending));
        assert!(
            before.current_execution_record(parent).is_some(),
            "the actual nonleaf execution has complete current callback backing"
        );
        assert!(
            !before.future_owners.known(parent),
            "direct last-release reasoning must not grant descendant cleanup ownership"
        );
        assert!(
            !before.queues.is_empty(),
            "the escaped child remains represented conservatively"
        );
        let mut after = entry;
        assert!(
            !flow.transfer_block(BlockId(index as u32), &mut after),
            "the last release waits forever before the later Cell drop and Return"
        );
        assert!(
            after.value(parent).futures[&Vec::new()]
                .iter()
                .all(|handle| handle.pending),
            "a nonreturning wait does not complete the parent"
        );
        assert!(
            after.queues.values().any(|value| value
                .futures
                .values()
                .flatten()
                .any(|handle| handle.pending)),
            "the escaped child remains pending during the wait"
        );
        assert!(
            flow.program
                .unwrap()
                .summaries
                .iter()
                .filter(|(key, _)| key.function == flow.function.def_id)
                .all(|(_, summary)| !summary.normal_return),
            "MAY dependency growth does not fossilize a synthetic normal return"
        );
        assert!(
            !flow.summary().normal_return,
            "a syntactic Return after the blocking drop has no normal-return summary"
        );
    });
}

#[test]
fn canonical_nonleaf_spawn_earns_direct_ownership_without_leaf_birth_or_cleanup_coverage() {
    use crate::provenance::tasks::lifecycle::Coverage;
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/build-pass/future_task_nonreturning_child_memory_scoped.spark"
    ));
    with_source_flow(source, |flow| {
        let parent = flow
            .function
            .locals
            .iter()
            .find(|decl| decl.name.is_some_and(|name| name.as_str() == "parent"))
            .unwrap()
            .local;
        let state = flow
            .entries
            .iter()
            .flatten()
            .find(|state| state.current_execution_record(parent).is_some())
            .expect("the actual spawned parent is backed");
        let role = Role::root(parent);
        assert_eq!(state.future_owners.coverage(&role), Some(Coverage::Direct));
        assert!(!state.future_owners.known(parent));
        assert!(
            state.future_copies.witness(parent).birth.is_none(),
            "nonleaf admission does not widen leaf birth provenance"
        );
        assert_eq!(
            Coverage::Closed.meet(Coverage::Direct),
            Some(Coverage::Direct)
        );
        assert_eq!(
            Coverage::Direct.meet(Coverage::Closed),
            Some(Coverage::Direct)
        );
        let conditional = Coverage::ThroughQueue(std::sync::Arc::new(
            crate::provenance::aliases::Port::root(parent, flow.function.locals[parent.index()].ty),
        ));
        assert_eq!(Coverage::Direct.meet(conditional.clone()), None);
        assert_eq!(conditional.meet(Coverage::Direct), None);
        let mut missing = state.clone();
        missing.future_owners.forget(parent);
        let mut joined = state.clone();
        assert!(
            joined.join(&missing),
            "metadata-only Direct loss must wake the owner analysis"
        );
        assert!(joined.future_owners.coverage(&role).is_none());
        let mut forged = missing;
        let record = forged.current_execution_record(parent).unwrap();
        forged.install_execution_callback(parent, record.callback_ty, record.callback, true);
        assert!(
            forged.future_owners.coverage(&role).is_none(),
            "complete MAY callback installation cannot forge actual constructor admission"
        );
    });
}

fn simple_parent_source(body: &str, main: &str) -> String {
    format!("async fn childWork(): Int {{ return 7 }}\nasync fn worker(): Int {{ child: Future[Int] = spawn {{ childWork() }} {body} }}\nfn main(): Int {{ parent: Future[Int] = spawn {{ worker() }} {main} return 0 }}")
}

#[test]
fn only_the_final_direct_copy_release_can_stop_normal_flow() {
    use crate::provenance::tasks::lifecycle::Coverage;
    with_source_flow(
        &simple_parent_source("loop { } return 0", "copy: Future[Int] = parent"),
        |flow| {
            let named = |name: &str| {
                flow.function
                    .locals
                    .iter()
                    .find(|decl| decl.name.is_some_and(|symbol| symbol.as_str() == name))
                    .unwrap()
                    .local
            };
            let parent = named("parent");
            let copy = named("copy");
            let (index, block, step) = flow.function.blocks.iter().enumerate().find_map(|(index, block)| block.statements.iter().position(|statement| matches!(statement, Statement::Drop {place,..} if place.local == copy)).map(|step| (index, block.clone(), step))).unwrap();
            let mut state = flow.entries[index].clone().unwrap();
            for (step, statement) in block.statements.iter().take(step).enumerate() {
                assert!(flow.transfer_statement(
                    statement,
                    Point::new(BlockId(index as u32), step as u32),
                    &mut state
                ));
            }
            assert_eq!(
                state.future_owners.coverage(&Role::root(copy)),
                Some(Coverage::Direct)
            );
            assert_eq!(
                state.future_copies.witness(copy).peers,
                rustc_hash::FxHashSet::from_iter([Role::root(parent), Role::root(copy)])
            );
            assert!(
                flow.transfer_statement(
                    &block.statements[step],
                    Point::new(BlockId(index as u32), step as u32),
                    &mut state
                ),
                "a surviving strong owner prevents the first release from waiting"
            );
            let mut stopped = false;
            for (offset, statement) in block.statements.iter().enumerate().skip(step + 1) {
                if !flow.transfer_statement(
                    statement,
                    Point::new(BlockId(index as u32), offset as u32),
                    &mut state,
                ) {
                    assert!(
                        matches!(statement, Statement::Drop {place,..} if place.local == parent)
                    );
                    assert!(!state.future_owners.known(parent));
                    stopped = true;
                    break;
                }
            }
            assert!(stopped, "the final supported direct owner still waits");
        },
    );
}

#[test]
fn returning_exposed_or_unproven_parent_releases_keep_their_normal_edge() {
    for (body, main, forget, missing_context) in [
        ("return 0", "", false, false),
        (
            "loop { } return 0",
            "reference: Ref[Future[Int]] = parent",
            false,
            false,
        ),
        ("loop { } return 0", "", true, false),
        ("loop { } return 0", "", false, true),
    ] {
        with_source_flow(&simple_parent_source(body, main), |flow| {
            let parent = flow
                .function
                .locals
                .iter()
                .find(|decl| decl.name.is_some_and(|name| name.as_str() == "parent"))
                .unwrap()
                .local;
            let (index, block, step) = flow.function.blocks.iter().enumerate().find_map(|(index, block)| block.statements.iter().position(|statement| matches!(statement, Statement::Drop {place,..} if place.local == parent)).map(|step| (index, block.clone(), step))).unwrap();
            let mut state = flow.entries[index].clone().unwrap();
            for (step, statement) in block.statements.iter().take(step).enumerate() {
                assert!(flow.transfer_statement(
                    statement,
                    Point::new(BlockId(index as u32), step as u32),
                    &mut state
                ));
            }
            if forget {
                state.future_owners.forget(parent);
            }
            if missing_context {
                flow.program = None;
            }
            assert!(
                flow.transfer_statement(
                    &block.statements[step],
                    Point::new(BlockId(index as u32), step as u32),
                    &mut state
                ),
                "body={body} exposed={main} forgotten={forget} noContext={missing_context}"
            );
        });
    }
}

#[test]
fn a_known_missing_callback_dependency_is_revisited_before_keeping_a_normal_edge() {
    with_source_flow(&simple_parent_source("return 0", ""), |flow| {
        let parent = flow
            .function
            .locals
            .iter()
            .find(|decl| decl.name.is_some_and(|name| name.as_str() == "parent"))
            .unwrap()
            .local;
        let (index, block, step) = flow.function.blocks.iter().enumerate().find_map(|(index, block)| block.statements.iter().position(|statement| matches!(statement, Statement::Drop {place,..} if place.local == parent)).map(|step| (index, block.clone(), step))).unwrap();
        let mut state = flow.entries[index].clone().unwrap();
        for (step, statement) in block.statements.iter().take(step).enumerate() {
            assert!(flow.transfer_statement(
                statement,
                Point::new(BlockId(index as u32), step as u32),
                &mut state
            ));
        }
        let point = Point::new(BlockId(index as u32), step as u32);
        let place = spark_mir::Place::from_local(parent);
        let context = flow.program.unwrap();
        let empty = rustc_hash::FxHashMap::default();
        let mut probe = Flow::new(
            flow.function,
            flow.table,
            Some(Context {
                summaries: &empty,
                ..context
            }),
        );
        probe.ensure_transfer(point, &state);
        let mut pending = state.clone();
        assert!(!probe.future_drop_continues(&place, point, &mut pending));
        assert!(
            !probe.requests.is_empty(),
            "this is a pending dependency, not an empty-family NoReturn proof"
        );
        probe.program = Some(context);
        assert!(
            probe.future_drop_continues(&place, point, &mut state),
            "resolving the known returning family restores normal continuation"
        );
    });
}

#[test]
fn last_release_rechecks_changed_callable_pointees_after_a_real_helper_prefix() {
    use crate::provenance::tasks::{last_release::LastRelease, ReturnStatus};
    let source = "struct Holder { actions: List[(): Int]! }\nstruct Shared { holder: Ref[Holder]! }\nfn never(): Int { loop { } return 0 }\nfn returns(): Int { return 1 }\nasync fn runner(shared: Shared): Int { selected: (): Int = shared.holder.actions[0] ?? never return selected() }\nfn setAction(shared: Shared): Int { holder: Ref[Holder]! = shared.holder holder.actions = [returns] return 0 }\nfn main(): Int { holder: Holder! = Holder { actions: [never] } reference: Ref[Holder]! = holder shared: Shared = Shared { holder: reference } parent: Future[Int] = spawn { runner(shared) } changed: Int = setAction(shared) return 0 }";
    with_source_flow(source, |flow| {
        let parent = flow
            .function
            .locals
            .iter()
            .find(|decl| decl.name.is_some_and(|name| name.as_str() == "parent"))
            .unwrap()
            .local;
        let (index, block) = flow.function.blocks.iter().enumerate().find(|(_, block)| matches!(&block.terminator, Terminator::Call {func,..} if flow.function.resolve_callee_def_id(func).and_then(|def| flow.program.unwrap().definitions.unwrap().get(def)).is_some_and(|info| info.name.as_str() == "setAction"))).unwrap();
        let block = block.clone();
        let mut state = flow.entries[index].clone().unwrap();
        for (step, statement) in block.statements.iter().enumerate() {
            assert!(flow.transfer_statement(
                statement,
                Point::new(BlockId(index as u32), step as u32),
                &mut state
            ));
        }
        let place = spark_mir::Place::from_local(parent);
        let point = Point::new(BlockId(index as u32), block.statements.len() as u32);
        flow.ensure_transfer(point, &state);
        let release =
            LastRelease::at_drop(flow, &place, &state).expect("actual singleton direct parent");
        let before = state.current_execution_record(parent).unwrap();
        let handle = state.value(parent).futures[&Vec::new()]
            .iter()
            .next()
            .unwrap()
            .clone();
        let initial = flow.task_facts(
            &before,
            &handle.target,
            point,
            &state,
            flow.function.return_type,
        );
        assert_eq!(
            initial.return_status,
            ReturnStatus::NoReturn,
            "initial real callable context: {:?}",
            initial.contexts
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
        let dispatch = flow.call_result(func, args, flow.place_type(destination), point, &state);
        assert!(
            dispatch
                .returning
                .iter()
                .any(|outcome| !outcome.prefix_memory.is_empty() || !outcome.memory.is_empty()),
            "the helper really writes the referenced field"
        );
        flow.apply_call_outcomes(args, destination, dispatch.returning, point, &mut state);
        let current = state
            .current_execution_record(parent)
            .expect("the parent callback remains completely backed");
        assert!(
            LastRelease::at_drop(flow, &place, &state).is_some(),
            "the same singleton direct release remains eligible after the pointee write"
        );
        assert_eq!(before.callback_ty, current.callback_ty);
        assert_eq!(
            before.callback.callees.get(&Vec::new()),
            current.callback.callees.get(&Vec::new())
        );
        assert!(state.value(parent).futures[&Vec::new()].contains(&handle));
        let updated = flow.task_facts(
            &current,
            &handle.target,
            point,
            &state,
            flow.function.return_type,
        );
        assert_eq!(
            updated.return_status,
            ReturnStatus::MayReturn,
            "the same root function now observes a returning callable family"
        );
        assert_ne!(initial.contexts, updated.contexts);
        assert!(
            release.continues_after_prefix(flow, point, &mut state),
            "a frozen NoReturn result must not survive a changed captured callable"
        );
    });
}

#[test]
fn late_prefix_memory_clobbers_cancel_only_affected_final_release_proofs() {
    use crate::provenance::{
        address::OpaqueSite,
        observations::{FrameScope, Memory, Sample},
        tasks::memory::Lane,
    };
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/build-pass/future_task_nonreturning_child_memory_scoped.spark"
    ));
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
        let parent = named("parent");
        let unrelated = named("installed");
        let (index, block, step) = flow.function.blocks.iter().enumerate().find_map(|(index, block)| block.statements.iter().position(|statement| matches!(statement, Statement::Drop {place,..} if place.local == parent)).map(|step| (index, block.clone(), step))).unwrap();
        let mut before = flow.entries[index].clone().unwrap();
        for (step, statement) in block.statements.iter().take(step).enumerate() {
            assert!(flow.transfer_statement(
                statement,
                Point::new(BlockId(index as u32), step as u32),
                &mut before
            ));
        }
        let point = Point::new(BlockId(index as u32), step as u32);
        let place = spark_mir::Place::from_local(parent);
        let original = before.value(parent).origins();
        assert!(!original.is_empty());
        for overwritten in [Some(parent), Some(unrelated), None] {
            let mut state = before.clone();
            let local = overwritten.unwrap_or(parent);
            let ty = flow.function.locals[local.index()].ty;
            let payload = if local == parent {
                crate::provenance::tasks::birth(flow.function.def_id, point, ty).1
            } else {
                Value::default()
            };
            let mut value = state.value(parent);
            let (actor, view) = value
                .task_views
                .get_mut(&Vec::new())
                .unwrap()
                .iter_mut()
                .next()
                .unwrap();
            view.memory.import(
                std::sync::Arc::new(Lane {
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
                payload,
            );
            flow.transfers.remove(&point);
            flow.ensure_transfer(point, &state);
            let release =
                crate::provenance::tasks::last_release::LastRelease::at_drop(flow, &place, &state)
                    .expect("the failure must arise after the initial admission");
            flow.import_task_sources(&mut value, point, &mut state);
            assert!(
                state
                    .invalidated_copies
                    .contains(&overwritten.unwrap_or(parent)),
                "the actual source import must physically clobber its target"
            );
            let continues = release.continues_after_prefix(flow, point, &mut state);
            assert_eq!(
                continues,
                overwritten != Some(unrelated),
                "clobber {overwritten:?}"
            );
            assert!(
                state
                    .invalidated_copies
                    .contains(&overwritten.unwrap_or(parent)),
                "the real memory lane physically clobbers its target"
            );
            let remaining: Origins = state
                .rooted_activity()
                .into_carriers()
                .flat_map(|(_, value)| value.origins())
                .collect();
            assert!(
                original.is_subset(&remaining),
                "clobber {overwritten:?} preserves borrowed pending writer support"
            );
        }
    });
}
