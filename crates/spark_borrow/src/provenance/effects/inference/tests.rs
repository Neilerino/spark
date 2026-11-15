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
