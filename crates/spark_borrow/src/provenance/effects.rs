// TAG [SPARK-114]

//! Direct function summaries use the same slot transfer engine as diagnostics.

use crate::provenance::tasks::proofs::CallProof;

use rustc_hash::{FxHashMap, FxHashSet};
use spark_diagnostics::Diagnostics;
use spark_mir::{BlockId, Local, Mir, MirFunction, Operand, Terminator};
use spark_resolve::DefId;
use spark_types::{TypeId, TypeTable};

use super::address::{Dependency, DerefKind, Endpoint, Intent, Selector};
use super::aliases::{Aliases, Key, Port, View};

use super::{
    child_type, value, AccessKind, Accesses, Flow, Origin, Origins, Path, Point, Slot, State, Value,
};

#[derive(Clone, Default)]
pub(super) struct Summary {
    returned: Value,
    returned_owner_tags: super::tasks::lifecycle::Tags,
    retained_owner_tags: super::tasks::lifecycle::Tags,
    pub(super) returned_callbacks: super::tasks::outputs::CallbackOutputs,
    queues: FxHashMap<super::channels::QueueKey, Value>,
    queue_contents: FxHashMap<(super::channels::Target, TypeId), Value>,
    tasks: FxHashMap<super::tasks::Target, super::tasks::Record>,
    normal_return: bool,
    memory: FxHashMap<Port, Value>,
    prefix_memory: PrefixMemory,
    normal_prefix: ReachedPrefix,
    normal_publications: PublicationValues,
    receive_snapshots: ReceiveSnapshots,
    modified: FxHashSet<View>,
    async_modified: FxHashSet<View>,
    residuals: FxHashMap<std::sync::Arc<super::tasks::residual::Key>, Value>,
    prefix_residuals: super::tasks::residual::Activities,
    residual_callbacks: FxHashMap<(std::sync::Arc<super::tasks::residual::Key>, TypeId), Value>,
    written_inputs: FxHashSet<View>,
    unknown_copy_clobber: bool,
    observed: Accesses,
    obligations: FxHashMap<super::obligations::Boundary, Value>,
}

impl Summary {
    fn join(&mut self, other: &Self) -> bool {
        let mut changed = self.returned.join(&other.returned);
        let before = self.returned_owner_tags.len();
        self.returned_owner_tags
            .extend(other.returned_owner_tags.iter().copied());
        changed |= before != self.returned_owner_tags.len();
        let before = self.retained_owner_tags.len();
        self.retained_owner_tags
            .extend(other.retained_owner_tags.iter().copied());
        changed |= before != self.retained_owner_tags.len();
        for (ty, output) in &other.returned_callbacks {
            changed |= self.returned_callbacks.entry(*ty).or_default().join(output);
        }
        changed |= join_prefix(&mut self.normal_prefix, &other.normal_prefix);
        changed |= self.normal_publications.join(&other.normal_publications);
        for (receive, values) in &other.receive_snapshots {
            let missing = !self.receive_snapshots.contains_key(receive);
            changed |= self
                .receive_snapshots
                .entry(receive.clone())
                .or_default()
                .join(values)
                || missing;
        }
        for (target, record) in &other.tasks {
            if let Some(current) = self.tasks.get_mut(target) {
                changed |= current.join(record);
            } else {
                self.tasks.insert(target.clone(), record.clone());
                changed = true;
            }
        }
        for (target, value) in &other.queue_contents {
            changed |= self
                .queue_contents
                .entry(target.clone())
                .or_default()
                .join(value);
        }
        for (target, value) in &other.queues {
            let existed = self.queues.contains_key(target);
            changed |= self.queues.entry(target.clone()).or_default().join(value) || !existed;
        }
        changed |= !self.unknown_copy_clobber && other.unknown_copy_clobber;
        self.unknown_copy_clobber |= other.unknown_copy_clobber;
        changed |= !self.normal_return && other.normal_return;
        self.normal_return |= other.normal_return;
        for (event, value) in &other.prefix_memory {
            let existed = self.prefix_memory.contains_key(event);
            changed |= self
                .prefix_memory
                .entry(event.clone())
                .or_default()
                .join(value)
                || !existed;
        }
        for (local, value) in &other.memory {
            changed |= self.memory.entry(local.clone()).or_default().join(value);
        }
        for (boundary, value) in &other.obligations {
            changed |= self
                .obligations
                .entry(boundary.clone())
                .or_default()
                .join(value);
        }
        let before = self.modified.len();
        self.modified.extend(other.modified.iter().cloned());
        changed |= before != self.modified.len();
        for (key, activity) in &other.prefix_residuals {
            let absent = !self.prefix_residuals.contains_key(key);
            changed |= self
                .prefix_residuals
                .entry(key.clone())
                .or_default()
                .join(activity)
                || absent;
        }
        for (key, value) in &other.residuals {
            let absent = !self.residuals.contains_key(key);
            changed |= self.residuals.entry(key.clone()).or_default().join(value) || absent;
        }
        for (key, value) in &other.residual_callbacks {
            changed |= self
                .residual_callbacks
                .entry(key.clone())
                .or_default()
                .join(value);
        }
        let before = self.async_modified.len();
        self.async_modified
            .extend(other.async_modified.iter().cloned());
        changed |= before != self.async_modified.len();
        let before = self.written_inputs.len();
        self.written_inputs
            .extend(other.written_inputs.iter().cloned());
        changed |= before != self.written_inputs.len();
        let before = self.observed.len();
        self.observed.extend(other.observed.iter().cloned());
        changed || before != self.observed.len()
    }
}

