// TAG [SPARK-114]

use super::*;
use crate::provenance::Value;
use spark_mir::{lower, MirInput, MirOutput};
use spark_modules::ModuleGraph;
use std::collections::HashMap;

mod ambient_captures;
mod fresh_messages;
mod history_transport;
mod output_contracts;
mod owner_calls;
mod queue_counts;
mod receive_snapshots;
mod spawn_owners;

#[test]
fn certificates_are_closed_against_their_own_refined_may_summaries() {
    let output = lower_source(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_helper_proof_closure.spark"
    )));
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
    let program = Program {
        mir: &output.mir,
        table: &output.table,
        facts: &facts,
        definitions: Some(&output.definitions),
    };
    let mut events = proofs
        .iter()
        .map(|(key, proof)| (key.clone(), proof.events.keys().cloned().collect()))
        .collect();
    let mut outputs = OutputVocabulary::default();
    discover_summary_outputs(&mut outputs, &summaries);
    let (rechecked, discovered, requests, new_events) =
        program.proofs(&summaries, &ports, &mut events, &mut outputs, Some(&proofs));
    assert!(!new_events);
    assert_eq!(proofs, rechecked);
    assert_eq!(ports, discovered);
    assert!(requests.iter().all(|key| summaries.contains_key(key)));
}

#[test]
fn task_queue_writes_are_exported_even_when_the_task_never_returns() {
    let output = lower_source(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_queue_prefix_before_divergence.spark"
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
    for name in ["publishForever", "launch"] {
        let matching: Vec<_> = summaries
            .iter()
            .filter(|(key, _)| {
                output
                    .definitions
                    .get(key.function)
                    .is_some_and(|definition| definition.name.as_str() == name)
            })
            .collect();
        assert!(!matching.is_empty(), "{name}");
        for (_, summary) in matching {
            assert!(!summary.queues.is_empty(), "{name}");
            assert_eq!(summary.normal_return, name == "launch", "{name}");
        }
    }
}

#[test]
fn task_created_queues_keep_their_identity_across_observation_points() {
    use super::super::CallActuals;
    use crate::{
        facts::Point,
        provenance::{
            channels::{OutputStorage, Target},
            value::child_type,
            Slot, Value,
        },
    };
    use spark_mir::BlockId;
    let output = lower_source(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_private_returned_queue_scoped.spark"
    )));
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
    let state = flow
        .entries
        .iter()
        .flatten()
        .find(|state| !state.tasks.is_empty())
        .unwrap()
        .clone();
    let (target, record) = state.tasks.iter().next().unwrap();
    let inner = child_type(target.ty(), &Slot::Element, &output.table).unwrap();
    let first = flow.task_facts(record, target, Point::new(BlockId::ENTRY, 0), &state, inner);
    let later = flow.task_facts(record, target, Point::new(BlockId::ENTRY, 1), &state, inner);
    let targets = |value: &Value| {
        value
            .queues
            .values()
            .flatten()
            .map(|handle| handle.target.clone())
            .collect::<FxHashSet<_>>()
    };
    let first_targets = targets(&first.returned);
    assert!(!first_targets.is_empty());
    assert_eq!(first_targets, targets(&later.returned));
    let site = target.instantiation_site().unwrap();
    assert!(first_targets.iter().all(|target| matches!(target,
        Target::Allocation { function, point, constructor: Some(constructor), .. }
            if *function == site.function && *point == site.point
                && constructor.role.as_ref().is_some_and(|role| role.storage == OutputStorage::Return)
    )));

    let callee = Value::default();
    let queue = first_targets.iter().next().unwrap();
    let earlier_point = Point::new(BlockId::ENTRY, 0);
    let later_point = Point::new(BlockId::ENTRY, 1);
    let actuals = CallActuals {
        callee_place: None,
        callee: &callee,
        arguments: &[],
        ty: inner,
        point: earlier_point,
        instantiation_site: None,
        frame_caller: None,
        read: crate::provenance::observations::ReadView::current(&state),
        aliases: None,
    };
    let ordinary_earlier = flow.bind_queue_target(queue, function, actuals);
    let ordinary_later = flow.bind_queue_target(
        queue,
        function,
        CallActuals {
            point: later_point,
            ..actuals
        },
    );
    assert_ne!(ordinary_earlier, ordinary_later);
    let stable_later = flow.bind_queue_target(
        queue,
        function,
        CallActuals {
            point: later_point,
            instantiation_site: Some(site),
            ..actuals
        },
    );
    assert_eq!(stable_later, first_targets);
    let ordinary_future = flow.bind_task_target(target, function, actuals);
    let later_future = flow.bind_task_target(
        target,
        function,
        CallActuals {
            point: later_point,
            ..actuals
        },
    );
    assert_ne!(ordinary_future, later_future);
    let stable_future = flow.bind_task_target(
        target,
        function,
        CallActuals {
            point: later_point,
            instantiation_site: Some(site),
            ..actuals
        },
    );
    assert_eq!(stable_future.len(), 1);
    assert_eq!(&stable_future.iter().next().unwrap().target, target);

    assert!(!first.prefix.queues.is_empty());
    assert_eq!(
        first
            .prefix
            .queues
            .iter()
            .map(|event| event.write.target.clone())
            .collect::<FxHashSet<_>>(),
        later
            .prefix
            .queues
            .iter()
            .map(|event| event.write.target.clone())
            .collect::<FxHashSet<_>>(),
    );
}

