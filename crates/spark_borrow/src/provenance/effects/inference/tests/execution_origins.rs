// TAG [SPARK-114]

use super::*;
use crate::provenance::effects::prefix::Execution;

#[test]
fn synchronous_and_child_writes_keep_distinct_execution_origins_at_the_same_site() {
    let output = lower_source(
        r#"
struct Cell { entries: Map[String, Ref[String]]! }
struct Shared { cell: Ref[Cell]! }
fn write(shared: Shared): Int {
    cell: Ref[Cell]! = shared.cell
    empty: Map[String, Ref[String]] = {}
    cell.entries = empty
    return 0
}
async fn worker(shared: Shared): Int { return write(shared) }
fn mix(shared: Shared): Int {
    current: Int = write(shared)
    task: Future[Int] = spawn { worker(shared) }
    joined: Int = await task
    return joined
}
fn forwarded(shared: Shared): Int { return mix(shared) }
async fn envelope(shared: Shared): Int { return mix(shared) }
fn outer(shared: Shared): Int {
    parent: Future[Int] = spawn { envelope(shared) }
    joined: Int = await parent
    return joined
}
fn main(): Int { return 0 }
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
    let writer = output
        .mir
        .functions
        .keys()
        .find(|id| output.definitions.get(**id).unwrap().name.as_str() == "write")
        .unwrap();
    let mut checked = FxHashSet::default();
    for (key, summary) in &summaries {
        let name = output.definitions.get(key.function).unwrap().name.as_str();
        if !matches!(name, "mix" | "forwarded" | "outer") {
            continue;
        }
        let origins: FxHashSet<_> = summary
            .prefix_memory
            .keys()
            .filter(|event| event.site.function == *writer)
            .map(|event| event.execution)
            .collect();
        assert!(
            origins.contains(&Execution::Other),
            "{name}: child write must not become the parent's own write"
        );
        assert_eq!(
            origins.contains(&Execution::Current),
            name != "outer",
            "{name}"
        );
        checked.insert(name.to_owned());
    }
    assert_eq!(checked.len(), 3);
}

#[test]
fn receive_snapshots_keep_execution_origin_through_sync_and_task_binding() {
    let output = lower_source(
        r#"
struct Cell { entries: Map[String, Ref[String]]! }
struct Shared { cell: Ref[Cell]! }
fn write(shared: Shared): Int {
    cell: Ref[Cell]! = shared.cell
    empty: Map[String, Ref[String]] = {}
    cell.entries = empty
    return 0
}
fn snapshots(shared: Shared, input: Channel[Ref[String]]): Int {
    changed: Int = write(shared)
    value: Ref[String] = input.recv()
    return value.len()
}
fn syncSnapshot(shared: Shared, input: Channel[Ref[String]]): Int {
    return snapshots(shared, input)
}
async fn snapshotWorker(shared: Shared, input: Channel[Ref[String]]): Int {
    return snapshots(shared, input)
}
fn asyncSnapshot(shared: Shared, input: Channel[Ref[String]]): Int {
    waiting: Future[Int] = spawn { snapshotWorker(shared, input) }
    result: Int = await waiting
    return result
}
fn mixedSnapshot(shared: Shared, input: Channel[Ref[String]]): Int {
    changed: Int = write(shared)
    waiting: Future[Int] = spawn { snapshotWorker(shared, input) }
    result: Int = await waiting
    return result
}
fn main(): Int { return 0 }
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
    let writer = output
        .mir
        .functions
        .keys()
        .find(|id| output.definitions.get(**id).unwrap().name.as_str() == "write")
        .unwrap();
    let mut checked = FxHashSet::default();
    for (key, summary) in &summaries {
        let name = output.definitions.get(key.function).unwrap().name.as_str();
        if !matches!(
            name,
            "snapshots" | "syncSnapshot" | "snapshotWorker" | "asyncSnapshot" | "mixedSnapshot"
        ) {
            continue;
        }
        let origins: FxHashSet<_> = summary
            .receive_snapshots
            .values()
            .flat_map(|values| values.memory.keys())
            .filter(|event| event.site.function == *writer)
            .map(|event| event.execution)
            .collect();
        let expected = match name {
            "asyncSnapshot" => FxHashSet::from_iter([Execution::Other]),
            "mixedSnapshot" => FxHashSet::from_iter([Execution::Current, Execution::Other]),
            _ => FxHashSet::from_iter([Execution::Current]),
        };
        assert_eq!(origins, expected, "{name}");
        checked.insert(name.to_owned());
    }
    assert_eq!(checked.len(), 5);
}
