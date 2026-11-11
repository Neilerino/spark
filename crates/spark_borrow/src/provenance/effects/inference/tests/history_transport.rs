// TAG [SPARK-114]

use super::*;

#[test]
fn returned_task_summary_keeps_the_observed_pointer_before_helper_retargeting() {
    let output = lower_source(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_returned_history_old_endpoint_scoped.spark"
    )));
    assert!(!output.diagnostics.has_errors());
    let facts = output
        .mir
        .functions
        .iter()
        .map(|(id, function)| (*id, crate::facts::extract_facts(function)))
        .collect();
    let (summaries, _, _) = infer_summaries_with_definitions(
        &output.mir,
        &output.table,
        &facts,
        Some(&output.definitions),
    );
    let launch = output
        .mir
        .functions
        .keys()
        .copied()
        .find(|id| output.definitions.get(*id).unwrap().name.as_str() == "launch")
        .unwrap();
    let samples: Vec<_> = summaries
        .iter()
        .filter(|(key, _)| key.function == launch)
        .flat_map(|(_, summary)| summary.returned.task_views.values())
        .flat_map(|subjects| subjects.values())
        .flat_map(|view| view.history.samples())
        .collect();
    assert!(
        !samples.is_empty(),
        "a returned pending task retains its sampled support"
    );
    let saved_inputs: FxHashSet<_> = samples
        .iter()
        .filter(|(lane, _)| {
            matches!(output.table.resolve(lane.sample.ty), spark_types::Ty::Struct(def)
            if output.definitions.get(*def).unwrap().name.as_str() == "Holder")
        })
        .flat_map(|(_, value)| value.leaves.values())
        .flat_map(|leaf| &leaf.dependencies)
        .filter_map(|dependency| match dependency {
            crate::provenance::address::Dependency::InputValue(selector, _) => Some(selector.input),
            _ => None,
        })
        .collect();
    assert!(
        saved_inputs.contains(&spark_mir::Local(1)),
        "the saved Holder value must retain its entry target, observed inputs: {saved_inputs:?}"
    );
}
