// TAG [SPARK-114]

use super::*;
use spark_mir::{BlockId, LocalDecl};
use spark_resolve::DefId;
use spark_span::Span;
use spark_types::{IntWidth, TypeTable};

#[test]
fn widened_future_input_selections_keep_type_and_capture_roles_separate() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let string = table.intern(Ty::String);
    let first = table.intern(Ty::Future { inner: int });
    let other = table.intern(Ty::Future { inner: string });
    let callable = table.intern(Ty::Function {
        params: Vec::new(),
        ret: int,
        is_async: false,
    });
    let function = MirFunction {
        def_id: DefId::new(9920),
        param_count: 1,
        return_type: int,
        locals: [int, callable]
            .into_iter()
            .enumerate()
            .map(|(index, ty)| LocalDecl {
                local: super::super::Local(index as u32),
                ty,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            })
            .collect(),
        blocks: Vec::new(),
        captures: Vec::new(),
        is_async: false,
        span: Span::DUMMY,
    };
    let missing_ty = table.intern(Ty::Future { inner: callable });
    let flow = Flow::new(&function, &table, None);
    let source = super::super::super::tasks::Capture {
        owner: DefId::new(9921),
        index: 0,
        ty: first,
    };
    let mut argument = Value::default();
    for (index, ty, capture) in [
        (1, first, Some(source)),
        (
            2,
            first,
            Some(super::super::super::tasks::Capture { index: 1, ..source }),
        ),
        (3, other, None),
    ] {
        let (_, mut value) = super::super::super::tasks::birth(
            function.def_id,
            Point::new(BlockId::ENTRY, index),
            ty,
        );
        for handles in value.futures.values_mut() {
            *handles = handles
                .iter()
                .map(|h| Handle {
                    capture,
                    ..h.clone()
                })
                .collect();
        }
        argument.join(&value.prefixed(Slot::Tail, callable, &table));
    }
    let arguments = [argument];
    let state = super::super::super::State::default();
    let callee = Value::default();
    let point = Point::new(BlockId::ENTRY, 9);
    let actuals = CallActuals {
        callee: &callee,
        arguments: &arguments,
        ty: first,
        point,
        state: &state,
        instantiation_site: None,
        aliases: None,
    };
    let target = Target::Input {
        input: super::super::Local(1),
        path: vec![Slot::Tail],
        ty: first,
        capture: Some(source),
        queue: None,
    };
    let selected = flow.bind_task_target(&target, &function, actuals);
    assert_eq!(selected.len(), 1);
    assert_eq!(selected.iter().next().unwrap().target.ty(), first);
    assert_eq!(selected.iter().next().unwrap().capture, Some(source));
    let missing = Target::Input {
        input: super::super::Local(1),
        path: vec![Slot::Tail],
        ty: missing_ty,
        capture: Some(source),
        queue: None,
    };
    let selected = flow.bind_task_target(&missing, &function, actuals);
    assert!(
        matches!(&selected.iter().next().unwrap().target,Target::Unknown{site,..} if site.function==function.def_id && site.point==point)
    );
}
