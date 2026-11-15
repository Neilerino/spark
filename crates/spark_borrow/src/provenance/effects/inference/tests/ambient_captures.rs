// TAG [SPARK-114]

use super::*;
use crate::{facts::Point, provenance::tasks::Role};
use spark_mir::{BlockId, Local, Operand, Rvalue, Statement, Terminator};
use spark_types::Ty;

const AMBIENT: &str = r#"
async fn value(): Int { return 1 }
async fn hold(child: Future[Int], release: Channel[Int]): Int { return release.recv() }
fn main(): Int {
    child: Future[Int] = spawn { value() }
    release: Channel[Int] = Channel[Int]()
    keeper: Future[Int] = spawn { hold(child, release) }
    return 0
}
"#;

#[test]
fn a_borrowed_callback_capture_creates_lineage_only_for_owning_copies() {
    use crate::provenance::tasks::lifecycle::Token;
    let output = lower_source(AMBIENT);
    assert!(!output.diagnostics.has_errors());
    let function = output
        .mir
        .functions
        .values()
        .find(|function| {
            function
                .captures
                .iter()
                .any(|(_, ty)| matches!(output.table.resolve(*ty), Ty::Future { .. }))
        })
        .unwrap();
    let index = function
        .captures
        .iter()
        .position(|(_, ty)| matches!(output.table.resolve(*ty), Ty::Future { .. }))
        .unwrap();
    let capture = Local(1 + function.param_count + index as u32);
    let mut flow = Flow::new(function, &output.table, None);
    let mut state = flow.initial_state();
    assert!(
        state.input_owners.tags(&Role::root(capture)).is_empty(),
        "a capture local borrows the environment field"
    );
    assert!(
        state.future_copies.inputs(capture).contains(&capture),
        "the borrowed capture needs symbolic execution identity without an owned token"
    );
    let body = &function.blocks[0];
    let mut copied = None;
    for (index, statement) in body.statements.iter().enumerate() {
        let point = Point::new(BlockId::ENTRY, index as u32);
        flow.ensure_transfer(point, &state);
        flow.transfer_statement(statement, point, &mut state);
        if let Statement::Assign {
            place,
            rvalue: Rvalue::Use(Operand::Copy(source)),
            ..
        } = statement
        {
            if source.projections.is_empty() && source.local == capture {
                copied = Some(place.local);
                break;
            }
        }
    }
    let copied = copied.expect("callback supplies an owning Copy of its borrowed capture");
    let tags = state.input_owners.tags(&Role::root(copied));
    assert!(tags.contains(&Token::Derived(capture)));
    assert!(!tags.contains(&Token::Provided(capture)));
    assert!(!state.input_owners.retains_no_owner(capture));
}

