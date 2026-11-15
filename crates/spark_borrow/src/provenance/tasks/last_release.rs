// TAG [SPARK-114]

//! A proven final execution release can block its normal continuation.

use super::{lifecycle::Coverage, Handle, Record, ReturnStatus, Role};
use crate::provenance::{AccessKind, Accesses, Flow, Point, State};
use spark_mir::{Local, Place};
use spark_types::{Ty, TypeId};

pub(in crate::provenance) struct LastRelease {
    local: Local,
    handle: Handle,
    record: Record,
    result: TypeId,
}

impl LastRelease {
    pub fn at_drop(flow: &Flow<'_>, place: &Place, state: &State) -> Option<Self> {
        let Ty::Future { inner } = flow.table.resolve(crate::provenance::value::strip_mutable(
            flow.place_type(place),
            flow.table,
        )) else {
            return None;
        };
        if !place.projections.is_empty() {
            return None;
        }
        let role = Role::root(place.local);
        if state.future_copies.role_peers(&role) != rustc_hash::FxHashSet::from_iter([role.clone()])
            || !matches!(
                state.future_owners.coverage(&role),
                Some(Coverage::Closed | Coverage::Direct)
            )
            || state.future_owners.exposed_role(&role)
            || state.invalidated_copies.contains(&place.local)
        {
            return None;
        }
        let value = state.value(place.local);
        let handles = value.futures.get(&Vec::new())?;
        if handles.len() != 1 {
            return None;
        }
        let handle = handles.iter().next()?.clone();
        if !handle.pending || handle.capture.is_some() {
            return None;
        }
        Some(Self {
            local: place.local,
            handle,
            record: state.current_execution_record(place.local)?,
            result: *inner,
        })
    }

    fn valid(&self, flow: &Flow<'_>, state: &State) -> bool {
        Self::at_drop(flow, &Place::from_local(self.local), state).is_some_and(|current| {
            current.handle == self.handle
                && current.record.callback_ty == self.record.callback_ty
                && current.record.callback.callees.get(&Vec::new())
                    == self.record.callback.callees.get(&Vec::new())
        })
    }
}

impl LastRelease {
    pub fn continues_after_prefix(
        &self,
        flow: &mut Flow<'_>,
        point: Point,
        state: &mut State,
    ) -> bool {
        if !self.valid(flow, state) {
            return true;
        }
        let Some(current) = state.current_execution_record(self.local) else {
            return true;
        };
        let settled = flow.task_facts(&current, &self.handle.target, point, state, self.result);
        flow.observe_prefix(&settled.prefix, point, state);
        flow.apply_prefix_writes(
            &settled.prefix.memory,
            &settled.prefix.queues,
            &settled.prefix.residuals,
            point,
            state,
        );
        if !self.valid(flow, state) {
            return true;
        }
        let Some(current) = state.current_execution_record(self.local) else {
            return true;
        };
        let checked = flow.task_facts(&current, &self.handle.target, point, state, self.result);
        flow.observe_prefix(&checked.prefix, point, state);
        if checked.return_status == ReturnStatus::MayReturn {
            if settled.return_status == ReturnStatus::MayReturn
                && checked.normal_return
                && checked.contexts == settled.contexts
                && flow.task_contexts_available(&checked.contexts)
                && self.valid(flow, state)
            {
                flow.complete_future(self.local, &checked.returned, point, state);
            }
            return true;
        }
        if (checked.return_status == ReturnStatus::NoReturn && checked.contexts != settled.contexts)
            || !self.valid(flow, state)
        {
            return true;
        }
        let mut accesses = Accesses::default();
        flow.read_access(
            &Place::from_local(self.local),
            state,
            AccessKind::Value,
            &mut accesses,
        );
        flow.record_accesses(point, accesses, state);
        false
    }
}

impl Flow<'_> {
    pub(in crate::provenance) fn future_drop_continues(
        &mut self,
        place: &Place,
        point: Point,
        state: &mut State,
    ) -> bool {
        let Some(release) = LastRelease::at_drop(self, place, state) else {
            return true;
        };
        let facts = self.task_facts(
            &release.record,
            &release.handle.target,
            point,
            state,
            release.result,
        );
        self.observe_prefix(&facts.prefix, point, state);
        self.apply_prefix_writes(
            &facts.prefix.memory,
            &facts.prefix.queues,
            &facts.prefix.residuals,
            point,
            state,
        );
        release.continues_after_prefix(self, point, state)
    }
}
