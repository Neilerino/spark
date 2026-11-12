// TAG [SPARK-114]

use super::*;

fn check_producer(source: &str, expected: bool) {
    let output = lower_source(source);
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
    let mut checked = 0;
    for (key, summary) in &summaries {
        if output.definitions.get(key.function).unwrap().name.as_str() != "produce" {
            continue;
        }
        let recipes = proofs[key]
            .certificate()
            .map(|certificate| &certificate.fresh_messages);
        assert_eq!(
            recipes.is_some_and(|recipes| !recipes.is_empty()),
            expected,
            "{source}\nnormal return: {}",
            summary.normal_return
        );
        if expected {
            let recipes = recipes.unwrap();
            assert_eq!(recipes.len(), 1);
            let recipe = recipes.iter().next().unwrap();
            let payload = &summary.fresh_outputs[recipe];
            let record = recipe.birth.record(payload).unwrap();
            assert_eq!(record.callback.callees[&Vec::new()].known.len(), 1);
        }
        checked += 1;
    }
    assert!(checked > 0);
}

fn program(body: &str, result: &str, extra: &str) -> String {
    format!("async fn work(): Int {{ return 7 }}\nfn produce(out: Channel[Future[Int]], flag: Bool): {result} {{\n{body}\n}}\n{extra}\nfn main(): Int {{ return 0 }}")
}

const SEND: &str = "if true { child: Future[Int] = spawn { work() } out.send(child) }";

#[test]
fn fresh_output_certificates_require_single_send_on_every_normal_return() {
    check_producer(&program(&format!("{SEND}\nreturn 0"), "Int", ""), true);
    check_producer(
        &program(
            "if flag { child: Future[Int] = spawn { work() } out.send(child) return 0 } return 0",
            "Int",
            "",
        ),
        false,
    );
    check_producer(
        &program(&format!("{SEND}\n{SEND}\nreturn 0"), "Int", ""),
        false,
    );
    check_producer(
        &program(
            &format!("loop {{ {SEND} if flag {{ break }} }} return 0"),
            "Int",
            "",
        ),
        false,
    );
    check_producer(
        &program(&format!("{SEND}\nloop {{ }} return 0"), "Int", ""),
        false,
    );
}

#[test]
fn destructive_and_reference_access_deny_fresh_output_certificates() {
    for tail in [
        "received: Future[Int] = out.recv()",
        "out.close()",
        "borrowed: Ref[Channel[Future[Int]]] = out",
    ] {
        check_producer(
            &program(&format!("{SEND}\n{tail}\nreturn 0"), "Int", ""),
            false,
        );
    }
    check_producer(&program(&format!("{SEND}\nconsumed: Int = consume(out)\nreturn 0"), "Int", "fn consume(input: Channel[Future[Int]]): Int { child: Future[Int] = input.recv() return 0 }"), false);
}

#[test]
fn channel_payloads_cannot_hide_escaping_or_deferred_queue_access() {
    let boxed =
        "box: Channel[Channel[Future[Int]]] = Channel[Channel[Future[Int]]]() box.send(out)";
    check_producer(
        &program(
            &format!("{boxed}\n{SEND}\nreturn box"),
            "Channel[Channel[Future[Int]]]",
            "",
        ),
        false,
    );
    check_producer(&program(&format!("{boxed}\n{SEND}\nconsumer: Future[Int] = spawn {{ consume_box(box) }}\nreturn 0"), "Int", "async fn consume_box(box: Channel[Channel[Future[Int]]]): Int { handle: Channel[Future[Int]] = box.recv() task: Future[Int] = handle.recv() return 0 }"), false);
}

#[test]
fn a_formal_future_with_hidden_callback_access_cannot_certify_an_output() {
    for tail in ["joined: Int = await hidden\nreturn 0", "return hidden"] {
        let result = if tail == "return hidden" {
            "Future[Int]"
        } else {
            "Int"
        };
        let source = program(&format!("{SEND}\n{tail}"), result, "")
            .replace("flag: Bool)", "flag: Bool, hidden: Future[Int])");
        check_producer(&source, false);
    }
}

#[test]
fn opaque_callable_environments_cannot_hide_deferred_or_returned_access() {
    let source = program(
        &format!("{SEND}\nworker: Future[Int] = spawn {{ action() }}\nreturn 0"),
        "Int",
        "",
    )
    .replace("flag: Bool)", "flag: Bool, action: (): Future[Int])");
    check_producer(&source, false);
    let source = program(&format!("{SEND}\nreturn action"), "(): Int", "")
        .replace("flag: Bool)", "flag: Bool, action: (): Int)");
    check_producer(&source, false);
}