#[test]
fn callback_copies_remain_owned_until_their_actual_helper_disposition() {
    let identity = "async fn identity(child: Future[Int]): Future[Int] { return child }";
    let keep = "async fn keep(child: Future[Int], release: Channel[Int]): Future[Int] { return spawn { hold(child, release) } }";
    for (name, invocation, helpers, result_ty, clean) in [
        ("hold", "hold(child, release)", String::new(), "Int", true),
        ("identity", "identity(child)", identity.to_owned(), "Future[Int]", false),
        ("keep", "keep(child, release)", keep.to_owned(), "Future[Int]", false),
        ("publish", "publish(child, out)", "async fn publish(child: Future[Int], out: Channel[Future[Int]]): Int { out.send(child) return 0 }".to_owned(), "Int", false),
        ("maybePublish", "maybePublish(child, out, true)", "async fn maybePublish(child: Future[Int], out: Channel[Future[Int]], choose: Bool): Int { if choose { out.send(child) } return 0 }".to_owned(), "Int", false),
        ("forwardDrop", "forwardDrop(child)", "fn pass(child: Future[Int]): Future[Int] { return child }\nasync fn forwardDrop(child: Future[Int]): Int { copy: Future[Int] = pass(child) return 0 }".to_owned(), "Int", true),
    ] {
        let source = AMBIENT.replace("fn main(): Int {", &format!("{helpers}\nfn main(): Int {{"))
            .replace("keeper: Future[Int] = spawn { hold(child, release) }", &format!("out: Channel[Future[Int]] = Channel[Future[Int]]()\nkeeper: Future[{result_ty}] = spawn {{ {invocation} }}"));
        let output = lower_source(&source);
        assert!(!output.diagnostics.has_errors(), "{name}: {:?}", output.diagnostics);
        let function = output.mir.functions.values().find(|function| {
            !function.captures.is_empty() && function.blocks.iter().any(|block| {
                let Terminator::Call { func, .. } = &block.terminator else { return false };
                function.resolve_callee_def_id(func).and_then(|def| output.definitions.get(def))
                    .is_some_and(|definition| definition.name.as_str() == name)
            })
        }).unwrap();
        let index = function.captures.iter().position(|(_, ty)| matches!(output.table.resolve(*ty), Ty::Future { .. })).unwrap();
        let capture = Local(1 + function.param_count + index as u32);
        let facts = output.mir.functions.iter().map(|(id, function)| (*id, crate::facts::extract_facts(function))).collect();
        let (summaries, ports, proofs) = infer_summaries_with_definitions(&output.mir, &output.table, &facts, Some(&output.definitions));
        let context = Context { mir: &output.mir, summaries: &summaries, ports: &ports, definitions: Some(&output.definitions), proofs: Some(&proofs) };
        let mut flow = Flow::new(function, &output.table, Some(context));
        assert!(flow.initial_state().input_owners.retains_no_owner(capture));
        flow.solve(&facts[&function.def_id]);
        let mut returns = 0;
        for (index, block) in function.blocks.iter().enumerate() {
            if !matches!(block.terminator, Terminator::Return) { continue; }
            let Some(mut state) = flow.entries[index].clone() else { continue; };
            assert!(flow.transfer_block(BlockId(index as u32), &mut state));
            returns += 1;
            assert!(state.input_owners.released_provided(capture));
            assert_eq!(state.input_owners.retains_no_owner(capture), clean, "{name}: only actual owned copies count against the borrowed capture");
        }
        assert!(returns > 0, "{name}");
        let crate::provenance::tasks::proofs::Guarantee::Returns(certificate) = flow.future_guarantee() else { panic!("{name}: expected normal return"); };
        assert_eq!(certificate.clean_ambient_captures.contains(&capture), clean, "{name}: body cleanliness is an all-normal-return guarantee");
        assert_eq!(proofs[&Key::distinct(function.def_id)].certificate().is_some_and(|certificate| certificate.clean_ambient_captures.contains(&capture)), clean, "{name}: the inferred guarantee survives refined MAY rechecking");

    }
}

#[derive(Clone, Copy)]
enum CallableAccess {
    Direct,
    Copied,
    Borrowed,
}

