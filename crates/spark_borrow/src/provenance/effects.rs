// TAG [SPARK-114]

//! Direct function summaries use the same slot transfer engine as diagnostics.

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
    queues: FxHashMap<(super::channels::Target, TypeId), Value>,
    queue_contents: FxHashMap<(super::channels::Target, TypeId), Value>,
    tasks: FxHashMap<super::tasks::Target, super::tasks::Record>,
    normal_return: bool,
    memory: FxHashMap<Port, Value>,
    modified: FxHashSet<View>,
    written_inputs: FxHashSet<View>,
    unknown_copy_clobber: bool,
    observed: Accesses,
    obligations: FxHashMap<super::obligations::Boundary, Value>,
}

impl Summary {
    fn join(&mut self, other: &Self) -> bool {
        let mut changed = self.returned.join(&other.returned);
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
            changed |= self.queues.entry(target.clone()).or_default().join(value);
        }
        changed |= !self.unknown_copy_clobber && other.unknown_copy_clobber;
        self.unknown_copy_clobber |= other.unknown_copy_clobber;
        changed |= !self.normal_return && other.normal_return;
        self.normal_return |= other.normal_return;
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
    pub(super) ports: &'a Ports,
}

pub(super) struct CallOutcome {
    pub returned: Value,
    pub tasks: Vec<(super::tasks::Target, super::tasks::Record)>,
    pub completed_task: Option<(Local, Value)>,
    pub clobbered_copies: Vec<Local>,
    pub place_writes: Vec<(spark_mir::Place, Value)>,
    pub memory: Vec<MemoryWrite>,
    pub queues: Vec<super::channels::Write>,
    pub queue_contents: Vec<super::channels::Write>,
}

pub(super) struct MemoryWrite {
    pub target: super::value::Leaf,
    pub ty: TypeId,
    pub value: Value,
}

#[derive(Clone, Copy)]
struct CallActuals<'a> {
    callee: &'a Value,
    arguments: &'a [Value],
    ty: TypeId,
    point: Point,
    state: &'a State,
    aliases: Option<&'a Aliases>,
}

impl CallActuals<'_> {
    fn is_cell(self, port: &Port) -> bool {
        self.aliases
            .map_or_else(|| port.exact(), |aliases| aliases.is_cell(port))
    }
}

type Summaries = FxHashMap<Key, Summary>;
type Ports = FxHashMap<DefId, Vec<Port>>;

mod channel_binding;
mod inference;
mod opaque;
mod selection;
mod task_binding;
pub use inference::analyze_program;
#[cfg(test)]
use inference::infer_summaries;

