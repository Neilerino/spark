// TAG [SPARK-114]

use super::*;
use spark_mir::{lower, MirInput, MirOutput};
use spark_modules::ModuleGraph;
use std::collections::HashMap;

#[test]
fn certificates_are_closed_against_their_own_refined_may_summaries() {
    let output = lower_source(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_helper_proof_closure.spark"
    )));
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
    let program = Program {
        mir: &output.mir,
        table: &output.table,
        facts: &facts,
        definitions: Some(&output.definitions),
    };
    let (rechecked, discovered, requests) = program.proofs(&summaries, &ports, Some(&proofs));
    assert_eq!(proofs, rechecked);
    assert_eq!(ports, discovered);
    assert!(requests.iter().all(|key| summaries.contains_key(key)));
}

#[test]
fn task_queue_writes_are_exported_even_when_the_task_never_returns() {
    let output = lower_source(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_queue_prefix_before_divergence.spark"
    )));
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
    for name in ["publishForever", "launch"] {
        let matching: Vec<_> = summaries
            .iter()
            .filter(|(key, _)| {
                output
                    .definitions
                    .get(key.function)
                    .is_some_and(|definition| definition.name.as_str() == name)
            })
            .collect();
        assert!(!matching.is_empty(), "{name}");
        for (_, summary) in matching {
            assert!(!summary.queues.is_empty(), "{name}");
            assert_eq!(summary.normal_return, name == "launch", "{name}");
        }
    }
}

#[test]
fn task_created_queues_keep_their_identity_across_observation_points() {
    use super::super::CallActuals;
    use crate::{
        facts::Point,
        provenance::{
            channels::{OutputStorage, Target},
            value::child_type,
            Slot, Value,
        },
    };
    use spark_mir::BlockId;
    let output = lower_source(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_private_returned_queue_scoped.spark"
    )));
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
                == "main"
        })
        .unwrap();
    let mut flow = Flow::new(function, &output.table, Some(context));
    flow.solve(&facts[&function.def_id]);
    let state = flow
        .entries
        .iter()
        .flatten()
        .find(|state| !state.tasks.is_empty())
        .unwrap()
        .clone();
    let (target, record) = state.tasks.iter().next().unwrap();
    let inner = child_type(target.ty(), &Slot::Element, &output.table).unwrap();
    let first = flow.task_facts(record, target, Point::new(BlockId::ENTRY, 0), &state, inner);
    let later = flow.task_facts(record, target, Point::new(BlockId::ENTRY, 1), &state, inner);
    let targets = |value: &Value| {
        value
            .queues
            .values()
            .flatten()
            .map(|handle| handle.target.clone())
            .collect::<FxHashSet<_>>()
    };
    let first_targets = targets(&first.returned);
    assert!(!first_targets.is_empty());
    assert_eq!(first_targets, targets(&later.returned));
    let site = target.instantiation_site().unwrap();
    assert!(first_targets.iter().all(|target| matches!(target,
        Target::Allocation { function, point, constructor: Some(constructor), .. }
            if *function == site.function && *point == site.point
                && constructor.role.as_ref().is_some_and(|role| role.storage == OutputStorage::Return)
    )));

    let callee = Value::default();
    let queue = first_targets.iter().next().unwrap();
    let earlier_point = Point::new(BlockId::ENTRY, 0);
    let later_point = Point::new(BlockId::ENTRY, 1);
    let actuals = CallActuals {
        callee: &callee,
        arguments: &[],
        ty: inner,
        point: earlier_point,
        instantiation_site: None,
        state: &state,
        aliases: None,
    };
    let ordinary_earlier = flow.bind_queue_target(queue, function, actuals);
    let ordinary_later = flow.bind_queue_target(
        queue,
        function,
        CallActuals {
            point: later_point,
            ..actuals
        },
    );
    assert_ne!(ordinary_earlier, ordinary_later);
    let stable_later = flow.bind_queue_target(
        queue,
        function,
        CallActuals {
            point: later_point,
            instantiation_site: Some(site),
            ..actuals
        },
    );
    assert_eq!(stable_later, first_targets);
    let ordinary_future = flow.bind_task_target(target, function, actuals);
    let later_future = flow.bind_task_target(
        target,
        function,
        CallActuals {
            point: later_point,
            ..actuals
        },
    );
    assert_ne!(ordinary_future, later_future);
    let stable_future = flow.bind_task_target(
        target,
        function,
        CallActuals {
            point: later_point,
            instantiation_site: Some(site),
            ..actuals
        },
    );
    assert_eq!(stable_future.len(), 1);
    assert_eq!(&stable_future.iter().next().unwrap().target, target);

    assert!(!first.prefix.queues.is_empty());
    assert_eq!(
        first
            .prefix
            .queues
            .iter()
            .map(|write| write.target.clone())
            .collect::<FxHashSet<_>>(),
        later
            .prefix
            .queues
            .iter()
            .map(|write| write.target.clone())
            .collect::<FxHashSet<_>>(),
    );
}

fn make_graph(file_id: spark_span::FileId, ast: &spark_ast::Ast) -> ModuleGraph {
    use spark_ast::Item;
    use spark_modules::{ModuleInfo, ModulePath};

    let module_path = ModulePath::from_single(spark_intern::sym("test"));
    let mut graph = ModuleGraph::default();
    let mut module_info = ModuleInfo::new(module_path.clone());
    module_info.files.push(file_id);
    for item in &ast.items {
        let name = match item {
            Item::Function(f) => Some(f.name.name),
            Item::Struct(s) => Some(s.name.name),
            Item::Enum(e) => Some(e.name.name),
            Item::Interface(i) => Some(i.name.name),
            Item::TypeAlias(t) => Some(t.name.name),
            Item::TypeDef(t) => Some(t.name.name),
            Item::Const(c) => Some(c.name.name),
            Item::Binding(_) => None,
        };
        if let Some(n) = name {
            module_info.exports.insert(n);
        }
    }
    graph.modules.insert(module_path.clone(), module_info);
    graph.compilation_order.push(module_path);
    graph
}

fn lower_source(source: &str) -> MirOutput {
    use spark_lexer::Lexer;
    use spark_parser::parse;
    use spark_resolve::{resolve, ResolverInput};
    use spark_span::FileId;
    use spark_types::{check, TypeCheckerInput};

    let file_id = FileId::new(0);

    let tokens = Lexer::new(file_id, source).tokenize();
    let (ast, diagnostics) = parse(tokens, file_id);
    assert!(!diagnostics.has_errors());
    let graph = make_graph(file_id, &ast);
    let mut asts = HashMap::new();
    asts.insert(file_id, ast);

    let resolver_output = resolve(ResolverInput { graph, asts });

    assert!(!resolver_output.diagnostics.has_errors());
    let type_output = check(TypeCheckerInput {
        resolved: &resolver_output.resolved,
        definitions: &resolver_output.definitions,
        graph: ModuleGraph::default(),
    });

    assert!(!type_output.diagnostics.has_errors());
    let hir_output = spark_hir::lower(spark_hir::HirInput {
        resolved: resolver_output.resolved,
        definitions: resolver_output.definitions,
        table: type_output.table,
        builtins: type_output.builtins,
    });

    assert!(!hir_output.diagnostics.has_errors());
    lower(MirInput {
        hir: hir_output.hir,
        table: hir_output.table,
        definitions: hir_output.definitions,
    })
}
