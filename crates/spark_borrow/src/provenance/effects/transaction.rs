// TAG [SPARK-114]

//! Apply each possible call before joining its continuation with the other alternatives.

use spark_mir::Place;

use super::{
    Aliases, CallActuals, Flow, Local, MemoryEvent, MirFunction, Operand, Point, PrefixWrites,
    State, Summary, TypeId, Value,
};
use crate::provenance::tasks::proofs::CallProof;

#[derive(Default)]
pub(in crate::provenance) struct Dispatch {
    pub returning: Vec<CallOutcome>,
    pub diverging: Vec<PrefixWrites>,
}

impl Dispatch {
    pub const fn returning(returning: Vec<CallOutcome>) -> Self {
        Self {
            returning,
            diverging: Vec::new(),
        }
    }
}

pub(in crate::provenance) struct CallOutcome {
    pub returned: Value,
    pub tasks: Vec<(
        crate::provenance::tasks::Target,
        crate::provenance::tasks::Record,
    )>,
    pub completed_task: Option<(Local, Value)>,
    pub clobbered_copies: Vec<Local>,
    pub future_proof: crate::provenance::tasks::proofs::CallProof,
    pub returned_prefix: super::ReachedPrefix,
    pub publications: PrefixWrites,
    pub place_writes: Vec<(spark_mir::Place, Value)>,
    pub memory: Vec<MemoryWrite>,
    pub prefix_memory: Vec<MemoryEvent>,
    pub queues: Vec<crate::provenance::channels::QueueEvent>,
    pub queue_contents: Vec<crate::provenance::channels::Write>,
}

impl CallOutcome {
    pub(super) fn freeze_sources(&mut self, flow: &Flow<'_>, point: Point) {
        self.returned = flow.freeze_sources(point, std::mem::take(&mut self.returned));
        for (_, record) in &mut self.tasks {
            record.callback = flow.freeze_sources(point, std::mem::take(&mut record.callback));
        }
        if let Some((_, result)) = &mut self.completed_task {
            *result = flow.freeze_sources(point, std::mem::take(result));
        }
        for (_, value) in &mut self.place_writes {
            *value = flow.freeze_sources(point, std::mem::take(value));
        }
        for event in &mut self.prefix_memory {
            event.freeze_sources(flow, point);
        }
        for write in &mut self.memory {
            write.value = flow.freeze_sources(point, std::mem::take(&mut write.value));
        }
        for write in self
            .queues
            .iter_mut()
            .map(|event| &mut event.write)
            .chain(&mut self.queue_contents)
        {
            write.value = flow.freeze_sources(point, std::mem::take(&mut write.value));
        }
    }
}

#[derive(Clone)]
pub(in crate::provenance) struct MemoryWrite {
    pub target: crate::provenance::value::Leaf,
    pub ty: TypeId,
    pub value: Value,
}