fn assert_callable_result_retains_child(
    expression: &str,
    nested: bool,
    argument: bool,
    access: CallableAccess,
) {
    use crate::provenance::tasks::lifecycle::Token;
    let parameter = if argument { "ignored: Future[Int]" } else { "" };
    let extra = if argument { ", other: Future[Int]" } else { "" };
    let actual = if argument { "other" } else { "" };
    let invocation = match access {
        CallableAccess::Direct => "factory",
        _ => "invoked",
    };
    let parameter_type = if argument { "Future[Int]" } else { "" };
    let copy = match access {
        CallableAccess::Direct => String::new(),
        CallableAccess::Copied => format!("invoked: ({parameter_type}): Future[Int] = factory"),
        CallableAccess::Borrowed => format!("pointer: Ref[({parameter_type}): Future[Int]] = factory\ninvoked: ({parameter_type}): Future[Int] = pointer"),
    };
    let source = format!(
        r#"
async fn hold(child: Future[Int], release: Channel[Int]): Int {{ return release.recv() }}
fn make(child: Future[Int], release: Channel[Int]{extra}): Future[Int] {{
    factory: ({parameter_type}): Future[Int] = fn({parameter}): Future[Int] {{
        gate: Channel[Int] = release
        held: Future[Int] = child
        return {expression}
    }}
    {copy}
    made: Future[Int] = {invocation}({actual})
    return made
}}
fn main(): Int {{ return 0 }}
"#
    );
    let output = lower_source(&source);
    assert!(!output.diagnostics.has_errors());
    let function = output
        .mir
        .functions
        .values()
        .find(|function| {
            output
                .definitions
                .get(function.def_id)
                .is_some_and(|definition| definition.name.as_str() == "make")
        })
        .unwrap();
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
    let factory = function
        .locals
        .iter()
        .find(|decl| decl.name.is_some_and(|name| name.as_str() == invocation))
        .unwrap()
        .local;
    let (block, body) = function
        .blocks
        .iter()
        .enumerate()
        .find(|(_, body)| {
            matches!(&body.terminator,
        Terminator::Call { func: Operand::Copy(place), .. } if place.local == factory)
        })
        .unwrap();
    let Terminator::Call {
        destination,
        target,
        ..
    } = &body.terminator
    else {
        unreachable!()
    };
    let mut flow = Flow::new(function, &output.table, Some(context));
    flow.solve(&facts[&function.def_id]);
    let mut state = flow.entries[block].clone().unwrap();
    for (index, statement) in body.statements.iter().enumerate() {
        let point = Point::new(BlockId(block as u32), index as u32);
        flow.ensure_transfer(point, &state);
        flow.transfer_statement(statement, point, &mut state);
    }
    let targets = &state.value(factory).callees[&vec![]];
    assert_eq!(targets.known.len(), 1);
    let definition = &output.mir.functions[targets.known.iter().next().unwrap()];
    assert_eq!(definition.captures.len(), 2);
    assert!(matches!(
        output.table.resolve(definition.captures[0].1),
        Ty::Channel { .. }
    ));
    let (_, future) = definition.captures[1];
    assert!(matches!(output.table.resolve(future), Ty::Future { .. }));
    let source = if matches!(access, CallableAccess::Borrowed) {
        function
            .locals
            .iter()
            .find(|decl| decl.name.is_some_and(|name| name.as_str() == "factory"))
            .unwrap()
            .local
    } else {
        factory
    };
    let captured = Role::capture(source, definition.def_id, 1, future);
    assert!(state
        .input_owners
        .tags(&captured)
        .contains(&Token::Derived(Local(1))));
    if matches!(access, CallableAccess::Direct) {
        assert!(state.input_owners.retains_only(
            Local(1),
            &rustc_hash::FxHashSet::from_iter([Role::root(Local(1)), captured])
        ));
    }
    assert!(flow.transfer_terminator(BlockId(block as u32), &mut state));
    assert_result_owner(&state, destination.local, nested, argument);
    assert!(flow.transfer_block(*target, &mut state));
    assert_result_owner(&state, Local::RETURN_PLACE, nested, argument);
}

fn assert_result_owner(
    state: &crate::provenance::State,
    local: Local,
    nested: bool,
    argument: bool,
) {
    use crate::provenance::tasks::lifecycle::Token;
    let contains = |tags: &crate::provenance::tasks::lifecycle::Tags| {
        if argument {
            assert!(
                !tags.iter().any(|tag| tag.input() == Local(3)),
                "an unused explicit argument is not the captured child"
            );
        }
        tags.contains(&Token::Derived(Local(1)))
    };
    let retained = if nested {
        state
            .executions
            .keys()
            .filter(|(anchor, _)| *anchor == local)
            .any(|(_, ty)| {
                state
                    .input_owners
                    .execution_outputs(local, *ty)
                    .values()
                    .any(contains)
            })
    } else {
        contains(&state.input_owners.tags(&Role::root(local)))
    };
    assert!(retained, "the returned owner must retain the captured child's mapped lineage independently of old callee escapes");
}

#[test]
fn a_callable_parent_result_keeps_its_captured_child_owner() {
    for argument in [false, true] {
        assert_callable_result_retains_child(
            "spawn { hold(held, gate) }",
            true,
            argument,
            CallableAccess::Direct,
        );
    }
}

#[test]
fn a_callable_child_result_keeps_its_captured_child_owner() {
    for argument in [false, true] {
        assert_callable_result_retains_child("held", false, argument, CallableAccess::Direct);
    }
}

#[test]
fn a_copied_callable_parent_result_keeps_its_captured_child_owner() {
    assert_callable_result_retains_child(
        "spawn { hold(held, gate) }",
        true,
        true,
        CallableAccess::Copied,
    );
}

#[test]
fn a_copied_callable_child_result_keeps_its_captured_child_owner() {
    assert_callable_result_retains_child("held", false, true, CallableAccess::Copied);
}

#[test]
fn a_borrowed_callable_parent_result_keeps_its_captured_child_owner() {
    assert_callable_result_retains_child(
        "spawn { hold(held, gate) }",
        true,
        true,
        CallableAccess::Borrowed,
    );
}

#[test]
fn a_borrowed_callable_child_result_keeps_its_captured_child_owner() {
    assert_callable_result_retains_child("held", false, true, CallableAccess::Borrowed);
}

