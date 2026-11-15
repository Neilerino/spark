//! Closed forwarding contracts for cooperative spawn adapters.

use rustc_hash::{FxHashMap, FxHashSet};
use spark_resolve::{DefId, DefinitionKind, DefinitionTable};
use spark_types::{Ty, TypeId, TypeTable};

use crate::{Constant, Local, Mir, MirFunction, Operand, Rvalue, Statement, Terminator};

/// A parameter value in a certified spawn callback.
#[derive(Debug, Clone)]
pub enum SpawnArgument {
    /// Read a value from this slot of the callback's captured environment.
    Capture(usize),
    /// Materialize the literal argument without evaluating creator expressions.
    Constant(Constant),
}

/// A callback which only forwards arguments and the result of one async call.
#[derive(Debug, Clone)]
pub struct SpawnAdapter {
    /// Captured environment types, in their original declared order.
    pub capture_types: Vec<TypeId>,
    /// Argument bindings, in the async function's parameter order.
    pub arguments: Vec<SpawnArgument>,
}

#[derive(Clone)]
enum Value {
    Capture(usize),
    Constant(Constant),
    Result,
}

fn read(
    operand: &Operand,
    values: &FxHashMap<Local, Value>,
    used: &mut FxHashSet<Local>,
) -> Option<Value> {
    match operand {
        Operand::Constant(value) => Some(Value::Constant(value.clone())),
        Operand::Copy(place) | Operand::Move(place) if place.projections.is_empty() => {
            used.insert(place.local);
            values.get(&place.local).cloned()
        }
        _ => None,
    }
}

const fn argument(value: Value) -> Option<SpawnArgument> {
    match value {
        Value::Capture(index) => Some(SpawnArgument::Capture(index)),
        Value::Constant(value)
            if !matches!(value, Constant::FunctionRef(_) | Constant::IntrinsicRef(_)) =>
        {
            Some(SpawnArgument::Constant(value))
        }
        _ => None,
    }
}

pub(super) fn certify(
    function: &MirFunction,
    async_functions: &FxHashSet<DefId>,
) -> Option<(DefId, SpawnAdapter)> {
    if function.param_count != 0 {
        return None;
    }
    let mut values: FxHashMap<_, _> = function
        .captures
        .iter()
        .enumerate()
        .map(|(index, _)| (Local(index as u32 + 1), Value::Capture(index)))
        .collect();
    let mut written = FxHashSet::default();
    let mut used = FxHashSet::default();
    let mut visited = FxHashSet::default();
    let mut current = 0;
    let mut call = None;
    loop {
        if !visited.insert(current) {
            return None;
        }
        let block = function.blocks.get(current)?;
        for statement in &block.statements {
            match statement {
                Statement::Assign {
                    place,
                    rvalue: Rvalue::Use(operand),
                    ..
                } if place.projections.is_empty() => {
                    let declaration = function.locals.get(place.local.index())?;
                    if values.contains_key(&place.local)
                        || (place.local != Local::RETURN_PLACE && declaration.mutable)
                    {
                        return None;
                    }
                    let value = read(operand, &values, &mut used)?;
                    values.insert(place.local, value);
                    written.insert(place.local);
                }
                Statement::Nop => {}
                _ => return None,
            }
        }
        match &block.terminator {
            Terminator::Goto { target } => current = target.index(),
            Terminator::Call {
                func,
                args,
                destination,
                target,
                ..
            } if call.is_none() && destination.projections.is_empty() => {
                let Value::Constant(Constant::FunctionRef(callee)) =
                    read(func, &values, &mut used)?
                else {
                    return None;
                };
                if !async_functions.contains(&callee) {
                    return None;
                }
                let arguments = args
                    .iter()
                    .map(|operand| argument(read(operand, &values, &mut used)?))
                    .collect::<Option<Vec<_>>>()?;
                if values.insert(destination.local, Value::Result).is_some() {
                    return None;
                }
                written.insert(destination.local);
                call = Some((callee, arguments));
                current = target.index();
            }
            Terminator::Return => {
                if !matches!(values.get(&Local::RETURN_PLACE), Some(Value::Result)) {
                    return None;
                }
                used.insert(Local::RETURN_PLACE);
                if !(0..function.captures.len())
                    .all(|index| used.contains(&Local(index as u32 + 1)))
                {
                    return None;
                }
                if !written.is_subset(&used) {
                    return None;
                }
                let (callee, arguments) = call?;
                return Some((
                    callee,
                    SpawnAdapter {
                        capture_types: function.captures.iter().map(|(_, ty)| *ty).collect(),
                        arguments,
                    },
                ));
            }
            _ => return None,
        }
    }
}

fn immutable_type(mut ty: TypeId, table: &TypeTable) -> TypeId {
    while let Ty::Mutable(inner) = table.resolve(ty) {
        ty = *inner;
    }
    ty
}

pub(super) fn valid_arguments(
    adapter: &SpawnAdapter,
    callback: &MirFunction,
    callee: &MirFunction,
    table: &TypeTable,
) -> bool {
    if adapter.arguments.len() != callee.param_count as usize
        || immutable_type(callback.return_type, table) != immutable_type(callee.return_type, table)
    {
        return false;
    }
    adapter
        .arguments
        .iter()
        .zip(&callee.locals[1..=callee.param_count as usize])
        .all(|(argument, parameter)| {
            let ty = immutable_type(parameter.ty, table);
            match argument {
                SpawnArgument::Capture(index) => adapter
                    .capture_types
                    .get(*index)
                    .is_some_and(|capture| immutable_type(*capture, table) == ty),
                SpawnArgument::Constant(value) => matches!(
                    (value, table.resolve(ty)),
                    (Constant::Int(_), Ty::Int(_))
                        | (Constant::Float(_), Ty::Float(_))
                        | (Constant::Bool(_), Ty::Bool)
                        | (Constant::Char(_), Ty::Char)
                        | (Constant::String(_), Ty::String)
                        | (Constant::Void, Ty::Void)
                        | (Constant::Null, Ty::Null | Ty::Optional(_))
                ),
            }
        })
}

pub(super) fn attach(
    mir: &Mir,
    table: &TypeTable,
    definitions: &DefinitionTable,
    poll_functions: &mut FxHashMap<DefId, super::PollFnInfo>,
) {
    let async_functions = poll_functions.keys().copied().collect();
    for callback in mir.functions.values() {
        if !definitions
            .get(callback.def_id)
            .is_some_and(|info| matches!(info.kind, DefinitionKind::Lambda))
        {
            continue;
        }
        if let Some((callee, adapter)) = certify(callback, &async_functions) {
            if valid_arguments(&adapter, callback, &mir.functions[&callee], table) {
                if let Some(info) = poll_functions.get_mut(&callee) {
                    info.spawn_adapters.insert(callback.def_id, adapter);
                }
            }
        }
    }
}