#[derive(Clone, Copy)]
pub(super) struct Context<'a> {
    pub(super) mir: &'a Mir,
    pub(super) definitions: Option<&'a spark_resolve::DefinitionTable>,
    summaries: &'a Summaries,
    pub(super) proofs: Option<&'a super::tasks::proofs::Proofs>,
    pub(super) ports: &'a Ports,
}

#[derive(Clone, Copy)]
struct CallActuals<'a> {
    callee: &'a Value,
    callee_place: Option<&'a spark_mir::Place>,
    arguments: &'a [Value],
    ty: TypeId,
    point: Point,
    instantiation_site: Option<super::address::OpaqueSite>,
    frame_caller: Option<DefId>,
    read: super::observations::ReadView<'a>,
    aliases: Option<&'a Aliases>,
}

impl<'a> CallActuals<'a> {
    fn normal(
        operand: &'a Operand,
        callee: &'a Value,
        arguments: &'a [Value],
        ty: TypeId,
        point: Point,
        frame_owner: DefId,
        state: &'a State,
    ) -> Self {
        Self {
            callee_place: match operand {
                Operand::Copy(place) | Operand::Move(place) => Some(place),
                Operand::Constant(_) => None,
            },
            callee,
            arguments,
            ty,
            point,
            read: super::observations::ReadView::current(state),
            instantiation_site: None,
            frame_caller: Some(frame_owner),
            aliases: None,
        }
    }

    const fn at_read<'b>(self, receive: Option<&'b Receive>) -> CallActuals<'b>
    where
        'a: 'b,
    {
        CallActuals {
            read: self.read.with_entry_prefix().with_receive(receive),
            ..self
        }
    }

    fn instantiation_site(self, caller: DefId) -> super::address::OpaqueSite {
        self.instantiation_site
            .unwrap_or(super::address::OpaqueSite {
                function: caller,
                point: self.point,
            })
    }

    fn is_cell(self, port: &Port) -> bool {
        self.aliases
            .map_or_else(|| port.exact(), |aliases| aliases.is_cell(port))
    }
}

mod publications;
pub(super) use publications::PublicationValues;

type Summaries = FxHashMap<Key, Summary>;
type Ports = FxHashMap<DefId, Vec<Port>>;

