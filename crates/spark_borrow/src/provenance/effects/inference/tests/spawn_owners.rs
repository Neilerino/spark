// TAG [SPARK-114]

use super::*;
use crate::provenance::tasks::Role;
use crate::{
    facts::Point,
    provenance::{Origins, State},
};
use spark_mir::{
    AggregateKind, BlockId, Local, LocalDecl, Operand, Place, Rvalue, Statement, Terminator,
};
use spark_span::Span;
use spark_types::Ty;

const KEEPER: &str = r#"
struct Text { value: Ref[String] }
async fn wait(text: Text, input: Channel[Int]): Int {
    return input.recv()
}
async fn hold(child: Future[Int], release: Channel[Int]): Int { return release.recv() }
fn main(): Int {
    input: Channel[Int] = Channel[Int]()
    release: Channel[Int] = Channel[Int]()
    text: String = " borrowed ".trim()
    borrowed: Ref[String] = text
    shared: Text = Text { value: borrowed }
    child: Future[Int] = spawn { wait(shared, input) }
    keeper: Future[Int] = spawn { hold(child, release) }
    result: Int = await child
    return result
}
"#;

fn with_keeper(
    source: &str,
    run: impl FnOnce(&mut Flow<'_>, State, Local, Local, Role, Vec<Local>, Origins),
) {
    let output = lower_source(source);
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
    let callback = output
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
    let (capture_index, (_, child_ty)) = callback
        .captures
        .iter()
        .enumerate()
        .find(|(_, (_, ty))| matches!(output.table.resolve(*ty), Ty::Future { .. }))
        .unwrap();
    let mut function = output
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
        .unwrap()
        .clone();
    let child = function
        .locals
        .iter()
        .find(|decl| decl.name.is_some_and(|name| name.as_str() == "child"))
        .unwrap()
        .local;
    let future_ty = function.locals[child.index()].ty;
    let extras: Vec<_> = (0..3)
        .map(|_| {
            let local = Local(function.locals.len() as u32);
            function.locals.push(LocalDecl {
                local,
                ty: future_ty,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            });
            local
        })
        .collect();
    let (index, body) = function.blocks.iter().enumerate().find(|(_, body)| body.statements.iter().any(|statement| matches!(statement, Statement::Assign { rvalue: Rvalue::Aggregate(AggregateKind::Closure { func_def_id }, _), .. } if *func_def_id == callback.def_id))).unwrap();
    let Terminator::Call { destination, .. } = &body.terminator else {
        panic!("expected keeper spawn")
    };
    let mut flow = Flow::new(&function, &output.table, Some(context));
    flow.solve(&facts[&function.def_id]);
    let mut state = flow.entries[index].clone().unwrap();
    assert!(flow.transfer_block(BlockId(index as u32), &mut state));
    let borrowed_sources = state.value(child).origins();
    assert!(
        !borrowed_sources.is_empty(),
        "the child carries an actual borrowed String source"
    );
    assert!(
        borrowed_sources.is_subset(&state.value(destination.local).origins()),
        "the parent retains the child source backing while its callback is pending"
    );
    let callback_ty = state
        .executions
        .keys()
        .find(|(local, _)| *local == destination.local)
        .unwrap()
        .1;
    let captured = Role::capture(destination.local, callback.def_id, capture_index, *child_ty)
        .in_execution(destination.local, callback_ty)
        .unwrap();
    run(
        &mut flow,
        state,
        destination.local,
        child,
        captured,
        extras,
        borrowed_sources,
    );
}

fn assign(flow: &mut Flow<'_>, state: &mut State, destination: Local, operand: Operand, step: u32) {
    flow.transfer_statement(
        &Statement::Assign {
            place: Place::from_local(destination),
            rvalue: Rvalue::Use(operand),
            span: Span::DUMMY,
        },
        Point::new(BlockId::ENTRY, step),
        state,
    );
}
fn drop(flow: &mut Flow<'_>, state: &mut State, local: Local, step: u32) {
    flow.transfer_statement(
        &Statement::Drop {
            place: Place::from_local(local),
            span: Span::DUMMY,
        },
        Point::new(BlockId::ENTRY, step),
        state,
    );
}
fn moved(local: Local) -> Operand {
    Operand::Move(Place::from_local(local))
}
fn copied(local: Local) -> Operand {
    Operand::Copy(Place::from_local(local))
}

#[test]
fn spawning_a_keeper_transfers_its_captured_child_owner_to_the_execution() {
    with_keeper(KEEPER, |_, state, _, child, captured, _, _| {
        assert!(state.future_copies.witness(child).peers.contains(&captured));
        assert!(state.future_owners.known(child));
        assert!(state.future_owners.known_role(&captured));
    });
}

#[test]
fn parent_handle_aliases_rehome_one_execution_graph_and_preserve_borrow_sources() {
    with_keeper(
        KEEPER,
        |flow, mut state, parent, child, captured, extras, origins| {
            let [alias, moved_parent, _] = extras[..] else {
                unreachable!()
            };
            assign(flow, &mut state, alias, copied(parent), 100);
            assert_eq!(
                state.executions.len(),
                1,
                "handle Copy must not copy physical child owners"
            );
            assert_eq!(state.future_copies.witness(child).peers.len(), 2);
            drop(flow, &mut state, parent, 101);
            let role = captured.reanchor(alias);
            assert!(state.future_copies.witness(child).peers.contains(&role));
            assign(flow, &mut state, alias, moved(alias), 102);
            assign(flow, &mut state, moved_parent, moved(alias), 103);
            let role = captured.reanchor(moved_parent);
            assert!(state.future_copies.witness(child).peers.contains(&role));
            assert_eq!(state.executions.len(), 1);
            assert!(origins.is_subset(
                &state.executions[&(moved_parent, role.callback_type().unwrap())].origins()
            ));
            drop(flow, &mut state, child, 104);
            assert!(state.future_owners.known_role(&role));
            drop(flow, &mut state, moved_parent, 105);
            assert!(state.executions.is_empty());
            assert!(
                state
                    .residuals
                    .values()
                    .any(|value| origins.is_subset(&value.origins())
                        && value
                            .futures
                            .values()
                            .flatten()
                            .any(|handle| handle.pending)),
                "unproved callback cleanup must retain its child activity and sources"
            );
        },
    );
}

#[test]
fn completing_a_parent_without_a_cleanup_certificate_keeps_the_child_pending() {
    with_keeper(KEEPER, |flow, mut state, parent, child, _, _, origins| {
        flow.complete_future(
            parent,
            &Value::default(),
            Point::new(BlockId::ENTRY, 100),
            &mut state,
        );
        assert!(state.executions.is_empty());
        assert!(!state.future_owners.known(child));
        assert!(state
            .value(child)
            .futures
            .values()
            .flatten()
            .any(|handle| handle.pending));
        assert!(state
            .residuals
            .values()
            .any(|value| origins.is_subset(&value.origins())));
    });
}

#[test]
fn replacing_a_retired_parent_coordinate_does_not_rebind_its_callback() {
    with_keeper(
        KEEPER,
        |flow, mut state, parent, child, captured, extras, _| {
            let alias = extras[0];
            assign(flow, &mut state, alias, copied(parent), 100);
            drop(flow, &mut state, parent, 101);
            assign(flow, &mut state, parent, copied(child), 102);
            assert!(!state.future_copies.witness(alias).contains_local(parent));
            assert!(state
                .future_copies
                .witness(child)
                .peers
                .contains(&captured.reanchor(alias)));
            assert_eq!(state.executions.len(), 1);
            assert!(state.executions.keys().all(|(local, _)| *local == alias));
        },
    );
}

#[test]
fn explicit_child_completion_updates_the_adopted_capture() {
    with_keeper(KEEPER, |flow, mut state, _, child, _, _, _| {
        flow.complete_future(
            child,
            &Value::default(),
            Point::new(BlockId::ENTRY, 100),
            &mut state,
        );
        assert!(
            state
                .executions
                .values()
                .flat_map(|callback| callback.futures.values().flatten())
                .all(|handle| !handle.pending),
            "the exact captured alias must observe child completion"
        );
    });
}

#[test]
fn rehoming_two_captured_aliases_without_root_owners_preserves_their_equality() {
    let source = KEEPER
        .replace(
            "hold(child: Future[Int], release:",
            "hold(child: Future[Int], other: Future[Int], release:",
        )
        .replace(
            "    keeper: Future[Int]",
            "    childAlias: Future[Int] = child\n    keeper: Future[Int]",
        )
        .replace("hold(child, release)", "hold(child, childAlias, release)")
        .replace(
            "    return result",
            "    otherResult: Int = await childAlias\n    return result",
        );
    with_keeper(
        &source,
        |flow, mut state, parent, child, captured, extras, _| {
            let roots: Vec<_> = state.future_copies.witness(child).locals().collect();
            for (index, root) in roots.into_iter().enumerate() {
                drop(flow, &mut state, root, 100 + index as u32);
            }
            assert_eq!(
                state.future_copies.role_peers(&captured).len(),
                2,
                "only the two physical capture owners remain"
            );
            let alias = extras[0];
            assign(flow, &mut state, alias, copied(parent), 110);
            assert_eq!(state.executions.len(), 1);
            drop(flow, &mut state, parent, 111);
            assert_eq!(
                state
                    .future_copies
                    .role_peers(&captured.reanchor(alias))
                    .len(),
                2,
                "entry capture equality must survive when the shared source anchor disappears"
            );
        },
    );
}

#[test]
fn merging_alternative_representatives_preserves_both_callback_sources() {
    with_keeper(KEEPER, |flow, mut state, parent, _, captured, extras, _| {
        let alias = extras[0];
        let ty = captured.callback_type().unwrap();
        assign(flow, &mut state, alias, copied(parent), 100);
        let mut left = state.clone();
        let mut right = state;
        let left_origin = crate::provenance::Origin::Loan(Point::new(BlockId::ENTRY, 200));
        let right_origin = crate::provenance::Origin::Loan(Point::new(BlockId::ENTRY, 201));
        let mut path = captured.selection().to_vec();
        path.push(crate::provenance::Slot::Tail);
        for (branch, origin) in [
            (&mut left, left_origin.clone()),
            (&mut right, right_origin.clone()),
        ] {
            branch
                .executions
                .get_mut(&(parent, ty))
                .unwrap()
                .join(&Value::reference(origin).prefixed_path(&path, ty, flow.table));
        }
        drop(flow, &mut left, parent, 101);
        assign(flow, &mut left, parent, copied(alias), 102);
        left.join(&right);
        assert_eq!(left.executions.len(), 2);
        assert!(left.future_copies.witness(parent).contains_local(alias));
        drop(flow, &mut left, parent, 103);
        let origins = left.executions[&(alias, ty)].origins();
        assert!(
            origins.contains(&left_origin),
            "rehoming must preserve the destination branch's backed source"
        );
        assert!(
            origins.contains(&right_origin),
            "rehoming must preserve the source branch's backed source"
        );
    });
}

#[test]
fn adopting_a_child_with_unproved_coverage_does_not_grant_cleanup_rights() {
    let source = KEEPER.replace(
        "    return input.recv()",
        "    token: Int = input.recv()\n    return text.value.len()",
    );
    with_keeper(&source, |_, state, _, child, captured, _, _| {
        assert!(!state.future_owners.known(child));
        assert!(!state.future_owners.known_role(&captured));
        assert!(
            state.future_copies.witness(child).peers.contains(&captured),
            "known transfer preserves equality without inventing closed coverage"
        );
    });
}