#[test]
fn a_known_empty_callable_capture_does_not_retain_an_unrelated_formal() {
    let output = lower_source(
        r#"
async fn value(): Int { return 1 }
fn make(unrelated: Future[Int]): Future[Int] {
    fresh: Future[Int] = spawn { value() }
    factory: (): Future[Int] = fn(): Future[Int] { return fresh }
    return factory()
}
fn main(): Int { return 0 }
"#,
    );
    let function = output
        .mir
        .functions
        .values()
        .find(|function| {
            output
                .definitions
                .get(function.def_id)
                .is_some_and(|definition| definition.name.as_str() == "make")
        })
        .unwrap();
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
    let mut flow = Flow::new(function, &output.table, Some(context));
    flow.solve(&facts[&function.def_id]);
    let crate::provenance::tasks::proofs::Guarantee::Returns(certificate) = flow.future_guarantee()
    else {
        panic!("make returns normally")
    };
    assert!(certificate.retains_no_owner.contains(&Local(1)));
}

#[test]
fn captured_owner_exports_revoke_physical_coverage_without_caller_labels() {
    for (expression, result, closed) in [
        ("held", "Future[Int]", false),
        ("spawn { hold(held, gate) }", "Future[Int]", false),
        ("{ queue.send(held) 0 }", "Int", false),
        ("0", "Int", true),
    ] {
        let source = format!(
            r#"
async fn value(): Int {{ return 1 }}
async fn hold(child: Future[Int], release: Channel[Int]): Int {{ return release.recv() }}
fn main(): Int {{
    child: Future[Int] = spawn {{ value() }}
    release: Channel[Int] = Channel[Int]()
    output: Channel[Future[Int]] = Channel[Future[Int]]()
    factory: (): {result} = fn(): {result} {{
        gate: Channel[Int] = release
        held: Future[Int] = child
        queue: Channel[Future[Int]] = output
        return {expression}
    }}
    made: {result} = factory()
    observed: Int = await child
    return 0
}}
"#
        );
        let output = lower_source(&source);
        let function = output
            .mir
            .functions
            .values()
            .find(|function| {
                output
                    .definitions
                    .get(function.def_id)
                    .is_some_and(|definition| definition.name.as_str() == "main")
            })
            .unwrap();
        let child = function
            .locals
            .iter()
            .find(|decl| decl.name.is_some_and(|name| name.as_str() == "child"))
            .unwrap()
            .local;
        let factory = function
            .locals
            .iter()
            .find(|decl| decl.name.is_some_and(|name| name.as_str() == "factory"))
            .unwrap()
            .local;
        let (block, body) = function
            .blocks
            .iter()
            .enumerate()
            .find(|(_, body)| {
                matches!(&body.terminator,
            Terminator::Call { func: Operand::Copy(place), .. } if place.local == factory)
            })
            .unwrap();
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
        let mut flow = Flow::new(function, &output.table, Some(context));
        flow.solve(&facts[&function.def_id]);
        let mut state = flow.entries[block].clone().unwrap();
        for (index, statement) in body.statements.iter().enumerate() {
            flow.transfer_statement(
                statement,
                Point::new(BlockId(block as u32), index as u32),
                &mut state,
            );
        }
        assert!(
            state.future_owners.known(child),
            "the original leaf child starts closed"
        );
        assert!(
            state.input_owners.all_value_tags().is_empty(),
            "main has no formal or ambient labels to poison"
        );
        assert!(flow.transfer_terminator(BlockId(block as u32), &mut state));
        assert_eq!(
            state.future_owners.known(child),
            closed,
            "{expression}: physical capture retention is independent of mapped label emptiness"
        );
    }
}

#[test]
fn a_nonreturning_callback_has_no_capture_cleanup_certificate() {
    let source = AMBIENT.replace("return release.recv()", "loop {} return 0");
    let output = lower_source(&source);
    let function = output
        .mir
        .functions
        .values()
        .find(|function| {
            function
                .captures
                .iter()
                .any(|(_, ty)| matches!(output.table.resolve(*ty), Ty::Future { .. }))
        })
        .unwrap();
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
    let mut flow = Flow::new(function, &output.table, Some(context));
    flow.solve(&facts[&function.def_id]);
    assert_eq!(
        flow.future_guarantee(),
        crate::provenance::tasks::proofs::Guarantee::NoReturn
    );
    assert!(proofs[&Key::distinct(function.def_id)]
        .certificate()
        .is_none());
}
