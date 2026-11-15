// TAG [SPARK-114]

use super::*;

#[test]
fn known_memory_lanes_do_not_hide_opaque_writes_in_helpers_or_tasks() {
    let output = lower_source(
        r#"
struct Cell { entries: Map[String, Ref[String]]! }
struct Shared { cell: Ref[Cell]! }
fn clear(shared: Shared): Int {
    cell: Ref[Cell]! = shared.cell
    empty: Map[String, Ref[String]] = {}
    cell.entries = empty
    return 0
}
fn mixed(shared: Shared, callback: (Shared): Int): Int {
    changed: Int = clear(shared)
    return callback(shared)
}
fn forwarded(shared: Shared, callback: (Shared): Int): Int {
    return mixed(shared, callback)
}
async fn worker(shared: Shared, callback: (Shared): Int): Int {
    return forwarded(shared, callback)
}
fn launch(shared: Shared, callback: (Shared): Int): Int {
    task: Future[Int] = spawn { worker(shared, callback) }
    joined: Int = await task
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
    let (summaries, ports, proofs) = infer_summaries_with_definitions(
        &output.mir,
        &output.table,
        &facts,
        Some(&output.definitions),
    );
    let mut checked = FxHashSet::default();
    for (key, summary) in &summaries {
        let name = output.definitions.get(key.function).unwrap().name.as_str();
        if !matches!(name, "clear" | "mixed" | "forwarded" | "worker" | "launch") {
            continue;
        }
        assert!(
            !summary.prefix_memory.is_empty(),
            "{name}: known lane missing"
        );
        assert_eq!(summary.unknown_memory_write, name != "clear", "{name}");
        if name == "mixed" {
            assert!(
                !summary.unknown_copy_clobber,
                "a known opaque argument target need not clobber every Future copy"
            );
        }
        checked.insert(name.to_owned());
    }
    assert_eq!(checked.len(), 5);
    let launch = output
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
                == "launch"
        })
        .unwrap();
    let context = Context {
        mir: &output.mir,
        summaries: &summaries,
        ports: &ports,
        definitions: Some(&output.definitions),
        proofs: Some(&proofs),
    };
    let mut flow = Flow::new(launch, &output.table, Some(context));
    flow.solve(&facts[&launch.def_id]);
    let mut observed = false;
    for (index, entry) in flow.entries.clone().into_iter().enumerate() {
        let Some(mut state) = entry else { continue };
        if !state.rooted_activity().views().any(|_| true) {
            continue;
        }
        let point = crate::facts::Point::new(spark_mir::BlockId(index as u32), 0);
        flow.ensure_transfer(point, &state);
        flow.settle_pending_tasks(point, &mut state);
        observed |= state
            .rooted_activity()
            .views()
            .any(|view| view.memory.unknown_write && view.memory.samples().next().is_some());
    }
    assert!(
        observed,
        "task views must retain both known lanes and unknown write support"
    );
}
