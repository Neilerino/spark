// TAG [SPARK-114]

//! Actual spawn-return admission and leaf-task birth provenance.

use super::{views::Subject, Record, Target};
use crate::provenance::{address::OpaqueSite, value, Flow, Point, State, Value};
use spark_mir::{Local, Operand, Place};
use spark_resolve::DefId;
use spark_types::TypeId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::provenance) struct Birth {
    pub site: OpaqueSite,
    pub future_ty: TypeId,
    pub callback_ty: TypeId,
    pub callback: DefId,
}

impl Birth {
    pub fn record(&self, value: &Value) -> Option<Record> {
        let handles = value.futures.get(&Vec::new())?;
        if handles.len() != 1 {
            return None;
        }
        let handle = handles.iter().next()?;
        if !handle.pending
            || handle.capture.is_some()
            || !matches!(&handle.target,
            Target::Birth { constructor, ty, .. } if *constructor == self.site && *ty == self.future_ty)
        {
            return None;
        }
        let views = value.task_views.get(&Vec::new())?;
        if views.len() != 1 {
            return None;
        }
        let view = views.get(&Subject::from(handle))?;
        if view.fallback || view.callbacks.len() != 1 {
            return None;
        }
        let callback = view.callbacks.get(&self.callback_ty)?.value();
        let targets = callback.callees.get(&Vec::new())?;
        if targets.open
            || targets.unresolved
            || targets.known.len() != 1
            || !targets.known.contains(&self.callback)
        {
            return None;
        }
        Some(Record {
            callback,
            callback_ty: self.callback_ty,
        })
    }
}

pub(in crate::provenance) struct Transfer {
    source: Local,
    birth: Birth,
}

impl Transfer {
    pub fn at_spawn(
        flow: &Flow<'_>,
        args: &[Operand],
        record: &Record,
        ty: TypeId,
        point: Point,
    ) -> Option<Self> {
        let Operand::Move(source) = args.first()? else {
            return None;
        };
        if !source.projections.is_empty() {
            return None;
        }
        let targets = flow.callee_targets(&record.callback, record.callback_ty);
        if targets.open || targets.unresolved || targets.known.len() != 1 {
            return None;
        }
        Some(Self {
            source: source.local,
            birth: Birth {
                site: OpaqueSite {
                    function: flow.function.def_id,
                    point,
                },
                future_ty: value::strip_mutable(ty, flow.table),
                callback_ty: record.callback_ty,
                callback: *targets.known.iter().next()?,
            },
        })
    }

    pub fn install(
        self,
        flow: &Flow<'_>,
        destination: &Place,
        clobbered: &[Local],
        state: &mut State,
    ) {
        if !destination.projections.is_empty()
            || [self.source, destination.local]
                .iter()
                .any(|local| clobbered.contains(local) || state.invalidated_copies.contains(local))
            || value::strip_mutable(flow.place_type(destination), flow.table)
                != self.birth.future_ty
            || self.birth.record(&state.value(destination.local)).is_none()
        {
            return;
        }
        let Some(record) = self.birth.record(&state.value(destination.local)) else {
            return;
        };
        if flow.leaf_task(&record) {
            state
                .future_copies
                .seed_birth(destination.local, self.birth);
        } else if !state.future_owners.known(destination.local)
            && state
                .current_execution_record(destination.local)
                .is_some_and(|record| {
                    record.callback_ty == self.birth.callback_ty
                        && record
                            .callback
                            .callees
                            .get(&Vec::new())
                            .is_some_and(|targets| {
                                !targets.open
                                    && !targets.unresolved
                                    && targets.known.len() == 1
                                    && targets.known.contains(&self.birth.callback)
                            })
                })
        {
            state.future_owners.install_role(
                super::Role::root(destination.local),
                super::lifecycle::Coverage::Direct,
                &state.future_copies,
            );
        }
    }
}

impl Flow<'_> {
    pub(in crate::provenance) fn current_leaf_birth(
        &self,
        local: Local,
        state: &State,
    ) -> Option<Birth> {
        if !state.live.contains(&local) || state.invalidated_copies.contains(&local) {
            return None;
        }
        let witness = state.future_copies.witness(local);
        if !witness.inputs.is_empty() {
            return None;
        }
        let birth = witness.birth?;
        let record = birth.record(&state.value(local))?;
        self.leaf_task(&record).then_some(birth)
    }
}
