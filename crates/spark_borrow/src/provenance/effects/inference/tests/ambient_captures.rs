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
    }
}