#[test]
fn late_task_publications_are_bound_even_when_the_body_never_returns() {
    let output = lower_source(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_late_diverging_prefix_scoped.spark"
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
    let matching: Vec<_> = summaries
        .iter()
        .filter(|(key, _)| {
            output.definitions.get(key.function).unwrap().name.as_str() == "publishForever"
        })
        .collect();
    assert!(!matching.is_empty());
    for (_, summary) in matching {
        assert!(!summary.normal_return);
        assert!(!summary.queues.is_empty());
    }
}

#[test]
fn executed_prefix_memory_keeps_transient_writes_and_no_return_effects() {
    let output = lower_source(
        r#"
        struct Cell { entries: Map[String, Ref[String]]! }
        fn transient(target: Ref[Cell]!, value: Ref[String]): Void {
            next: Map[String, Ref[String]] = {"one": value}
            target.entries = next
            empty: Map[String, Ref[String]] = {}
            target.entries = empty
        }
        fn forwarding(target: Ref[Cell]!, value: Ref[String]): Void {
            transient(target, value)
        }
        fn forever(target: Ref[Cell]!, value: Ref[String]): Void {
            next: Map[String, Ref[String]] = {"one": value}
            target.entries = next
            forever(target, value)
        }
    "#,
    );
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
    let transient = output
        .mir
        .functions
        .keys()
        .copied()
        .find(|id| output.definitions.get(*id).unwrap().name.as_str() == "transient")
        .unwrap();
    for name in ["transient", "forwarding", "forever"] {
        let matching: Vec<_> = summaries
            .iter()
            .filter(|(key, _)| output.definitions.get(key.function).unwrap().name.as_str() == name)
            .collect();
        assert!(!matching.is_empty(), "{name}");
        for (_, summary) in matching {
            let sites: FxHashSet<_> = summary
                .prefix_memory
                .keys()
                .map(|(site, _)| *site)
                .collect();
            assert_eq!(sites.len(), if name == "forever" { 1 } else { 2 }, "{name}");
            assert!(
                summary
                    .prefix_memory
                    .values()
                    .any(|value| !value.leaves.is_empty()),
                "{name}"
            );
            if name == "forever" {
                assert!(!summary.normal_return);
                assert!(summary.memory.is_empty());
            } else {
                assert!(summary.normal_return);
                assert!(
                    summary.prefix_memory.values().any(Value::is_empty),
                    "{name}"
                );
                assert!(
                    sites.iter().all(|site| site.function == transient),
                    "{name}: a snapshot is not an executed store"
                );
                assert!(
                    summary.memory.values().all(Value::is_empty),
                    "{name}: current memory must stay cleared"
                );
            }
        }
    }
}

#[test]
fn prefix_join_guards_follow_the_event_through_calls_and_nonreturning_paths() {
    use spark_mir::Local;
    let output = lower_source(
        r#"
        struct Cell { entries: Map[String, Ref[String]]! }
        fn write(target: Ref[Cell]!, value: Ref[String]): Void {
            next: Map[String, Ref[String]] = {"one": value}
            target.entries = next
        }
        fn waitThenWrite(target: Ref[Cell]!, value: Ref[String], task: Future[Int]): Void {
            joined: Int = await task
            write(target, value)
        }
        fn relay(target: Ref[Cell]!, value: Ref[String], other: Future[Int], task: Future[Int]): Void {
            waitThenWrite(target, value, task)
        }
        fn mixedOrder(target: Ref[Cell]!, value: Ref[String], task: Future[Int], choose: Bool): Void {
            if choose {
                waitThenWrite(target, value, task)
            } else {
                write(target, value)
            }
        }
        fn forever(): Void { forever() }
        fn diverge(target: Ref[Cell]!, value: Ref[String], task: Future[Int]): Void {
            waitThenWrite(target, value, task)
            forever()
        }
        fn forwardDiverge(target: Ref[Cell]!, value: Ref[String], task: Future[Int]): Void {
            diverge(target, value, task)
        }
    "#,
    );
    assert!(!output.diagnostics.has_errors());
    let facts = output
        .mir
        .functions
        .iter()
        .map(|(id, function)| (*id, crate::facts::extract_facts(function)))
        .collect();
    let (summaries, _, proofs) = infer_summaries_with_definitions(
        &output.mir,
        &output.table,
        &facts,
        Some(&output.definitions),
    );
    for (name, completed, normal_return) in [
        ("write", None, true),
        ("waitThenWrite", Some(Local(3)), true),
        ("relay", Some(Local(4)), true),
        ("mixedOrder", None, true),
        ("diverge", Some(Local(3)), false),
        ("forwardDiverge", Some(Local(3)), false),
    ] {
        let matching: Vec<_> = summaries
            .iter()
            .filter(|(key, _)| output.definitions.get(key.function).unwrap().name.as_str() == name)
            .collect();
        assert!(!matching.is_empty(), "{name}");
        for (key, summary) in matching {
            assert_eq!(summary.normal_return, normal_return, "{name}");
            assert!(!summary.prefix_memory.is_empty(), "{name}");
            let expected: FxHashSet<_> = completed.into_iter().collect();
            for event in summary.prefix_memory.keys() {
                assert_eq!(
                    proofs[key]
                        .events
                        .get(&super::EventKey::Memory(event.clone())),
                    Some(&expected),
                    "{name}"
                );
            }
        }
    }
}

#[test]
fn diverging_helper_composes_queue_and_memory_prefixes_before_task_completion() {
    let output = lower_source(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_prefix_queue_diverging_scoped.spark"
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
    let main = output
        .mir
        .functions
        .keys()
        .copied()
        .find(|id| output.definitions.get(*id).unwrap().name.as_str() == "main")
        .unwrap();
    let summary = &summaries[&Key::distinct(main)];
    assert!(!summary.normal_return);
    assert!(summary.queue_contents.is_empty());
    assert!(
        summary
            .queues
            .iter()
            .any(|((_, _, ty), payload)| matches!(output.table.resolve(*ty), spark_types::Ty::Ref { inner, .. }
                if matches!(output.table.resolve(*inner), spark_types::Ty::String))
                && payload.frame_sources(main).next().is_some()),
        "the worker can publish the borrowed string before the helper diverges"
    );
}

#[test]
fn forwarding_preserves_two_reads_with_the_same_selector() {
    let output = lower_source(
        r#"
        fn sample(input: Ref[String]): Int {
            first: Int = input.len()
            second: Int = input.len()
            return first + second
        }
        fn forward(input: Ref[String]): Int { return sample(input) }
    "#,
    );
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
    let owner = output
        .mir
        .functions
        .keys()
        .copied()
        .find(|id| output.definitions.get(*id).unwrap().name.as_str() == "sample")
        .unwrap();
    for name in ["sample", "forward"] {
        let function = output
            .mir
            .functions
            .keys()
            .copied()
            .find(|id| output.definitions.get(*id).unwrap().name.as_str() == name)
            .unwrap();
        let reads: Vec<_> = summaries[&Key::distinct(function)]
            .observed
            .iter()
            .filter(|access| access.kind == crate::provenance::AccessKind::Value)
            .collect();
        assert_eq!(reads.len(), 2, "{name}");
        let sites: FxHashSet<_> = reads.iter().map(|access| access.site.unwrap()).collect();
        assert_eq!(sites.len(), 2, "{name}");
        assert!(sites.iter().all(|site| site.function == owner), "{name}");
    }
}

#[test]
fn forwarding_preserves_two_receives_from_the_same_queue() {
    let output = lower_source(
        r#"
        fn sample(input: Channel[Ref[String]]): Int {
            first: Ref[String] = input.recv()
            second: Ref[String] = input.recv()
            return first.len() + second.len()
        }
        fn forward(input: Channel[Ref[String]]): Int { return sample(input) }
    "#,
    );
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
    let function = |name| {
        output
            .mir
            .functions
            .keys()
            .copied()
            .find(|id| output.definitions.get(*id).unwrap().name.as_str() == name)
            .unwrap()
    };
    use crate::provenance::{address::Dependency, AccessKind};

    let queue_reads = |name| {
        summaries[&Key::distinct(function(name))]
            .observed
            .iter()
            .filter(|access| access.kind == AccessKind::Value)
            .filter_map(|access| match &access.target {
                Dependency::InputValue(selector, site) if selector.queue.is_some() => {
                    Some(site.as_ref().expect("receive site").site)
                }
                _ => None,
            })
            .collect::<FxHashSet<_>>()
    };
    let sites = queue_reads("sample");
    assert_eq!(sites.len(), 2);
    assert!(sites.iter().all(|site| site.function == function("sample")));
    assert_eq!(queue_reads("forward"), sites);
}

#[test]
fn projected_queue_addresses_keep_their_receive_identity() {
    use crate::provenance::{observations::ReadView, read::ReadSite, Point, Slot};
    use spark_mir::{BlockId, Local};

    let output = lower_source(
        r#"
        struct Cell { text: String }
        fn sample(input: Channel[Ref[Cell]]): Void {}
    "#,
    );
    assert!(!output.diagnostics.has_errors());
    let function = output.mir.functions.values().next().unwrap();
    let flow = Flow::new(function, &output.table, None);
    let state = flow.initial_state();
    let channel = state.value(Local(1));
    let reference =
        crate::provenance::child_type(function.locals[1].ty, &Slot::Element, &output.table)
            .unwrap();
    let pointee = crate::provenance::child_type(reference, &Slot::Deref, &output.table).unwrap();
    let projection = vec![Slot::Field(spark_intern::sym("text"))];
    let project = |index| {
        let site = ReadSite {
            function: function.def_id,
            point: Point::new(BlockId::ENTRY, index),
        };
        let payload = flow.queue_payload_in(
            &channel,
            reference,
            ReadView::current(&state).with_receive(Some(&super::super::Receive::at(
                site,
                &state.reached_prefix,
            ))),
        );
        flow.project_reference(&payload, pointee, &projection)
            .endpoints
    };
    let first = project(0);
    assert!(!first.is_empty());
    assert_ne!(first, project(1));
}

#[test]
fn receive_prefixes_preserve_the_returning_call_that_precedes_each_read() {
    let output = lower_source(
        r#"
        struct Cell { items: Map[String, Ref[String]]! }
        fn write(target: Ref[Cell]!, value: Ref[String]): Void {
            items: Map[String, Ref[String]] = {"one": value}
            target.items = items
        }
        fn read(target: Ref[Cell]!, value: Ref[String], queue: Channel[Ref[String]]): Int {
            first: Ref[String] = queue.recv()
            write(target, value)
            second: Ref[String] = queue.recv()
            return first.len() + second.len()
        }
        fn forward(target: Ref[Cell]!, value: Ref[String], queue: Channel[Ref[String]]): Int {
            return read(target, value, queue)
        }
    "#,
    );
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
    let function = |name| {
        output
            .mir
            .functions
            .keys()
            .copied()
            .find(|id| output.definitions.get(*id).unwrap().name.as_str() == name)
            .unwrap()
    };
    let receipts = |name| {
        summaries[&Key::distinct(function(name))]
            .observed
            .iter()
            .filter_map(|access| match &access.target {
                crate::provenance::address::Dependency::InputValue(selector, Some(receive))
                    if selector.queue.is_some() =>
                {
                    Some(receive)
                }
                _ => None,
            })
            .collect::<FxHashSet<_>>()
    };
    let read = receipts("read");
    assert_eq!(read, receipts("forward"));
    assert_eq!(read.len(), 2);
    assert_eq!(
        read.iter()
            .filter(|receive| receive.before.is_empty())
            .count(),
        1
    );
    let occurrences: Vec<_> = read
        .iter()
        .flat_map(|receive| receive.before.iter())
        .collect();
    assert_eq!(occurrences.len(), 1);
    assert_eq!(occurrences[0].event.function, function("write"));
    let Some(super::super::prefix::Completion::Call(boundary)) = occurrences[0].returned_at else {
        panic!("the prefix must follow the returning call");
    };
    assert_eq!(boundary.function, function("read"));
}

#[test]
fn later_argument_binding_preserves_an_existing_receive_prefix() {
    use crate::provenance::{
        address::{Dependency, OpaqueSite},
        observations::ReadView,
        read::ReadSite,
        Point, Slot,
    };
    use spark_mir::{BlockId, Local};

    let output = lower_source(
        r#"
        struct Cell { text: Ref[String] }
        fn sample(input: Channel[Ref[Cell]]): Void {}
        fn extract(input: Ref[Cell]): Ref[String] {
            snapshot: Cell = input
            return snapshot.text
        }
    "#,
    );
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
    let function = |name| {
        output
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
                    == name
            })
            .unwrap()
    };
    let (sample, extract) = (function("sample"), function("extract"));
    let mut flow = Flow::new(sample, &output.table, None);
    let mut state = flow.initial_state();
    let point = Point::new(BlockId::ENTRY, 0);
    let receive = super::super::Receive::at(
        ReadSite {
            function: sample.def_id,
            point,
        },
        &state.reached_prefix,
    );
    let reference =
        crate::provenance::child_type(sample.locals[1].ty, &Slot::Element, &output.table).unwrap();
    let received = flow.queue_payload_in(
        &state.value(Local(1)),
        reference,
        ReadView::current(&state).with_receive(Some(&receive)),
    );
    state.reached_prefix.insert(super::super::PrefixOccurrence {
        event: OpaqueSite {
            function: extract.def_id,
            point,
        },
        returned_at: None,
    });
    let selected = flow.bind(
        &summaries[&Key::distinct(extract.def_id)].returned,
        extract,
        super::super::CallActuals {
            callee_place: None,
            callee: &Value::default(),
            arguments: &[received],
            ty: extract.return_type,
            point,
            instantiation_site: None,
            frame_caller: None,
            read: ReadView::current(&state),
            aliases: None,
        },
    );
    let receipts: Vec<_> = selected
        .leaves
        .values()
        .flat_map(|leaf| &leaf.dependencies)
        .filter_map(|dependency| match dependency {
            Dependency::InputValue(_, Some(receive)) => Some(receive),
            _ => None,
        })
        .collect();
    assert!(!receipts.is_empty(), "{selected:?}");
    assert!(
        receipts.iter().all(|receive| receive.before.is_empty()),
        "{receipts:?}"
    );
}

