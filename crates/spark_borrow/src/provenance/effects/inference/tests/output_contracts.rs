// TAG [SPARK-114]

use super::*;
use crate::provenance::tasks::proofs::Guarantee;

#[test]
fn helper_outputs_require_grounded_normal_returns_and_complete_owner_coverage() {
    let output = lower_source(
        r#"
async fn hold(child: Future[Int], release: Channel[Int]): Int { return release.recv() }
fn keepRecursive(child: Future[Int], release: Channel[Int], choose: Bool): Future[Int] {
    if choose { return spawn { hold(child, release) } }
    return keepRecursive(child, release, true)
}
fn keepNever(child: Future[Int], release: Channel[Int]): Future[Int] {
    return keepNever(child, release)
}
fn leaky(child: Future[Int], out: Channel[Future[Int]], release: Channel[Int]): Future[Int] {
    out.send(child)
    return spawn { hold(child, release) }
}
fn chooseParent(first: Future[Int], second: Future[Int], release: Channel[Int], choose: Bool): Future[Int] {
    if choose { return spawn { hold(first, release) } }
    return spawn { hold(second, release) }
}
async fn holdMap(child: Future[Int], held: Map[Int, Future[Int]], release: Channel[Int]): Int { return release.recv() }
fn mapCaptured(child: Future[Int], held: Map[Int, Future[Int]], release: Channel[Int]): Future[Int] {
    return spawn { holdMap(child, held, release) }
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
    let context = Context {
        mir: &output.mir,
        summaries: &summaries,
        ports: &ports,
        definitions: Some(&output.definitions),
        proofs: Some(&proofs),
    };
    for name in [
        "keepRecursive",
        "keepNever",
        "leaky",
        "chooseParent",
        "mapCaptured",
    ] {
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
                    == name
            })
            .unwrap();
        let mut flow = Flow::new(function, &output.table, Some(context));
        flow.solve(&facts[&function.def_id]);
        let guarantee = flow.future_guarantee();
        if name == "keepNever" {
            assert_eq!(guarantee, Guarantee::NoReturn);
            continue;
        }
        let Guarantee::Returns(certificate) = guarantee else {
            panic!("{name} must return normally");
        };
        assert!(
            certificate.completed.is_empty(),
            "a returned parent is not a child join"
        );
        assert_eq!(
            certificate.returned_owners.is_some(),
            name == "keepRecursive",
            "{name}: only a grounded, complete output may preserve owner coverage"
        );
        if name == "keepRecursive" {
            let mut invalid = function.clone();
            invalid.return_type = match output.table.resolve(function.return_type) {
                spark_types::Ty::Future { inner } => *inner,
                _ => panic!("expected a Future return type"),
            };
            let callbacks = &summaries[&Key::distinct(function.def_id)].returned_callbacks;
            assert!(
                crate::provenance::tasks::outputs::CallbackRecipe::candidate(
                    callbacks,
                    &invalid,
                    &output.mir,
                    &output.table,
                )
                .is_none(),
                "a callback graph requires a Future parent result"
            );
        }
        if name == "leaky" {
            assert!(certificate.released_provided.contains(&spark_mir::Local(1)));
            assert!(
                !certificate.retains_no_owner.contains(&spark_mir::Local(1)),
                "the sent copy remains outside the returned callback"
            );
        }
    }
    let mut events = proofs
        .iter()
        .map(|(key, proof)| (key.clone(), proof.events.keys().cloned().collect()))
        .collect();
    let mut vocabulary = OutputVocabulary::default();
    discover_summary_outputs(&mut vocabulary, &summaries);
    let program = Program {
        mir: &output.mir,
        table: &output.table,
        facts: &facts,
        definitions: Some(&output.definitions),
    };
    let (rechecked, _, _, grew) = program.proofs(
        &summaries,
        &ports,
        &mut events,
        &mut vocabulary,
        Some(&proofs),
    );
    assert!(!grew);
    assert_eq!(
        rechecked, proofs,
        "output contracts must remain valid against their own refined MAY summaries"
    );
}
