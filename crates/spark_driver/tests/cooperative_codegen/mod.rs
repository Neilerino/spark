// TAG [SPARK-154]

//! Tests native coroutine dispatch through the complete compiler pipeline.

use object::{Object, ObjectSection, ObjectSymbol, RelocationTarget};
use spark_opt::OptLevel;

#[allow(dead_code)] // This object-only harness does not run the linker.
#[path = "../../src/pipeline.rs"]
mod pipeline;

fn calls_in_function(bytes: &[u8], name: &str) -> Vec<String> {
    let file = object::File::parse(bytes).expect("parse native object");
    let function = file
        .symbols()
        .find(|symbol| {
            symbol.name().is_ok_and(|actual| {
                actual == name
                    || actual.strip_prefix('_') == Some(name)
                    || actual.ends_with(&format!("::{name}"))
            })
        })
        .unwrap_or_else(|| panic!("missing function {name}"));
    let section_index = function.section_index().expect("function section");
    let section = file
        .section_by_index(section_index)
        .expect("read function section");
    let start = function.address();
    let end = if function.size() > 0 {
        start + function.size()
    } else {
        file.symbols()
            .filter(|symbol| {
                symbol.section_index() == Some(section_index) && symbol.address() > start
            })
            .map(|symbol| symbol.address())
            .min()
            .unwrap_or(section.address() + section.size())
    };
    section
        .relocations()
        .filter_map(|(offset, relocation)| {
            let address = section.address() + offset;
            if !(start..end).contains(&address) {
                return None;
            }
            let RelocationTarget::Symbol(index) = relocation.target() else {
                return None;
            };
            Some(
                file.symbol_by_index(index)
                    .expect("relocation symbol")
                    .name()
                    .expect("symbol name")
                    .to_owned(),
            )
        })
        .collect()
}

fn assert_cooperative_dispatch(opt_level: OptLevel) {
    let source = include_str!("../run-pass/async/future_cooperative_owned_await.spark");
    let output = pipeline::compile(pipeline::CompileInput {
        sources: vec![("main.spark".into(), source.to_owned())],
        root_path: std::env::current_dir().expect("working directory"),
        opt_level,
    })
    .expect("compile coroutine fixture");
    let main_calls = calls_in_function(&output.object_bytes, "main");
    assert!(
        main_calls
            .iter()
            .any(|name| name.ends_with("__spark_spawn_coop")),
        "{opt_level:?}: main must dispatch the certified relay cooperatively; calls={main_calls:?}"
    );
    assert!(
        main_calls
            .iter()
            .any(|name| name.ends_with("__spark_spawn")),
        "{opt_level:?}: the child without await remains a blocking callback"
    );
    let poll_calls = calls_in_function(&output.object_bytes, "__poll_relay");
    assert!(
        poll_calls
            .iter()
            .any(|name| name.ends_with("__spark_try_poll")),
        "{opt_level:?}: the generated relay must poll its child; calls={poll_calls:?}"
    );
}

#[test]
fn cooperative_spawn_binding_optimized() {
    assert_cooperative_dispatch(OptLevel::Basic);
}

#[test]
fn cooperative_spawn_binding_no_opt() {
    assert_cooperative_dispatch(OptLevel::None);
}

#[test]
fn cooperative_spawn_bindings_and_effectful_fallback_select_the_correct_routes() {
    for opt_level in [OptLevel::Basic, OptLevel::None] {
        for (source, functions, cooperative) in [
            (
                include_str!("../run-pass/async/future_cooperative_bindings.spark"),
                &["spawnReversed", "spawnRepeated"][..],
                true,
            ),
            (
                include_str!("../run-pass/async/future_cooperative_float.spark"),
                &["main"][..],
                true,
            ),
            (
                include_str!("../run-pass/async/future_cooperative_empty_child.spark"),
                &["main"][..],
                true,
            ),
            (
                include_str!("../run-pass/async/future_cooperative_frame_reference.spark"),
                &["main"][..],
                true,
            ),
            (
                include_str!("../run-pass/async/future_cooperative_frame_places.spark"),
                &["main"][..],
                true,
            ),
            (
                include_str!("../run-pass/async/future_cooperative_frame_capture.spark"),
                &["main"][..],
                true,
            ),
            (
                include_str!("../run-pass/async/future_cooperative_frame_indexed.spark"),
                &["main"][..],
                true,
            ),
            (
                include_str!("../run-pass/async/future_cooperative_frame_cow.spark"),
                &["main"][..],
                true,
            ),
            (
                include_str!("../run-pass/async/future_spawn_effectful_wrapper.spark"),
                &["main"][..],
                false,
            ),
        ] {
            let output = pipeline::compile(pipeline::CompileInput {
                sources: vec![("main.spark".into(), source.to_owned())],
                root_path: std::env::current_dir().expect("working directory"),
                opt_level,
            })
            .expect("compile spawn binding fixture");
            for function in functions {
                let calls = calls_in_function(&output.object_bytes, function);
                assert_eq!(
                    calls
                        .iter()
                        .any(|name| name.ends_with("__spark_spawn_coop")),
                    cooperative,
                    "{opt_level:?} {function}: calls={calls:?}"
                );
                if !cooperative {
                    assert!(calls.iter().any(|name| name.ends_with("__spark_spawn")));
                }
            }
        }
    }
}
