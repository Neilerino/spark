// TAG [SPARK-114]

use super::*;
use crate::provenance::channels::{counts::Count, Target};
use spark_mir::Local;
use spark_types::Ty;

fn with_counts(source: &str, run: impl FnOnce(&MirOutput, &Summaries)) {
    let output = lower_source(source);
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
    run(&output, &summaries);
}

#[test]
fn normal_queue_send_counts_distinguish_calls_branches_and_repeated_executions() {
    with_counts(
        r#"
fn once(queue: Channel[Int]): Int {
    queue.send(1)
    return 0
}
fn twice(queue: Channel[Int]): Int {
    once(queue)
    once(queue)
    return 0
}
fn always(queue: Channel[Int]): Int {
    if true { queue.send(1) }
    return 0
}
fn never(queue: Channel[Int]): Int {
    if false { queue.send(1) }
    return 0
}
fn both(left: Channel[Int], right: Channel[Int]): Int {
    left.send(1)
    right.send(2)
    return 0
}
fn aliased(queue: Channel[Int]): Int {
    both(queue, queue)
    return 0
}
fn opaque(queue: Channel[Int], callback: (Channel[Int]): Int): Int {
    callback(queue)
    return 0
}
fn grounded(queue: Channel[Int], flag: Bool): Int {
    if flag {
        queue.send(1)
        return 0
    }
    result: Int = grounded(queue, flag)
    return result
}
fn recursiveNever(queue: Channel[Int]): Int {
    queue.send(1)
    result: Int = recursiveNever(queue)
    return result
}
fn conditional(queue: Channel[Int], flag: Bool): Int {
    if flag { queue.send(1) }
    return 0
}
fn repeated(queue: Channel[Int], flag: Bool): Int {
    loop {
        if flag == false { break }
        queue.send(1)
    }
    return 0
}
fn repeatedHelper(queue: Channel[Int], flag: Bool): Int {
    loop {
        if flag == false { break }
        once(queue)
    }
    return 0
}
fn forever(queue: Channel[Int]): Int {
    queue.send(1)
    loop {}
    return 0
}
"#,
        |output, summaries| {
            for (name, expected) in [
                ("once", Count::ONE),
                ("twice", Count::MANY),
                ("always", Count::ONE),
                ("never", Count::ZERO),
                ("aliased", Count::MANY),
                ("opaque", Count::UNKNOWN),
                ("grounded", Count::ONE),
                ("repeatedHelper", Count::UNKNOWN),
                ("conditional", Count::ZERO.union(Count::ONE)),
                ("repeated", Count::UNKNOWN),
            ] {
                let def = output
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
                    .def_id;
                let summary = &summaries[&Key::distinct(def)];
                assert!(summary.normal_return, "{name}");
                let counts = summary
                    .normal_queue_sends
                    .as_ref()
                    .expect("reachable normal returns need grounded send counts");
                let channel = output.mir.functions[&def].locals[1].ty;
                let target = Target::Input {
                    capture: None,
                    queue: None,
                    local: Local(1),
                    path: Vec::new(),
                    ty: channel,
                };
                assert_eq!(counts.get(&target), expected, "{name}");
            }
            for name in ["forever", "recursiveNever"] {
                let def = output
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
                    .def_id;
                let summary = &summaries[&Key::distinct(def)];
                assert!(!summary.normal_return, "{name}");
                assert!(
                    summary.normal_queue_sends.is_none(),
                    "NoReturn is not a vacuous zero or one: {name}"
                );
                assert!(
                    !summary.queues.is_empty(),
                    "diverging prefix publication must survive: {name}"
                );
            }
        },
    );
}

#[test]
fn the_retained_child_producer_counts_its_real_send_separately_from_task_samples() {
    with_counts(
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../spark_driver/tests/run-pass/async/future_task_retained_child_memory_scoped.spark"
        )),
        |output, summaries| {
            let actual_producers: FxHashSet<_> = output
                .mir
                .functions
                .values()
                .filter(|function| {
                    function.blocks.iter().any(|block| {
                        let spark_mir::Terminator::Call { func, args, .. } = &block.terminator
                        else {
                            return false;
                        };
                        function
                            .resolve_callee_def_id(func)
                            .and_then(|def| output.definitions.get(def))
                            .is_some_and(|info| info.name == spark_intern::kw::SEND)
                            && args.get(1).is_some_and(|arg| {
                                matches!(
                                    output
                                        .table
                                        .resolve(crate::provenance::value::strip_mutable(
                                            crate::provenance::operand_type(arg, function),
                                            &output.table
                                        )),
                                    Ty::Future { .. }
                                )
                            })
                    })
                })
                .map(|function| function.def_id)
                .collect();
            assert!(!actual_producers.is_empty());
            let mut checked = 0;
            for (_, summary) in summaries
                .iter()
                .filter(|(key, _)| actual_producers.contains(&key.function))
            {
                assert!(summary.normal_return);
                let counts = summary
                    .normal_queue_sends
                    .as_ref()
                    .expect("actual producer normal return");
                let retained: Vec<_> = counts.targets.keys().filter(|target|
                    matches!(output.table.resolve(crate::provenance::value::strip_mutable(target.ty(), &output.table)), Ty::Channel { inner }
                        if matches!(output.table.resolve(*inner), Ty::Future { .. }))).collect();
                assert!(!retained.is_empty());
                assert!(
                    retained
                        .iter()
                        .all(|target| counts.get(target) == Count::ONE),
                    "every returning alias context must count exactly one Future send: {counts:?}"
                );
                checked += 1;
            }
            assert!(checked > 0);
        },
    );
}

#[test]
fn literal_reachability_does_not_assume_user_mutable_or_addressed_slots_are_constants() {
    use spark_mir::{Operand, Rvalue, Statement, Terminator};
    let output = lower_source(
        r#"
fn guarded(): Int {
    flag: Bool = true
    reference: Ref[Bool] = flag
    if true { return 1 }
    return 0
}
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
                .unwrap()
                .name
                .as_str()
                == "guarded"
        })
        .unwrap();
    let (index, local) = function
        .blocks
        .iter()
        .enumerate()
        .find_map(|(index, block)| {
            let Terminator::SwitchInt {
                discriminant: Operand::Copy(place),
                ..
            } = &block.terminator
            else {
                return None;
            };
            Some((index, place.local))
        })
        .unwrap();
    let successor = |function: &spark_mir::MirFunction| {
        crate::provenance::literal_successor(function, &function.blocks[index])
    };
    assert!(successor(function).is_some());
    for mode in 0..3 {
        let mut guarded = function.clone();
        let declaration = &mut guarded.locals[local.index()];
        match mode {
            0 => declaration.is_user_variable = true,
            1 => declaration.mutable = true,
            _ => declaration.name = Some(spark_intern::sym("named")),
        }
        assert!(successor(&guarded).is_none());
    }
    let mut addressed = function.clone();
    let borrowed = addressed
        .blocks
        .iter_mut()
        .flat_map(|block| &mut block.statements)
        .find_map(|statement| {
            if let Statement::Assign {
                rvalue: Rvalue::Ref { place, .. },
                ..
            } = statement
            {
                Some(place)
            } else {
                None
            }
        })
        .expect("the typed fixture provides a real Bool reference shape");
    borrowed.local = local;
    assert!(
        successor(&addressed).is_none(),
        "an addressed temporary must remain conservative"
    );
}