#[test]
fn a_diverging_only_publication_has_no_normal_return_occurrence() {
    let output = lower_source(
        r#"
        struct Cell { items: Map[String, Ref[String]]! }
        fn maybeWrite(target: Ref[Cell]!, value: Ref[String], flag: Bool): Void {
            if flag {
                items: Map[String, Ref[String]] = {"one": value}
                target.items = items
                loop {}
            }
        }
    "#,
    );
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
    let function = output.mir.functions.keys().copied().next().unwrap();
    let summary = &summaries[&Key::distinct(function)];
    assert!(summary.normal_return);
    assert!(!summary.prefix_memory.is_empty());
    assert!(
        summary.normal_prefix.is_empty(),
        "{:?}",
        summary.normal_prefix
    );
}

#[test]
fn a_shared_publication_site_keeps_diverging_owner_facts_out_of_normal_paths() {
    let output = lower_source(
        r#"
        struct Cell { items: Map[String, Ref[String]]! }
        fn write(target: Ref[Cell]!, value: Ref[String]): Void {
            items: Map[String, Ref[String]] = {"one": value}
            target.items = items
        }
        fn maybeWrite(target: Ref[Cell]!, incoming: Ref[String], flag: Bool): Void {
            if flag {
                text: String = " seven ".trim()
                local: Ref[String] = text
                write(target, local)
                loop {}
            }
            write(target, incoming)
        }
    "#,
    );
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
    let function = output
        .mir
        .functions
        .keys()
        .copied()
        .find(|id| output.definitions.get(*id).unwrap().name.as_str() == "maybeWrite")
        .unwrap();
    let summary = &summaries[&Key::distinct(function)];
    assert!(summary.normal_return);
    assert!(!summary.prefix_memory.is_empty());
    assert!(!summary.normal_prefix.is_empty());
    assert!(!summary.normal_publications.is_empty());
    assert!(summary.normal_publications.events().map(|(_, value)| value).all(|value| {
        value.leaves.values().all(|leaf| leaf.endpoints.iter().all(|endpoint| !matches!(
            endpoint,
            crate::provenance::address::Endpoint::Frame { function: owner, .. } if *owner == function
        )))
    }), "{:?}", summary.normal_publications);
}

