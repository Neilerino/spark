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
    pub async_memory: Vec<MemoryWrite>,
    pub fresh_leaf_task: bool,
    pub residuals: Vec<crate::provenance::tasks::residual::Import>,
    pub prefix_memory: Vec<MemoryEvent>,
    pub prefix_residuals: Vec<crate::provenance::tasks::residual::Import>,
    pub queues: Vec<crate::provenance::channels::QueueEvent>,
    pub queue_contents: Vec<crate::provenance::channels::Write>,
}

impl CallOutcome {
    fn visit_values(&mut self, mut visit: impl FnMut(&mut Value)) {
        visit(&mut self.returned);
        for residual in self.residuals.iter_mut().chain(&mut self.prefix_residuals) {
            residual.visit_values(&mut visit);
        }
        for (_, record) in &mut self.tasks {
            visit(&mut record.callback);
        }
        if let Some((_, result)) = &mut self.completed_task {
            visit(result);
        }
        for (_, value) in &mut self.place_writes {
            visit(value);
        }
        for value in self
            .memory
            .iter_mut()
            .map(|write| &mut write.value)
            .chain(self.async_memory.iter_mut().map(|write| &mut write.value))
            .chain(
                self.prefix_memory
                    .iter_mut()
                    .map(|event| &mut event.write.value),
            )
            .chain(self.queues.iter_mut().map(|event| &mut event.write.value))
            .chain(self.queue_contents.iter_mut().map(|write| &mut write.value))
            .chain(
                self.publications
                    .memory
                    .iter_mut()
                    .map(|event| &mut event.write.value),
            )
            .chain(
                self.publications
                    .queues
                    .iter_mut()
                    .map(|event| &mut event.write.value),
            )
        {
            visit(value);
        }
    }

    pub(super) fn freeze_sources(&mut self, flow: &Flow<'_>, point: Point) {
        self.visit_values(|value| *value = flow.freeze_sources(point, std::mem::take(value)));
    }

    fn import_observations(&mut self, flow: &mut Flow<'_>, point: Point, state: &mut State) {
        self.visit_values(|value| flow.import_task_sources(value, point, state));
    }
}

#[derive(Clone)]
pub(in crate::provenance) struct MemoryWrite {
    pub target: crate::provenance::value::Leaf,
    pub ty: TypeId,
    pub value: Value,
}

