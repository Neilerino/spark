// TAG [SPARK-114]

use super::*;
use crate::provenance::tasks::proofs::Guarantee;

#[test]
fn normal_helper_cleanup_discharges_only_its_supplied_owner() {
    let output = lower_source(
        r#"
fn release(task: Future[Int]): Int { return 0 }
fn releaseAll(task: Future[Int]): Int {
    ignored: Int = release(task)
    return 0
}
fn preserveOriginal(task: Future[Int]): Future[Int] {
    ignored: Int = release(task)
    return task
}
async fn releaseAsync(task: Future[Int]): Int { return 0 }
fn retainInParent(task: Future[Int]): Future[Int] { return spawn { releaseAsync(task) } }
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
        "release",
        "releaseAll",
        "preserveOriginal",
        "retainInParent",
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
        let Guarantee::Returns(certificate) = flow.future_guarantee() else {
            panic!("{name} must return normally")
        };
        assert!(
            certificate.completed.is_empty(),
            "{name}: release is not a join"
        );
        assert_eq!(
            certificate.released_provided.contains(&spark_mir::Local(1)),
            name != "preserveOriginal",
            "{name}: returning the original transfers its provided owner"
        );
        assert_eq!(
            certificate.retains_no_owner.contains(&spark_mir::Local(1)),
            matches!(name, "release" | "releaseAll"),
            "{name}: a returned parent retains its captured child owner"
        );
        if name == "preserveOriginal" {
            assert!(
                certificate.returned.contains(&spark_mir::Local(1)),
                "the original still names the pending execution"
            );
        }
    }
}