impl Flow<'_> {
    fn discover_ports(&self, summary: &Summary, ports: &mut Vec<Port>) {
        for port in &self.queue_ports {
            if !ports.contains(port) {
                ports.push(port.clone());
            }
        }
        if summary.written_inputs.is_empty() {
            return;
        }
        for view in &summary.written_inputs {
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
                Dependency::InputValue(selector)
                | Dependency::Address(Endpoint::Input(selector)) => selector,
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
    ) -> Option<Vec<CallOutcome>> {
        if let Some(outcome) = self.task_call(func, args, ty, point, state) {
            return match outcome {
                task_binding::TaskDispatch::Returns(outcomes) => Some(outcomes),
                task_binding::TaskDispatch::NoReturn => None,
            };
        }
        if let Some(outcome) = self
            .collection_call(func, args, state)
            .or_else(|| self.channel_call(func, args, ty, point, state))
        {
            return Some(vec![outcome]);
        }
        let callee = self.operand(func, state);
        let arguments: Vec<_> = args.iter().map(|arg| self.operand(arg, state)).collect();
        let actuals = CallActuals {
            callee: &callee,
            arguments: &arguments,
            ty,
            point,
            state,
            aliases: None,
        };
        let targets = self.callee_targets(&callee, super::operand_type(func, self.function));
        let mut outcomes = Vec::new();
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
                self.observe(summary, function, actuals);
                self.observe_writes(summary, function, actuals);
                for (boundary, value) in &summary.obligations {
                    let bound = self.bind(
                        value,
                        function,
                        CallActuals {
                            ty: boundary.ty(),
                            ..actuals
                        },
                    );
                    self.record_obligation(point, boundary.clone(), &bound);
                }
                let queues = self.bind_queue_writes(summary, function, actuals);
                self.record_queue_prefix(&queues, point, state);
                if !summary.normal_return {
                    continue;
                }
                outcomes.push(self.bind_outcome(summary, function, actuals, &key.aliases, queues));
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
            outcome.clobbered_copies = self.opaque_copy_clobbers(func, args, actuals);
            outcomes.push(outcome);
        }
        if open {
            outcomes.push(self.open_outcome(ty, point));
        }
        (!outcomes.is_empty()).then_some(outcomes)
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
            tasks: Vec::new(),
            completed_task: None,
            clobbered_copies: Vec::new(),
            place_writes: Vec::new(),
            returned,
            memory: Vec::new(),
            queues: Vec::new(),
            queue_contents: Vec::new(),
        }
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
            let value = self.actual_input(decl.local, function, actuals);
            let captures: FxHashSet<_> = value
                .leaves
                .keys()
                .filter_map(|(_, _, capture)| *capture)
                .collect();
            let mut bases = vec![(value.clone(), decl.ty, None)];
            for capture in captures {
                let Some(inner) = child_type(capture.ty, &Slot::Deref, self.table) else {
                    continue;
                };
                if super::callbacks::selectors(inner, self.table).is_empty() {
                    continue;
                }
                let pointer = value.select_reference_capture(capture);
                bases.push((
                    self.dereference(&pointer, capture.ty, actuals.state),
                    inner,
                    Some(capture),
                ));
            }
            for (value, root_ty, capture) in bases {
                let mut pending = super::callbacks::selectors(root_ty, self.table);
                let mut visited = FxHashSet::default();
                while let Some((path, ty)) = pending.pop() {
                    let path = value::normalize(root_ty, path, self.table);
                    if path.contains(&Slot::Tail) || !visited.insert((path.clone(), ty)) {
                        continue;
                    }
                    let selected = self.select_input(
                        value.clone(),
                        root_ty,
                        &path,
                        decl.local,
                        function,
                        actuals,
                    );
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
                if let Endpoint::Input(selector) = endpoint {
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

    fn bind_outcome(
        &mut self,
        summary: &Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        aliases: &Aliases,
        queues: Vec<super::channels::Write>,
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
            let mut targets = super::value::Leaf::default();
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
            tasks,
            completed_task: None,
            clobbered_copies,
            place_writes: Vec::new(),
            returned,
            memory,
            queues,
            queue_contents,
        }
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
            self.export_queue_contents(&state, &mut summary);
            self.export_tasks(&state, &mut summary);
            summary.returned.join(&export_value(
                &state.value(Local::RETURN_PLACE),
                self.function.def_id,
            ));
            let mut ports = self
                .program
                .map(|context| context.ports[&self.function.def_id].clone())
                .unwrap_or_default();
            ports.extend(self.written_inputs.iter().map(|view| view.root.clone()));
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
        summary.queues = self.export_queue_writes();
        for ((_, boundary), value) in &self.obligations {
            summary
                .obligations
                .entry(boundary.clone())
                .or_default()
                .join(&export_value(value, self.function.def_id));
        }
        for access in self.accesses.values().flatten() {
            let selector = match &access.target {
                Dependency::InputValue(selector)
                | Dependency::Address(Endpoint::Input(selector)) => selector,
                Dependency::Address(Endpoint::Frame { .. }) => continue,
            };
            let mut observed = access.clone();
            observed.origin = Origin::Input {
                local: selector.input,
                path: selector.selection.clone(),
            };
            let pointee = if matches!(selector.intent, Intent::Address(_)) {
                selector.expected.unwrap_or(TypeId::ERROR)
            } else {
                let ty = selector.expected.unwrap_or_else(|| {
                    self.selected_type(
                        self.function.locals[selector.input.index()].ty,
                        &selector.selection,
                    )
                });
                child_type(ty, &Slot::Deref, self.table).unwrap_or(TypeId::ERROR)
            };
            observed.selection = value::normalize(pointee, access.selection.clone(), self.table);
            summary.observed.insert(observed);
        }

        self.normalize_fresh_queue_roles(&mut summary);
        summary
    }

    fn bind(&mut self, summary: &Value, function: &MirFunction, actuals: CallActuals<'_>) -> Value {
        let mut result = Value {
            callees: summary.callees.clone(),
            queues: self.bind_queue_handles(summary, function, actuals),
            futures: self.bind_task_handles(summary, function, actuals),
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
            let mut retained_sources = super::demand::Demand::default();
            for endpoint in &leaf.endpoints {
                match endpoint {
                    Endpoint::Frame { .. } => {
                        retained.endpoints.insert(endpoint.clone());
                    }
                    Endpoint::Input(selector) => {
                        let selected = self.select_selector(selector, function, actuals);
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
                    Dependency::Address(Endpoint::Frame { .. }) => {
                        retained.dependencies.insert(dependency.clone());
                    }
                    Dependency::Address(Endpoint::Input(selector)) => {
                        let selected = self.select_selector(selector, function, actuals);
                        retain_selected(&selected, &mut retained, &mut retained_sources);
                    }
                    Dependency::InputValue(selector) => {
                        let mut selector = selector.clone();
                        if selector.expected.is_none() && leaf.opaque.is_empty() {
                            let ty = self.selected_type(actuals.ty, path);
                            if matches!(
                                self.table.resolve(value::strip_mutable(ty, self.table)),
                                spark_types::Ty::Ref { .. }
                            ) {
                                selector.expected = Some(ty);
                            }
                        }
                        let mut selected = self.select_selector(&selector, function, actuals);
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
            let selector = match &access.target {
                Dependency::InputValue(selector)
                | Dependency::Address(Endpoint::Input(selector)) => selector,
                Dependency::Address(Endpoint::Frame { .. }) => continue,
            };
            let mut selected = self.select_selector(selector, function, actuals);
            if selector.selection.contains(&Slot::Tail)
                && matches!(selector.intent, Intent::Value)
                && selector.expected.is_none()
            {
                selected = self.tail_reference_value(selected, TypeId::ERROR, None, actuals.state);
            }
            accesses.observe_reference(&selected, None, &access.selection, access.kind);
        }
        accesses
    }

    fn observe(&mut self, summary: &Summary, function: &MirFunction, actuals: CallActuals<'_>) {
        let accesses = self.bound_accesses(summary, function, actuals);
        self.record_accesses(actuals.point, accesses, actuals.state);
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
                    .any(|dependency| matches!(dependency, Dependency::InputValue(_)))
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
                    Endpoint::Input(selector) => {
                        for (view, _) in self.external_targets(&selector) {
                            let mut selected = view.root.selector();
                            if let Intent::Address(payload) = &mut selected.intent {
                                payload.projection = view.path;
                                if !self.aliases.is_cell(&view.root) {
                                    payload.kind = DerefKind::Residual;
                                }
                            }
                            metadata.endpoints.insert(Endpoint::Input(selected));
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

fn export_value(value: &Value, function: DefId) -> Value {
    let mut result = Value {
        callees: value.callees.clone(),
        queues: value.queues.clone(),
        futures: value.futures.clone(),
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
    sources: &mut super::demand::Demand,
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