#[test]
fn forwarding_preserves_projected_address_sites() {
    use crate::provenance::{
        address::{Dependency, Endpoint, Intent},
        aliases::Port,
        observations::ReadView,
        read::{Access, AccessKind, ReadSite},
        Origin, Point, Slot,
    };
    use spark_mir::{BlockId, Local};

    let output = lower_source(
        r#"
        struct Cell { text: String }
        fn sample(input: Ref[Cell]): Void {}
        fn forward(input: Ref[Cell]): Void { sample(input) }
    "#,
    );
    assert!(!output.diagnostics.has_errors());
    let function = |name| {
        output
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
                    == name
            })
            .unwrap()
    };
    let (sample, forward) = (function("sample"), function("forward"));
    let reference = sample.locals[1].ty;
    let pointee = crate::provenance::child_type(reference, &Slot::Deref, &output.table).unwrap();
    let mut selector = Port::root(Local(1), pointee).selector();
    let Intent::Address(payload) = &mut selector.intent else {
        unreachable!();
    };
    payload
        .projection
        .push(Slot::Field(spark_intern::sym("text")));
    let point = Point::new(BlockId::ENTRY, 0);
    let site = ReadSite {
        function: sample.def_id,
        point,
    };
    let mut callee_summary = Summary::default();
    callee_summary.observed.insert(Access {
        site: Some(site),
        origin: Origin::Input {
            local: Local(1),
            path: Vec::new(),
        },
        target: Dependency::Address(Endpoint::Input(selector, None)),
        selection: Vec::new(),
        kind: AccessKind::Value,
    });

    let mut flow = Flow::new(forward, &output.table, None);
    let state = flow.initial_state();
    let accesses = flow.bound_accesses(
        &callee_summary,
        sample,
        super::super::CallActuals {
            callee_place: None,
            callee: &Value::default(),
            arguments: &[state.value(Local(1))],
            ty: reference,
            point,
            instantiation_site: None,
            frame_caller: None,
            read: ReadView::current(&state),
            aliases: None,
        },
    );
    flow.record_accesses(point, accesses, &state);
    let mut summary = Summary::default();
    flow.export_prefix(&mut summary);
    let addresses: Vec<_> = summary
        .observed
        .iter()
        .filter(|access| access.kind == AccessKind::Address)
        .collect();
    assert!(!addresses.is_empty());
    assert!(
        addresses.iter().all(|access| access.site == Some(site)),
        "{addresses:?}"
    );
}

