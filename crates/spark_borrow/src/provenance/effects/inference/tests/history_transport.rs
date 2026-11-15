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

#[test]
fn parent_activity_obeys_prefix_and_normal_return_contracts() {
    let diverging = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/build-pass/future_task_nonreturning_child_memory_scoped.spark"
    ));
    let returning = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_retained_child_memory_scoped.spark"
    ));
    for (source, returns) in [(diverging, false), (returning, true)] {
        let output = lower_source(source);
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
        let matching: Vec<_> = summaries
            .iter()
            .filter(|(key, _)| {
                output
                    .definitions
                    .get(key.function)
                    .is_some_and(|definition| definition.name.as_str() == "retainChild")
            })
            .collect();
        assert!(!matching.is_empty());
        for (_, summary) in matching {
            assert_eq!(summary.normal_return, returns);
            assert!(
                summary.residuals.is_empty(),
                "a represented queued child is not lost activity"
            );
            assert!(
                summary.returned.futures.is_empty(),
                "the helper returns only a scalar"
            );
            let pending_callback = |value: &Value| {
                value.futures.iter().any(|(path, handles)| {
                    handles
                        .iter()
                        .filter(|handle| handle.pending)
                        .any(|handle| {
                            let Some(view) = value.task_views.get(path).and_then(|views| {
                                views.get(&crate::provenance::tasks::views::Subject::from(handle))
                            }) else {
                                return false;
                            };
                            !view.fallback
                                && view.callbacks.values().any(|callback| {
                                    let callback = callback.value();
                                    !callback.origins().is_empty()
                                        && callback.callees.get(&Vec::new()).is_some_and(
                                            |targets| {
                                                !targets.open
                                                    && !targets.unresolved
                                                    && targets.known.len() == 1
                                            },
                                        )
                                })
                        })
                })
            };
            assert!(summary.queues.values().any(pending_callback), "reachable queue publication must retain pending child callback backing even without a normal return");
            assert_eq!(
                summary.queue_contents.values().any(pending_callback),
                returns,
                "only a normal return exports final queued contents"
            );
            assert!(
                summary.prefix_residuals.is_empty(),
                "the queued physical copy preserves activity without duplicate prefix loss debt"
            );
            assert!(summary.residual_callbacks.is_empty());
        }
    }
}