impl Flow<'_> {
    pub(super) fn bind_outcome(
        &mut self,
        summary: &Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        aliases: &Aliases,
        prefix: super::prefix::Prefix,
    ) -> CallOutcome {
        let returned = self.bind(&summary.returned, function, actuals);
        let mut memory = Vec::new();
        for modified in &summary.modified {
            if summary.modified.iter().any(|other| {
                other != modified
                    && other.root == modified.root
                    && modified.path.starts_with(&other.path)
            }) {
                continue;
            }
            let Some(snapshot) = summary.memory.get(&modified.root) else {
                continue;
            };
            let bound = self.bind(
                snapshot,
                function,
                CallActuals {
                    ty: modified.root.pointee,
                    ..actuals
                },
            );
            let mut targets = crate::provenance::value::Leaf::default();
            for member in aliases.members(&modified.root) {
                let mut input = self.port_input(&member, function, actuals);
                if !aliases.is_cell(&member) {
                    input = self.residual_reference(&input, &member, function, actuals);
                }
                targets.join(&self.project_reference(&input, member.pointee, &modified.path));
            }
            let mut value = bound;
            for slot in &modified.path {
                value = value.project(slot);
            }
            memory.push(MemoryWrite {
                target: targets,
                ty: self.selected_type(modified.root.pointee, &modified.path),
                value,
            });
        }
        let queue_contents = self.bind_queue_contents(summary, function, actuals);
        let tasks = self.bind_tasks(summary, function, actuals);
        let clobbered_copies = self.bound_copy_clobbers(summary, function, actuals);
        CallOutcome {
            publications: prefix.publications,
            tasks,
            completed_task: None,
            clobbered_copies,
            future_proof: CallProof::default(),
            returned_prefix: prefix.returned,
            place_writes: Vec::new(),
            prefix_memory: prefix.memory,
            returned,
            memory,
            queues: prefix.queues,
            queue_contents,
        }
    }

    pub(in crate::provenance) fn apply_dispatch(
        &mut self,
        dispatch: Dispatch,
        args: &[Operand],
        destination: &Place,
        point: Point,
        state: &mut State,
    ) -> bool {
        for mut events in dispatch.diverging {
            if events.memory.is_empty() && events.queues.is_empty() {
                continue;
            }
            let mut entry = self.transfers[&point].clone();
            for event in &mut events.memory {
                event.freeze_sources(self, point);
            }
            for event in &mut events.queues {
                event.write.value =
                    self.freeze_sources(point, std::mem::take(&mut event.write.value));
            }
            self.apply_prefix_writes(&events.memory, &events.queues, point, &mut state.clone());
            entry
                .seeds
                .extend(self.transfers[&point].seeds.iter().cloned());
            self.transfers.insert(point, entry);
        }
        if dispatch.returning.is_empty() {
            return false;
        }
        self.apply_call_outcomes(args, destination, dispatch.returning, point, state);
        true
    }

    fn apply_prefix_writes(
        &mut self,
        memory: &[MemoryEvent],
        queues: &[crate::provenance::channels::QueueEvent],
        point: Point,
        state: &mut State,
    ) {
        if !memory.is_empty() || !queues.is_empty() {
            self.expose_prefix_memory(memory, state);
            self.settle_pending_tasks_in(point, state, memory, queues);
        }
    }

    pub(in crate::provenance) fn apply_call_outcomes(
        &mut self,
        args: &[Operand],
        destination: &spark_mir::Place,
        mut outcome: Vec<CallOutcome>,
        point: Point,
        state: &mut State,
    ) {
        for alternative in &mut outcome {
            alternative.freeze_sources(self, point);
        }
        let common = self.transfers[&point].clone();
        let mut alternatives: Option<(State, crate::provenance::demand::Transfer)> = None;
        for alternative in outcome {
            self.transfers.insert(point, common.clone());
            let mut after = state.clone();
            self.apply_prefix_writes(
                &alternative.prefix_memory,
                &alternative.queues,
                point,
                &mut after,
            );
            after.reached_prefix.extend(alternative.returned_prefix);
            self.remember_publications(&alternative.publications, &mut after.publications);
            for arg in args {
                if let Some(local) = crate::provenance::consume(arg, &mut after) {
                    self.record_consumed(point, local);
                }
            }
            let returned_copy = self.apply_call_future_proof(
                alternative.future_proof,
                &alternative.clobbered_copies,
                point,
                &mut after,
            );
            self.import_tasks(alternative.tasks, point, &mut after);
            if let Some((local, result)) = alternative.completed_task {
                self.complete_future(local, &result, point, &mut after);
            }

            for write in alternative.queue_contents {
                self.import_queue_contents(
                    &write.target,
                    &write.value,
                    write.ty,
                    point,
                    &mut after,
                );
            }
            for write in alternative.queues {
                self.apply_queue_event(&write, point, &mut after);
            }
            for write in alternative.memory {
                self.apply_memory_snapshot(
                    &write.target,
                    &write.value,
                    write.ty,
                    point,
                    &mut after,
                );
            }
            for (place, value) in alternative.place_writes {
                self.store(&place, value, point, &mut after);
            }
            self.store(destination, alternative.returned, point, &mut after);
            self.store_future_copy(destination, returned_copy, &mut after);
            self.retire_call_frames(point, &mut after);
            let transfer = self.transfers[&point].clone();
            if let Some((joined, ledger)) = &mut alternatives {
                joined.join(&after);
                ledger.join_alternative(&transfer);
            } else {
                alternatives = Some((after, transfer));
            }
        }
        if let Some((after, transfer)) = alternatives {
            *state = after;
            self.transfers.insert(point, transfer);
        }
    }
}
