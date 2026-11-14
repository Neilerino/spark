// TAG [SPARK-114]

//! Bind typed input selections against a call's before-state.

use super::{
    child_type, value, AccessKind, Accesses, CallActuals, DerefKind, Flow, Intent, Local,
    MirFunction, Origin, Path, Port, Selector, Slot, TypeId, Value,
};

impl Flow<'_> {
    pub(super) fn select_input_value(
        &self,
        selector: &Selector,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> Value {
        let source = self.actual_input(selector.input, function, actuals);
        let source_ty = function.locals[selector.input.index()].ty;
        let mut selected = self.select_input(
            source,
            source_ty,
            &selector.selection,
            selector.input,
            function,
            actuals,
        );
        let expected = match &selector.intent {
            Intent::Address(payload) | Intent::Payload(payload) => Some(payload.pointee),
            Intent::Value => selector.expected.and_then(|ty| {
                if let spark_types::Ty::Ref { inner, .. } =
                    self.table.resolve(value::strip_mutable(ty, self.table))
                {
                    Some(*inner)
                } else {
                    None
                }
            }),
        };
        if let Some(capture) = selector.capture {
            selected = selected.select_reference_capture(capture);
        }
        if selector.capture.is_none()
            && selector.selection.contains(&Slot::Tail)
            && expected.is_some()
        {
            let prefix: Vec<_> = selector
                .selection
                .iter()
                .take_while(|slot| **slot != Slot::Tail)
                .cloned()
                .collect();
            selected = self.tail_reference_value(
                selected,
                self.selected_type(source_ty, &prefix),
                expected,
                actuals.state,
            );
        }
        selected
    }

    pub(super) fn select_selector(
        &mut self,
        selector: &Selector,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> Value {
        let mut selected = self.select_input_value(selector, function, actuals);
        if let Intent::Address(payload) | Intent::Payload(payload) = &selector.intent {
            if payload.kind == DerefKind::Residual {
                let port = Port {
                    capture: selector.capture,
                    input: selector.input,
                    selection: selector.selection.clone(),
                    pointee: payload.pointee,
                };
                selected = self.residual_reference(&selected, &port, function, actuals);
            }
        }
        if let Intent::Address(payload) = &selector.intent {
            return self.bind_reference(&selected, payload.pointee, &payload.projection, actuals);
        }
        if let Intent::Payload(payload) = &selector.intent {
            let value = self.read_pointee(&selected, payload.pointee, actuals.state);
            return self.project_path(value, payload.pointee, &payload.projection, actuals.state);
        }
        selected
    }

    pub(super) fn actual_input(
        &self,
        local: Local,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> Value {
        if local.0 > 0 && local.0 <= function.param_count {
            actuals
                .arguments
                .get(local.index() - 1)
                .cloned()
                .unwrap_or_default()
        } else if local.0 > function.param_count {
            let index = local.index() - function.param_count as usize - 1;
            let selected = actuals.callee.capture(function.def_id, index);
            let ty = function.locals[local.index()].ty;
            if matches!(
                self.table.resolve(value::strip_mutable(ty, self.table)),
                spark_types::Ty::Ref { .. }
            ) {
                selected.select_reference_capture(super::value::ReferenceCapture {
                    owner: function.def_id,
                    index,
                    ty,
                })
            } else {
                selected
            }
        } else {
            Value::default()
        }
    }

    fn bind_reference(
        &mut self,
        selected: &Value,
        ty: TypeId,
        projection: &Path,
        actuals: CallActuals<'_>,
    ) -> Value {
        if selected.is_empty() {
            return Value::default();
        }
        let mut accesses = Accesses::default();
        accesses.observe_reference(selected, None, projection, AccessKind::Address);
        self.record_accesses(actuals.point, accesses, actuals.state);
        let metadata = self.project_reference(selected, ty, projection);
        let mut result = Value::reference(Origin::Loan(actuals.point)).with_metadata(&metadata);
        result.insert_sourced(
            Vec::new(),
            Origin::Loan(actuals.point),
            selected.address_sources(),
        );
        result
    }

    pub(super) fn select_input(
        &self,
        mut value: Value,
        source_ty: TypeId,
        path: &Path,
        input: Local,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> Value {
        let mut ty = source_ty;
        for (index, slot) in path.iter().enumerate() {
            if *slot == Slot::Tail {
                break;
            }
            value = match slot {
                Slot::Deref => self.dereference(&value, ty, actuals.state),
                Slot::ResidualDeref => {
                    let port = Port {
                        capture: None,
                        input,
                        selection: path[..index].to_vec(),
                        pointee: child_type(ty, slot, self.table).unwrap_or(TypeId::ERROR),
                    };
                    let selected = self.residual_reference(&value, &port, function, actuals);
                    self.dereference(&selected, ty, actuals.state)
                }
                Slot::Capture(owner, index, ty)
                    if matches!(
                        self.table.resolve(value::strip_mutable(*ty, self.table)),
                        spark_types::Ty::Ref { .. }
                    ) =>
                {
                    value.capture(*owner, *index).select_reference_capture(
                        super::value::ReferenceCapture {
                            owner: *owner,
                            index: *index,
                            ty: *ty,
                        },
                    )
                }
                _ => value.project(slot),
            };
            ty = child_type(ty, slot, self.table).unwrap_or(TypeId::ERROR);
        }
        value
    }
}
