// TAG [SPARK-114]

use super::*;

#[test]
fn actual_empty_stores_and_known_wrappers_earn_normal_definition_coverage() {
    let output = lower_source(
        r#"
struct Cell { entries: Map[String, Ref[String]]! }
fn clear(cell: Ref[Cell]!): Int {
    empty: Map[String, Ref[String]] = {}
    cell.entries = empty
    return 0
}
fn wrapper(cell: Ref[Cell]!): Int { return clear(cell) }
fn identity(cell: Ref[Cell]!): Int { return 0 }
fn allBranches(cell: Ref[Cell]!, flag: Bool): Int {
    if flag { return clear(cell) }
    return clear(cell)
}
fn partial(cell: Ref[Cell]!, flag: Bool): Int {
    if flag { return clear(cell) }
    return 0
}
fn recursive(cell: Ref[Cell]!, depth: Int): Int {
    if depth > 0 { return recursive(cell, depth - 1) }
    return clear(cell)
}
fn never(cell: Ref[Cell]!): Int {
    cleared: Int = clear(cell)
    loop {}
    return 0
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
    let (summaries, _, proofs) = infer_summaries_with_definitions(
        &output.mir,
        &output.table,
        &facts,
        Some(&output.definitions),
    );
    let mut checked = FxHashSet::default();
    for key in summaries.keys() {
        let name = output.definitions.get(key.function).unwrap().name.as_str();
        if !matches!(
            name,
            "clear" | "wrapper" | "identity" | "allBranches" | "partial" | "recursive" | "never"
        ) {
            continue;
        }
        if name == "never" {
            assert!(proofs[key].certificate().is_none());
            checked.insert(name.to_owned());
            continue;
        }
        let certificate = proofs[key].certificate().unwrap();
        assert_eq!(
            !certificate.actual_definitions.is_empty(),
            !matches!(name, "identity" | "partial"),
            "normal actual-definition coverage for {name}",
        );
        checked.insert(name.to_owned());
    }
    assert_eq!(checked.len(), 7);
}

#[test]
fn stores_only_earn_ordered_coverage_after_the_named_input_joins() {
    let output = lower_source(
        r#"
struct Cell { entries: Map[String, Ref[String]]! }
fn after(cell: Ref[Cell]!, task: Future[Int]): Int {
    result: Int = await task
    empty: Map[String, Ref[String]] = {}
    cell.entries = empty
    return result
}
fn before(cell: Ref[Cell]!, task: Future[Int]): Int {
    empty: Map[String, Ref[String]] = {}
    cell.entries = empty
    return await task
}
fn throughAfter(cell: Ref[Cell]!, task: Future[Int]): Int { return after(cell, task) }
fn throughBefore(cell: Ref[Cell]!, task: Future[Int]): Int { return before(cell, task) }
fn recursiveAfter(cell: Ref[Cell]!, task: Future[Int], depth: Int): Int {
    if depth > 0 { return recursiveAfter(cell, task, depth - 1) }
    return after(cell, task)
}
fn recursiveBefore(cell: Ref[Cell]!, task: Future[Int], depth: Int): Int {
    if depth > 0 { return recursiveBefore(cell, task, depth - 1) }
    return before(cell, task)
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
    let (summaries, _, proofs) = infer_summaries_with_definitions(
        &output.mir,
        &output.table,
        &facts,
        Some(&output.definitions),
    );
    let mut checked = FxHashSet::default();
    for key in summaries.keys() {
        let name = output.definitions.get(key.function).unwrap().name.as_str();
        if !matches!(
            name,
            "after"
                | "before"
                | "throughAfter"
                | "throughBefore"
                | "recursiveAfter"
                | "recursiveBefore"
        ) {
            continue;
        }
        let certificate = proofs[key].certificate().unwrap();
        assert!(!certificate.actual_definitions.is_empty());
        assert_eq!(certificate.completed.len(), 1);
        let joined = certificate.completed.iter().next().unwrap();
        assert_eq!(
            certificate.ordered_definitions.0.contains_key(joined),
            matches!(name, "after" | "throughAfter" | "recursiveAfter")
        );
        checked.insert(name.to_owned());
    }
    assert_eq!(checked.len(), 6);
}