fn make_graph(file_id: spark_span::FileId, ast: &spark_ast::Ast) -> ModuleGraph {
    use spark_ast::Item;
    use spark_modules::{ModuleInfo, ModulePath};

    let module_path = ModulePath::from_single(spark_intern::sym("test"));
    let mut graph = ModuleGraph::default();
    let mut module_info = ModuleInfo::new(module_path.clone());
    module_info.files.push(file_id);
    for item in &ast.items {
        let name = match item {
            Item::Function(f) => Some(f.name.name),
            Item::Struct(s) => Some(s.name.name),
            Item::Enum(e) => Some(e.name.name),
            Item::Interface(i) => Some(i.name.name),
            Item::TypeAlias(t) => Some(t.name.name),
            Item::TypeDef(t) => Some(t.name.name),
            Item::Const(c) => Some(c.name.name),
            Item::Binding(_) => None,
        };
        if let Some(n) = name {
            module_info.exports.insert(n);
        }
    }
    graph.modules.insert(module_path.clone(), module_info);
    graph.compilation_order.push(module_path);
    graph
}

fn lower_source(source: &str) -> MirOutput {
    use spark_lexer::Lexer;
    use spark_parser::parse;
    use spark_resolve::{resolve, ResolverInput};
    use spark_span::FileId;
    use spark_types::{check, TypeCheckerInput};

    let file_id = FileId::new(0);

    let tokens = Lexer::new(file_id, source).tokenize();
    let (ast, diagnostics) = parse(tokens, file_id);
    assert!(!diagnostics.has_errors(), "{diagnostics:?}");
    let graph = make_graph(file_id, &ast);
    let mut asts = HashMap::new();
    asts.insert(file_id, ast);

    let resolver_output = resolve(ResolverInput { graph, asts });

    assert!(!resolver_output.diagnostics.has_errors());
    let type_output = check(TypeCheckerInput {
        resolved: &resolver_output.resolved,
        definitions: &resolver_output.definitions,
        graph: ModuleGraph::default(),
    });

    assert!(
        !type_output.diagnostics.has_errors(),
        "{source}\n{:?}",
        type_output.diagnostics
    );
    let hir_output = spark_hir::lower(spark_hir::HirInput {
        resolved: resolver_output.resolved,
        definitions: resolver_output.definitions,
        table: type_output.table,
        builtins: type_output.builtins,
    });

    assert!(
        !hir_output.diagnostics.has_errors(),
        "{source}\n{:?}",
        hir_output.diagnostics
    );
    lower(MirInput {
        hir: hir_output.hir,
        table: hir_output.table,
        definitions: hir_output.definitions,
    })
}

mod fresh_certificates;

mod fresh_helper_bindings;