mod channel_binding;
mod inference;
mod opaque;
mod prefix;
mod receive_prefix;
mod receive_snapshots;
pub(super) use prefix::{
    join_prefix, EventKey, Memory as PrefixMemory, MemoryEvent, PrefixOccurrence, PrefixWrites,
    ReachedPrefix, Receive,
};
pub(super) use receive_snapshots::ReceiveSnapshots;
mod history;
mod output_binding;
mod selection;
mod task_binding;
mod task_views;
mod transaction;
pub use inference::analyze_program;
#[cfg(test)]
use inference::infer_summaries;
pub(super) use transaction::{CallOutcome, Dispatch, MemoryWrite};

impl Flow<'_> {
    fn discover_ports(&self, summary: &Summary, ports: &mut Vec<Port>) {
        for port in &self.queue_ports {
            if !ports.contains(port) {
                ports.push(port.clone());
            }
        }
        if summary.written_inputs.is_empty() && summary.async_modified.is_empty() {
            return;
        }
        for view in summary.written_inputs.iter().chain(&summary.async_modified) {
            if !value::inputs(
                view.root.pointee,
                view.root.input,
                view.root.pointee,
                &[],
                self.table,
            )
            .is_empty()
                && !ports.contains(&view.root)
            {
                ports.push(view.root.clone());
            }
        }
        for access in &summary.observed {
            let selector = match &access.target {
                Dependency::InputValue(selector, _)
                | Dependency::Address(Endpoint::Input(selector, _)) => selector,
                Dependency::Address(Endpoint::Frame { .. }) => continue,
            };
            if let Some(port) = Port::from_selector(selector, self.function, self.table) {
                if !value::inputs(port.pointee, port.input, port.pointee, &[], self.table)
                    .is_empty()
                    && !ports.contains(&port)
                {
                    ports.push(port);
                }
            }
        }
    }

    pub(super) fn call_result(
        &mut self,
        func: &Operand,
        args: &[Operand],
        ty: TypeId,
        point: Point,
        state: &State,
    ) -> Dispatch {
        self.ensure_transfer(point, state);
        if let Some(outcome) = self.task_call(func, args, ty, point, state) {
            return match outcome {
                task_binding::TaskDispatch::Returns(outcomes) => Dispatch::returning(outcomes),
                task_binding::TaskDispatch::NoReturn(events) => Dispatch {
                    diverging: vec![events],
                    ..Dispatch::default()
                },
            };
        }
        if let Some(outcome) = self
            .collection_call(func, args, state)
            .or_else(|| self.channel_call(func, args, ty, point, state))
        {
            return Dispatch::returning(vec![outcome]);
        }
        let callee = self.operand(func, state);
        let arguments: Vec<_> = args.iter().map(|arg| self.operand(arg, state)).collect();
        let actuals = CallActuals::normal(
            func,
            &callee,
            &arguments,
            ty,
            point,
            self.function.def_id,
            state,
        );
        let targets = self.callee_targets(&callee, super::operand_type(func, self.function));
        let mut dispatch = Dispatch::default();
        let open = targets.open;
        let unresolved_value = targets.unresolved || (!targets.open && targets.known.is_empty());
        let mut unknown = unresolved_value;
        let mut dispatched = FxHashSet::default();
        for def in &targets.known {
            let Some(context) = self.program else {
                unknown = true;
                continue;
            };
            let Some(function) = context.mir.functions.get(def) else {
                unknown = true;
                continue;
            };
            let roots = &context.ports[def];
            let locations = self.call_locations(roots, function, actuals);
            for aliases in Aliases::call_variants(roots, self.table, &locations, &self.aliases) {
                dispatched.insert(*def);
                let key = Key {
                    function: *def,
                    aliases,
                    callables: self.callback_inputs(function, actuals),
                };
                let Some(summary) = context.summaries.get(&key) else {
                    self.requests.insert(key);
                    continue;
                };
                let actuals = CallActuals {
                    aliases: Some(&key.aliases),
                    ..actuals
                };
                self.observe_writes(summary, function, actuals);
                let prefix = self.bind_prefix(summary, function, actuals, &key, Some(args), None);
                self.observe_prefix(&prefix, point, state);
                if !summary.normal_return {
                    dispatch.diverging.push(PrefixWrites {
                        memory: prefix.memory,
                        queues: prefix.queues,
                        residuals: prefix.residuals,
                    });
                    continue;
                }
                let mut outcome = self.bind_outcome(summary, function, actuals, &key, args, prefix);
                outcome.future_proof = self.call_future_proof(&key, args, state);
                dispatch.returning.push(outcome);
            }
        }
        if unknown {
            if unresolved_value {
                self.record_unknown_arguments(point, args, &arguments, state);
            }
            self.record_obligation(
                point,
                super::obligations::Boundary::invoke(
                    super::operand_type(func, self.function),
                    dispatched,
                ),
                &callee,
            );
            let returned = self.opaque_result(args, actuals);
            let mut outcome = self.opaque_queue_outcome(returned, args, actuals);
            outcome.retained_captures = self.retained_capture_witnesses(None, actuals);
            outcome.clobbered_copies = self.opaque_copy_clobbers(func, args, actuals);
            dispatch.returning.push(outcome);
        }
        if open {
            let mut outcome = self.open_outcome(ty, point);
            outcome.retained_captures = self.retained_capture_witnesses(None, actuals);
            dispatch.returning.push(outcome);
        }
        dispatch
    }

    fn open_outcome(&self, ty: TypeId, point: Point) -> CallOutcome {
        let mut returned = value::validation_hole(ty, self.table);
        returned.join(&super::channels::validation_handles(
            ty,
            super::address::OpaqueSite {
                function: self.function.def_id,
                point,
            },
            self.table,
        ));
        returned.join(&super::tasks::opaque_handles(
            ty,
            super::address::OpaqueSite {
                function: self.function.def_id,
                point,
            },
            self.table,
            true,
        ));
        CallOutcome {
            direct_queue_send: None,
            fresh_leaf_task: false,
            spawn_owners: None,
            returned_owner_tags: crate::provenance::tasks::lifecycle::Tags::default(),
            retained_captures: Vec::new(),
            returned_callbacks: crate::provenance::tasks::outputs::CallbackOutputs::default(),
            residuals: Vec::new(),
            async_memory: Vec::new(),
            tasks: Vec::new(),
            completed_task: None,
            clobbered_copies: Vec::new(),
            future_proof: CallProof::default(),
            returned_prefix: rustc_hash::FxHashSet::default(),
            publications: crate::provenance::effects::PrefixWrites::default(),
            place_writes: Vec::new(),
            prefix_memory: Vec::new(),
            prefix_residuals: Vec::new(),
            returned,
            memory: Vec::new(),
            queues: Vec::new(),
            queue_contents: Vec::new(),
        }
    }

    fn callback_bases(
        &self,
        input: Local,
        ty: TypeId,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> Vec<(Value, TypeId, Option<value::ReferenceCapture>)> {
        let value = self.actual_input(input, function, actuals);
        let captures: FxHashSet<_> = value
            .leaves
            .keys()
            .filter_map(|(_, _, capture)| *capture)
            .collect();
        let mut bases = Vec::new();
        for capture in captures {
            let Some(inner) = child_type(capture.ty, &Slot::Deref, self.table) else {
                continue;
            };
            if super::callbacks::selectors(inner, self.table).is_empty() {
                continue;
            }
            let selector = Selector {
                input,
                selection: Vec::new(),
                intent: Intent::Value,
                expected: Some(capture.ty),
                capture: Some(capture),
                channel_capture: None,
                queue: None,
            };
            let read = actuals.read.select(function.def_id, &selector);
            let pointer = value.select_reference_capture(capture);
            bases.push((
                self.dereference_in(&pointer, capture.ty, read),
                inner,
                Some(capture),
            ));
        }
        bases.push((value, ty, None));
        bases
    }

    fn callback_inputs(
        &self,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> super::callbacks::Inputs {
        let mut inputs = super::callbacks::Inputs::invocation();
        for decl in function
            .locals
            .iter()
            .skip(1)
            .take(function.param_count as usize + function.captures.len())
        {
            for (value, root_ty, capture) in
                self.callback_bases(decl.local, decl.ty, function, actuals)
            {
                let mut pending = super::callbacks::selectors(root_ty, self.table);
                let mut visited = FxHashSet::default();
                while let Some((path, ty)) = pending.pop() {
                    let path = value::normalize(root_ty, path, self.table);
                    if path.contains(&Slot::Tail) || !visited.insert((path.clone(), ty)) {
                        continue;
                    }
                    let selector = Selector {
                        input: decl.local,
                        selection: path.clone(),
                        intent: Intent::Value,
                        expected: Some(ty),
                        capture,
                        channel_capture: None,
                        queue: None,
                    };
                    let selected =
                        self.select_input(value.clone(), root_ty, &selector, function, actuals);
                    let targets = self.callee_targets(&selected, ty);
                    for def in &targets.known {
                        let Some(target) = self
                            .program
                            .and_then(|context| context.mir.functions.get(def))
                        else {
                            continue;
                        };
                        for (index, (_, capture_ty)) in target.captures.iter().enumerate() {
                            for (selection, selected_ty) in
                                super::callbacks::selectors(*capture_ty, self.table)
                            {
                                pending.push((
                                    [
                                        path.clone(),
                                        vec![Slot::Capture(*def, index, *capture_ty)],
                                        selection,
                                    ]
                                    .concat(),
                                    selected_ty,
                                ));
                            }
                        }
                    }
                    inputs
                        .slots
                        .entry((decl.local, path, ty, capture))
                        .or_default()
                        .join(&targets);
                }
                for (path, targets) in value.callees {
                    let ty = self.selected_type(root_ty, &path);
                    inputs
                        .slots
                        .entry((
                            decl.local,
                            value::normalize(root_ty, path, self.table),
                            ty,
                            capture,
                        ))
                        .or_default()
                        .join(&targets);
                }
            }
        }
        inputs
    }

    fn observe_writes(
        &mut self,
        summary: &Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) {
        for view in &summary.written_inputs {
            let mut input = self.port_input(&view.root, function, actuals);
            if !actuals.is_cell(&view.root) {
                input = self.residual_reference(&input, &view.root, function, actuals);
            }
            let targets = self.project_reference(&input, view.root.pointee, &view.path);
            for endpoint in targets.endpoints {
                if let Endpoint::Input(selector, _) = endpoint {
                    for (view, _) in self.external_targets(&selector) {
                        self.written_inputs.insert(view);
                    }
                }
            }
        }
    }

    fn port_input(&self, port: &Port, function: &MirFunction, actuals: CallActuals<'_>) -> Value {
        self.select_input_value(&port.selector(), function, actuals)
    }

    fn summary(&mut self) -> Summary {
        let mut summary = Summary {
            written_inputs: self.written_inputs.clone(),
            unknown_copy_clobber: self.unknown_copy_clobber,
            ..Summary::default()
        };
        for index in 0..self.entries.len() {
            if !matches!(self.function.blocks[index].terminator, Terminator::Return) {
                continue;
            }
            let Some(mut state) = self.entries[index].clone() else {
                continue;
            };
            self.transfer_block(BlockId(index as u32), &mut state);
            summary.normal_return = true;
            join_prefix(&mut summary.normal_prefix, &state.reached_prefix);
            summary
                .normal_publications
                .join(&state.publications.exported(self.function.def_id));
            self.export_queue_contents(&state, &mut summary);
            self.export_tasks(&state, &mut summary);
            self.export_callback_outputs(&state, &mut summary);
            summary
                .retained_owner_tags
                .extend(state.input_owners.retained_tags());
            summary.returned_owner_tags.extend(
                state
                    .input_owners
                    .tags(&super::tasks::Role::root(Local::RETURN_PLACE)),
            );
            for (key, value) in &state.residuals {
                summary
                    .residuals
                    .entry(key.clone())
                    .or_default()
                    .join(&export_value(value, self.function.def_id));
            }
            for (key, value) in &state.residual_callbacks {
                summary
                    .residual_callbacks
                    .entry(key.clone())
                    .or_default()
                    .join(&export_value(value, self.function.def_id));
            }
            summary.returned.join(&export_value(
                &state.value(Local::RETURN_PLACE),
                self.function.def_id,
            ));
            let mut ports = self
                .program
                .map(|context| context.ports[&self.function.def_id].clone())
                .unwrap_or_default();
            ports.extend(self.written_inputs.iter().map(|view| view.root.clone()));
            let async_modified = Self::async_modified(&state);
            ports.extend(async_modified.iter().map(|view| view.root.clone()));
            summary.async_modified.extend(async_modified);
            for declaration in self
                .function
                .locals
                .iter()
                .skip(1)
                .take(self.function.param_count as usize + self.function.captures.len())
            {
                if let Some(inner) = child_type(declaration.ty, &Slot::Deref, self.table) {
                    ports.push(Port::root(declaration.local, inner));
                }
            }
            let ports: FxHashSet<_> = ports
                .into_iter()
                .map(|port| self.aliases.representative(&port))
                .collect();
            for port in ports {
                if matches!(port.kind, super::aliases::StorageKind::Channel(_)) {
                    continue;
                }
                let mut selector = port.selector();
                if !self.aliases.is_cell(&port) {
                    if let Intent::Address(payload) = &mut selector.intent {
                        payload.kind = DerefKind::Residual;
                    }
                }
                let value = self.symbolic_pointee(&selector, port.pointee, &state);
                summary
                    .memory
                    .entry(port.clone())
                    .or_default()
                    .join(&export_value(&value, self.function.def_id));
                summary.modified.extend(
                    state
                        .modified_views
                        .iter()
                        .filter(|view| view.root == port)
                        .cloned(),
                );
            }
        }
        self.export_prefix(&mut summary);

        self.normalize_fresh_queue_roles(&mut summary);
        summary
    }

    fn bind_frame(&mut self, endpoint: &Endpoint, actuals: CallActuals<'_>) -> Endpoint {
        let boundary = actuals
            .frame_caller
            .map(|function| super::address::OpaqueSite {
                function,
                point: actuals.point,
            });
        let bound = endpoint.bind_frame(boundary);
        if let Endpoint::Frame {
            lifetime: super::address::FrameLifetime::DuringCall { boundary: site, .. },
            ..
        } = &bound
        {
            if boundary == Some(*site) {
                self.frame_boundaries.insert(*site);
            }
        }
        bound
    }

    fn typed_dependency(
        &self,
        selector: &Selector,
        leaf: &super::value::Leaf,
        path: &Path,
        root: TypeId,
    ) -> Selector {
        let mut selector = selector.clone();
        if selector.expected.is_none() && leaf.opaque.is_empty() {
            let ty = self.selected_type(root, path);
            if matches!(
                self.table.resolve(value::strip_mutable(ty, self.table)),
                spark_types::Ty::Ref { .. }
            ) {
                selector.expected = Some(ty);
            }
        }
        selector
    }

    fn bind(&mut self, summary: &Value, function: &MirFunction, actuals: CallActuals<'_>) -> Value {
        let mut result = Value {
            callees: summary.callees.clone(),
            queues: self.bind_queue_handles(summary, function, actuals),
            futures: self.bind_task_handles(summary, function, actuals),
            task_views: self.bind_task_views(summary, function, actuals),
            ..Default::default()
        };
        for ((path, origin, capture), leaf) in &summary.leaves {
            if leaf.has_validation_hole(origin) {
                result.insert_leaf(
                    path.clone(),
                    Origin::ValidationHole,
                    &super::value::Leaf::default(),
                );
            }
            if leaf.is_validation_only(origin) {
                continue;
            }
            let mut retained = super::value::Leaf {
                opaque: leaf.opaque.clone(),
                ..Default::default()
            };
            let mut retained_sources = super::demand::Frontier::default();
            for endpoint in &leaf.endpoints {
                match endpoint {
                    Endpoint::Frame { .. } => {
                        retained
                            .endpoints
                            .insert(self.bind_frame(endpoint, actuals));
                    }
                    Endpoint::Input(selector, site) => {
                        let read = actuals.at_read(site.as_deref());
                        let selected = self.select_selector(selector, function, read);
                        if leaf.opaque.is_empty() {
                            result.insert_addresses(path, &selected);
                        } else {
                            retain_selected(&selected, &mut retained, &mut retained_sources);
                        }
                    }
                }
            }
            for dependency in &leaf.dependencies {
                match dependency {
                    Dependency::Address(endpoint @ Endpoint::Frame { .. }) => {
                        retained
                            .dependencies
                            .insert(Dependency::Address(self.bind_frame(endpoint, actuals)));
                    }
                    Dependency::Address(Endpoint::Input(selector, site)) => {
                        let read = actuals.at_read(site.as_deref());
                        let selected = self.select_selector(selector, function, read);
                        retain_selected(&selected, &mut retained, &mut retained_sources);
                    }
                    Dependency::InputValue(selector, site) => {
                        let selector = self.typed_dependency(selector, leaf, path, actuals.ty);
                        let read = actuals.at_read(site.as_deref());
                        let mut selected = self.select_selector(&selector, function, read);
                        if let Some(capture) = capture {
                            selected = selected.with_reference_capture(*capture);
                        }
                        if !leaf.opaque.is_empty() {
                            retain_selected(&selected, &mut retained, &mut retained_sources);
                        } else if path.last() == Some(&Slot::Tail) {
                            let prefix = if selector.expected.is_some_and(|ty| {
                                child_type(ty, &Slot::Deref, self.table).is_some()
                            }) {
                                path.as_slice()
                            } else {
                                &path[..path.len() - 1]
                            };
                            result.join(&selected.prefixed_path(prefix, actuals.ty, self.table));
                        } else {
                            result.insert_addresses(path, &selected);
                        }
                    }
                }
            }
            if !retained.endpoints.is_empty()
                || !retained.dependencies.is_empty()
                || !retained.opaque.is_empty()
            {
                let identity = if retained.opaque.is_empty() {
                    origin.clone()
                } else {
                    Origin::Unproven(actuals.point)
                };
                result.insert_contribution(path.clone(), identity.clone(), *capture, &retained);
                result
                    .sources
                    .entry((path.clone(), identity, *capture))
                    .or_default()
                    .extend(retained_sources);
            }
        }
        result
    }

    fn bound_accesses(
        &mut self,
        summary: &Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> Accesses {
        let mut accesses = Accesses::default();
        for access in &summary.observed {
            self.bind_access(access, function, actuals, &mut accesses);
        }
        accesses
    }

    fn bind_access(
        &mut self,
        access: &super::read::Access,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        accesses: &mut Accesses,
    ) {
        let (selector, receive) = match &access.target {
            Dependency::InputValue(selector, receive)
            | Dependency::Address(Endpoint::Input(selector, receive)) => {
                (selector, receive.as_deref())
            }
            Dependency::Address(Endpoint::Frame { .. }) => return,
        };
        let actuals = CallActuals {
            read: actuals
                .read
                .with_read_site(access.site)
                .with_entry_prefix()
                .with_receive(receive)
                .with_access_site(access.site),
            ..actuals
        };
        let mut selected = self.select_selector(selector, function, actuals);
        if selector.selection.contains(&Slot::Tail)
            && matches!(selector.intent, Intent::Value)
            && selector.expected.is_none()
        {
            selected = self.tail_reference_value(selected, TypeId::ERROR, None, actuals.read.state);
        }
        accesses.observe_reference_at(&selected, None, &access.selection, access.kind, access.site);
    }

    fn residual_reference(
        &self,
        value: &Value,
        port: &Port,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> Value {
        let mut selected = self.expand_reference(value, port.pointee);
        selected.materialize_validation_holes();
        if actuals.is_cell(port) {
            return selected;
        }
        let Some(aliases) = actuals.aliases else {
            return selected;
        };
        let ports = self
            .program
            .map(|context| context.ports[&function.def_id].as_slice())
            .unwrap_or_default();
        let mut excluded = FxHashSet::default();
        for root in aliases.family(port, ports, self.table).excluded {
            let input = self.port_input(&root, function, actuals);
            if let Some(locations) = self.reference_locations(&input, root.pointee) {
                if locations.len() == 1 {
                    excluded.extend(
                        locations
                            .into_iter()
                            .filter(super::aliases::Location::unique),
                    );
                }
            }
        }
        selected.leaves.retain(|(_, origin, _), leaf| {
            leaf.endpoints.retain(|endpoint| {
                let pointer = Value::reference(origin.clone()).with_metadata(&super::value::Leaf {
                    endpoints: std::iter::once(endpoint.clone()).collect(),
                    ..Default::default()
                });
                self.reference_locations(&pointer, port.pointee)
                    .is_none_or(|locations| {
                        locations.len() != 1
                            || !locations.iter().all(|location| excluded.contains(location))
                    })
            });
            leaf.is_validation_only(origin)
                || !leaf.endpoints.is_empty()
                || !leaf.opaque.is_empty()
                || leaf
                    .dependencies
                    .iter()
                    .any(|dependency| matches!(dependency, Dependency::InputValue(..)))
        });
        selected
            .sources
            .retain(|key, _| selected.leaves.contains_key(key));
        selected
    }

    fn expand_reference(&self, value: &Value, ty: TypeId) -> Value {
        let mut result = Value::default();
        for (key @ (path, _, _), leaf) in &value.leaves {
            if !path.is_empty() && *path != [Slot::Tail] {
                continue;
            }
            let mut one = Value::default();
            one.leaves.insert(key.clone(), leaf.clone());
            let mut metadata = self.project_reference(&one, ty, &Vec::new());
            let endpoints = std::mem::take(&mut metadata.endpoints);
            for endpoint in endpoints {
                match endpoint {
                    Endpoint::Input(selector, site) => {
                        for (view, _) in self.external_targets(&selector) {
                            let mut selected = view.root.selector();
                            if let Intent::Address(payload) = &mut selected.intent {
                                payload.projection = view.path;
                                if !self.aliases.is_cell(&view.root) {
                                    payload.kind = DerefKind::Residual;
                                }
                            }
                            metadata
                                .endpoints
                                .insert(Endpoint::Input(selected, site.clone()));
                        }
                    }
                    frame @ Endpoint::Frame { .. } => {
                        metadata.endpoints.insert(frame);
                    }
                }
            }
            result.leaves.insert(key.clone(), metadata);
            if let Some(sources) = value.sources.get(key) {
                result.sources.insert(key.clone(), sources.clone());
            }
        }
        result
    }
}

pub(super) fn export_value(value: &Value, function: DefId) -> Value {
    let mut result = Value {
        callees: value.callees.clone(),
        queues: value.queues.clone(),
        futures: value.futures.clone(),
        task_views: task_views::export(value, function),
        ..Default::default()
    };
    for ((path, origin, capture), leaf) in &value.leaves {
        if leaf.has_validation_hole(origin) {
            result.insert_leaf(
                path.clone(),
                Origin::ValidationHole,
                &super::value::Leaf::default(),
            );
        }
        if leaf.is_validation_only(origin) {
            continue;
        }
        let origin = match origin {
            Origin::Loan(point) => Origin::Frame {
                function,
                point: *point,
            },
            Origin::Unproven(point) => Origin::Opaque {
                function,
                point: *point,
            },
            other => other.clone(),
        };
        let mut leaf = leaf.clone();
        leaf.parents.clear();
        result.insert_contribution(path.clone(), origin, *capture, &leaf);
    }
    result
}

fn retain_selected(
    value: &Value,
    retained: &mut super::value::Leaf,
    sources: &mut super::demand::Frontier,
) {
    retained.parents.extend(value.origins());
    sources.extend(value.all_sources());
    for leaf in value.leaves.values() {
        retained.parents.extend(leaf.parents.iter().cloned());
        retained
            .dependencies
            .extend(leaf.dependencies.iter().cloned());
        retained
            .dependencies
            .extend(leaf.endpoints.iter().cloned().map(Dependency::Address));
        retained.opaque.extend(leaf.opaque.iter().copied());
    }
    if !retained.opaque.is_empty() {
        retained.parents.remove(&Origin::ValidationHole);
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod write_tests;

#[cfg(test)]
mod family_tests;

#[cfg(test)]
mod unproven_tests;
