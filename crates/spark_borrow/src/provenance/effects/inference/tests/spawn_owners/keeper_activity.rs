// TAG [SPARK-114]

use super::*;

#[test]
fn replaced_child_kept_by_helper_has_no_pending_borrow_after_final_join() {
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_replaced_root_keeps_memory_scoped.spark"
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
                == "main"
        })
        .unwrap();
    let named = |name: &str| {
        function
            .locals
            .iter()
            .find(|decl| decl.name.is_some_and(|symbol| symbol.as_str() == name))
            .unwrap()
            .local
    };
    let cell_ref = named("cellRef");
    let mut flow = Flow::new(function, &output.table, Some(context));
    flow.solve(&facts[&function.def_id]);
    let origins: Origins = flow
        .entries
        .iter()
        .flatten()
        .flat_map(|state| state.value(cell_ref).origins())
        .collect();
    assert!(!origins.is_empty());
    let mut surviving = Vec::new();
    let mut reachable_returns = 0;
    for (index, block) in function.blocks.iter().enumerate() {
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
}
