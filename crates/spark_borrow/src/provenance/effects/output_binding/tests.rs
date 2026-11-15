// TAG [SPARK-114]

use super::*;
use crate::provenance::{
    address::{Endpoint, FrameLifetime},
    observations::ReadView,
    tasks::Role,
    value::Leaf,
    Origin, Point, Value,
};
use rustc_hash::FxHashSet;
use spark_mir::{BlockId, Local, LocalDecl, Place};
use spark_resolve::DefId;
use spark_span::Span;
use spark_types::{IntWidth, LifetimeId, Ty, TypeTable};

fn function(def: u32, types: &[spark_types::TypeId], parameters: u32) -> MirFunction {
    MirFunction {
        def_id: DefId::new(def),
        param_count: parameters,
        return_type: types[0],
        locals: types
            .iter()
            .enumerate()
            .map(|(index, ty)| LocalDecl {
                local: Local(index as u32),
                ty: *ty,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            })
            .collect(),
        blocks: vec![],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    }
}

#[test]
fn borrowed_partial_environment_provenance_cannot_hide_unresolved_ownership() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let callable = table.intern(Ty::Function {
        params: vec![],
        ret: future,
        is_async: false,
    });
    let reference = table.intern(Ty::Ref {
        inner: callable,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let caller = function(10020, &[future, future, future, callable, reference], 2);
    let mut callee = function(10021, &[future, future], 0);
    callee.captures = vec![(DefId::new(10022), future)];
    let flow = Flow::new(&caller, &table, None);
    let mut state = flow.initial_state();
    state.set(Local(3), Value::default());
    state.input_owners.add_output(
        Role::capture(Local(3), callee.def_id, 0, future),
        Tags::from_iter([Token::Derived(Local(1))]),
    );
    state.input_owners.mark_unresolved_environment(Local(3));
    let point = Point::new(BlockId::ENTRY, 0);
    let metadata = Leaf {
        endpoints: FxHashSet::from_iter([Endpoint::Frame {
            lifetime: FrameLifetime::Local,
            function: caller.def_id,
            root: point,
            local: Local(3),
            root_ty: callable,
            pointee: callable,
            location: vec![],
        }]),
        ..Leaf::default()
    };
    state.set(
        Local(4),
        Value::reference(Origin::Loan(point)).with_metadata(&metadata),
    );
    let place = Place::from_local(Local(4));
    let value = Value::default();
    let actuals = CallActuals {
        callee: &value,
        callee_place: Some(&place),
        arguments: &[],
        ty: future,
        point,
        instantiation_site: None,
        frame_caller: None,
        read: ReadView::current(&state),
        aliases: None,
    };
    let bound = flow.bind_owner_tags(
        &Tags::from_iter([Token::Derived(Local(1))]),
        &callee,
        actuals,
        &[],
    );
    assert!(bound.contains(&Token::Derived(Local(1))));
    assert!(
        bound.contains(&Token::Derived(Local(2))),
        "known tags in a reached unresolved environment are not a complete owner source"
    );
}

#[test]
fn a_physical_callable_write_revokes_known_empty_environment_lineage() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let callable = table.intern(Ty::Function {
        params: vec![],
        ret: future,
        is_async: false,
    });
    let caller = function(10023, &[future, future, callable], 1);
    let mut flow = Flow::new(&caller, &table, None);
    let mut state = flow.initial_state();
    state.set(Local(2), Value::default());
    assert!(!state.input_owners.unresolved_environment(Local(2)));
    let point = Point::new(BlockId::ENTRY, 0);
    let targets = Leaf {
        endpoints: FxHashSet::from_iter([Endpoint::Frame {
            lifetime: FrameLifetime::Local,
            function: caller.def_id,
            root: point,
            local: Local(2),
            root_ty: callable,
            pointee: callable,
            location: vec![],
        }]),
        ..Leaf::default()
    };
    let replacement = Value {
        callees: FxHashMap::from_iter([(
            vec![],
            crate::provenance::callbacks::Targets::function(DefId::new(10024)),
        )]),
        ..Value::default()
    };
    flow.store_references(&targets, &replacement, callable, point, &mut state);
    assert!(
        state.input_owners.unresolved_environment(Local(2)),
        "physical writes cannot establish complete capture lineage from a bare Value"
    );
    let uncertain = state.clone();
    flow.transfer_statement(
        &spark_mir::Statement::Assign {
            place: Place::from_local(Local(2)),
            rvalue: spark_mir::Rvalue::Use(Operand::Constant(spark_mir::Constant::FunctionRef(
                DefId::new(10027),
            ))),
            span: Span::DUMMY,
        },
        point,
        &mut state,
    );
    assert!(
        !state.input_owners.unresolved_environment(Local(2)),
        "known construction replaces unresolved metadata"
    );
    assert!(state.join(&uncertain));
    assert!(
        state.input_owners.unresolved_environment(Local(2)),
        "a possible clobber survives the branch join"
    );
}

#[test]
fn projected_callback_outputs_conserve_their_owner_debt() {
    use crate::provenance::{tasks::outputs::CallbackOutput, Slot};
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let callable = table.intern(Ty::Function {
        params: vec![],
        ret: int,
        is_async: false,
    });
    let tuple = table.intern(Ty::Tuple(vec![future]));
    let caller = function(10025, &[tuple, future], 1);
    let mut flow = Flow::new(&caller, &table, None);
    let mut state = flow.initial_state();
    state.input_owners.drop_local(Local(1));
    assert!(state.input_owners.retains_no_owner(Local(1)));
    let outputs = FxHashMap::from_iter([(
        callable,
        CallbackOutput {
            callback: Value::default(),
            lineage: FxHashMap::from_iter([(
                vec![Slot::Capture(DefId::new(10026), 0, future)],
                Tags::from_iter([Token::Derived(Local(1))]),
            )]),
        },
    )]);
    let mut destination = Place::from_local(Local(0));
    destination
        .projections
        .push(spark_mir::Projection::Field(spark_intern::sym("0")));
    flow.install_callback_outputs(
        outputs,
        &destination,
        Point::new(BlockId::ENTRY, 0),
        &mut state,
    );
    assert!(!state.input_owners.retains_no_owner(Local(1)));
    assert!(state.executions.is_empty());
}
