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

fn with_source_flow(source: &str, run: impl FnOnce(&mut Flow<'_>)) {
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