impl Flow<'_> {
    fn bind_memory_snapshots(
        &mut self,
        summary: &Summary,
        views: &rustc_hash::FxHashSet<super::View>,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        aliases: &Aliases,
    ) -> Vec<MemoryWrite> {
        let mut memory = Vec::new();
        for modified in views {
            if views.iter().any(|other| {
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
        memory
    }

    pub(super) fn bind_outcome(
        &mut self,
        summary: &Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        context: &super::Key,
        args: &[Operand],
        prefix: super::prefix::Prefix,
    ) -> CallOutcome {
        let returned = self.bind(&summary.returned, function, actuals);
        let memory = self.bind_memory_snapshots(
            summary,
            &summary.modified,
            function,
            actuals,
            &context.aliases,
        );
        let async_memory = self.bind_memory_snapshots(
            summary,
            &summary.async_modified,
            function,
            actuals,
            &context.aliases,
        );
        let queue_contents = self.bind_queue_contents(summary, function, actuals);
        let tasks = self.bind_tasks(summary, function, actuals);
        let clobbered_copies = self.bound_copy_clobbers(summary, function, actuals);
        let residuals = self.bind_residuals(summary, function, actuals, context, Some(args));
        CallOutcome {
            fresh_leaf_task: false,
            residuals,
            async_memory,
            publications: prefix.publications,
            tasks,
            completed_task: None,
            clobbered_copies,
            future_proof: CallProof::default(),
            returned_prefix: prefix.returned,
            place_writes: Vec::new(),
            prefix_memory: prefix.memory,
            prefix_residuals: prefix.residuals,
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
            if events.memory.is_empty() && events.queues.is_empty() && events.residuals.is_empty() {
                continue;
            }
            let mut entry = self.transfers[&point].clone();
            let mut divergent = state.clone();
            for residual in &mut events.residuals {
                residual.freeze_sources(self, point);
            }
            self.import_prefix_observations(
                &mut events.memory,
                &mut events.queues,
                point,
                &mut divergent,
            );
            self.apply_prefix_writes(
                &events.memory,
                &events.queues,
                &events.residuals,
                point,
                &mut divergent,
            );
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
        residuals: &[crate::provenance::tasks::residual::Import],
        point: Point,
        state: &mut State,
    ) {
        if !memory.is_empty() || !queues.is_empty() || !residuals.is_empty() {
            self.expose_prefix_memory(memory, state);
            self.settle_pending_tasks_in(point, state, memory, queues, residuals);
        }
    }

    fn apply_call_prefix(&mut self, outcome: &CallOutcome, point: Point, state: &mut State) {
        if outcome.prefix_residuals.is_empty() {
            self.apply_prefix_writes(
                &outcome.prefix_memory,
                &outcome.queues,
                &outcome.residuals,
                point,
                state,
            );
            return;
        }
        let entry = state.clone();
        let entry_transfer = self.transfers[&point].clone();
        self.apply_prefix_writes(
            &outcome.prefix_memory,
            &outcome.queues,
            &outcome.residuals,
            point,
            state,
        );
        let live = std::mem::replace(state, entry);
        let live_transfer = self.transfers[&point].clone();
        self.transfers.insert(point, entry_transfer);
        self.apply_prefix_writes(
            &outcome.prefix_memory,
            &outcome.queues,
            &outcome.prefix_residuals,
            point,
            state,
        );
        state.join_task_backing(&live);
        state.residuals = live.residuals;
        state.residual_callbacks = live.residual_callbacks;
        self.transfers
            .get_mut(&point)
            .expect("current call transfer")
            .join_alternative(&live_transfer);
    }

    fn consume_call_arguments(
        &mut self,
        args: &[Operand],
        outcome: &CallOutcome,
        point: Point,
        state: &mut State,
    ) {
        for arg in args {
            if let Operand::Move(place) = arg {
                let completed = outcome
                    .future_proof
                    .completed
                    .iter()
                    .any(|witness| witness.peers.contains(&place.local));
                let returned = outcome
                    .future_proof
                    .returned
                    .as_ref()
                    .is_some_and(|witness| witness.peers.contains(&place.local));
                let joined = outcome
                    .completed_task
                    .as_ref()
                    .is_some_and(|(local, _)| *local == place.local);
                if outcome.clobbered_copies.contains(&place.local)
                    || !(completed || returned || joined)
                {
                    Self::lose_owning_operand(arg, state);
                    self.preserve_writer_loss(place, point, state);
                }
            }
            Self::lose_owning_operand(arg, state);
            if let Some(local) = crate::provenance::consume(arg, state) {
                self.record_consumed(point, local);
            }
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
        for mut alternative in outcome {
            self.transfers.insert(point, common.clone());
            let mut after = state.clone();
            alternative.import_observations(self, point, &mut after);
            self.apply_call_prefix(&alternative, point, &mut after);
            for local in &after.invalidated_copies {
                if !alternative.clobbered_copies.contains(local) {
                    alternative.clobbered_copies.push(*local);
                }
            }
            Self::preserve_returned_task_progress(
                &mut alternative.returned,
                &alternative.future_proof,
                &alternative.clobbered_copies,
                &after,
            );
            after
                .reached_prefix
                .extend(alternative.returned_prefix.drain());
            self.remember_publications(&alternative.publications, &mut after.publications);
            for witness in &alternative.future_proof.completed {
                self.settle_task_memory(witness, &alternative.clobbered_copies, point, &mut after);
            }
            if let Some((local, _)) = &alternative.completed_task {
                let witness = after.future_copies.witness(*local);
                self.settle_task_memory(&witness, &alternative.clobbered_copies, point, &mut after);
            }
            for residual in alternative.residuals.drain(..) {
                self.import_residual(residual, point, &mut after);
            }
            self.consume_call_arguments(args, &alternative, point, &mut after);
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
            for write in alternative.async_memory {
                self.import_settled_memory(&write, point, &mut after);
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
            after.future_owners.forget(destination.local);
            if alternative.fresh_leaf_task && destination.projections.is_empty() {
                after
                    .future_owners
                    .grant(destination.local, &after.future_copies);
            }
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
