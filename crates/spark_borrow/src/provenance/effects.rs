//! Direct function summaries use the same slot transfer engine as diagnostics.

use rustc_hash::{FxHashMap, FxHashSet};
use spark_diagnostics::Diagnostics;
use spark_mir::{BlockId, Local, Mir, MirFunction, Operand, Terminator};
use spark_resolve::DefId;
use spark_types::{TypeId, TypeTable};

use super::address::{Dependency, Endpoint, Intent, Selector};
use super::aliases::{Aliases, Key, Port};

use super::{
    child_type, value, AccessKind, Accesses, Flow, Origin, Origins, Path, Point, Slot, State, Value,
};

#[derive(Clone, Default)]
pub(super) struct Summary {
    returned: Value,
    normal_return: bool,
    memory: FxHashMap<Port, Value>,
    modified: FxHashSet<Port>,
    written_inputs: FxHashSet<Port>,
    observed: Accesses,
}

impl Summary {
    fn join(&mut self, other: &Self) -> bool {
        let mut changed = self.returned.join(&other.returned);
        changed |= !self.normal_return && other.normal_return;
        self.normal_return |= other.normal_return;
        for (local, value) in &other.memory {
            changed |= self.memory.entry(local.clone()).or_default().join(value);
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
    mir: &'a Mir,
    summaries: &'a Summaries,
    ports: &'a Ports,
}

pub(super) struct CallOutcome {
    pub returned: Value,
    pub memory: Vec<(super::value::Leaf, Value)>,
}

#[derive(Clone, Copy)]
struct CallActuals<'a> {
    callee: &'a Value,
    arguments: &'a [Value],
    ty: TypeId,
    point: Point,
    state: &'a State,
}

type Summaries = FxHashMap<Key, Summary>;
type Ports = FxHashMap<DefId, Vec<Port>>;

pub fn analyze_program(
    mir: &Mir,
    table: &TypeTable,
    diagnostics: &mut Diagnostics,
) -> FxHashMap<DefId, Vec<Vec<crate::regions::Region>>> {
    let facts: FxHashMap<_, _> = mir
        .functions
        .iter()
        .map(|(def, function)| (*def, crate::facts::extract_facts(function)))
        .collect();
    let (summaries, ports) = infer_summaries(mir, table, &facts);
    let context = Context {
        mir,
        summaries: &summaries,
        ports: &ports,
    };
    let mut regions: FxHashMap<_, Vec<_>> = FxHashMap::default();
    for key in summaries.keys() {
        let function = &mir.functions[&key.function];
        let mut flow = Flow::new(function, table, Some(context));
        flow.aliases = key.aliases.clone();
        flow.solve(&facts[&key.function]);
        flow.report_returns(&facts[&key.function], diagnostics);
        flow.report_reads(diagnostics);
        regions
            .entry(key.function)
            .or_default()
            .push(flow.carrier_regions(&facts[&key.function]));
    }
    regions
}

fn infer_summaries(
    mir: &Mir,
    table: &TypeTable,
    facts: &FxHashMap<DefId, crate::facts::BorrowFacts>,
) -> (Summaries, Ports) {
    let mut summaries: Summaries = mir
        .functions
        .keys()
        .map(|def| (Key::distinct(*def), Summary::default()))
        .collect();
    let mut ports: Ports = mir.functions.keys().map(|def| (*def, Vec::new())).collect();
    loop {
        let context = Context {
            mir,
            summaries: &summaries,
            ports: &ports,
        };
        let mut next = summaries.clone();
        let mut discovered = ports.clone();
        let mut requests = FxHashSet::default();
        let mut changed = false;
        for key in summaries.keys() {
            let function = &mir.functions[&key.function];
            let mut flow = Flow::new(function, table, Some(context));
            flow.aliases = key.aliases.clone();
            flow.solve(&facts[&key.function]);
            let summary = flow.summary();
            flow.discover_ports(&summary, discovered.entry(key.function).or_default());
            changed |= next.entry(key.clone()).or_default().join(&summary);
            requests.extend(flow.requests);
        }
        if discovered != ports {
            for (def, roots) in &discovered {
                for aliases in Aliases::body_variants(&mir.functions[def], roots, table) {
                    requests.insert(Key {
                        function: *def,
                        aliases,
                    });
                }
            }
            ports = discovered;
            for summary in next.values_mut() {
                *summary = Summary::default();
            }
            changed = true;
        }
        for key in requests {
            if let std::collections::hash_map::Entry::Vacant(entry) = next.entry(key) {
                entry.insert(Summary::default());
                changed = true;
            }
        }
        summaries = next;
        if !changed {
            return (summaries, ports);
        }
    }
}

impl Flow<'_> {
    fn discover_ports(&self, summary: &Summary, ports: &mut Vec<Port>) {
        if summary.written_inputs.is_empty() {
            return;
        }
        for access in &summary.observed {
            let selector = match &access.target {
                Dependency::InputValue(selector)
                | Dependency::Address(Endpoint::Input(selector)) => selector,
                Dependency::Address(Endpoint::Frame { .. }) => continue,
            };
            if let Some(port) = Port::from_selector(selector, self.function, self.table) {
                if port.exact()
                    && !value::inputs(port.pointee, port.input, port.pointee, &[], self.table)
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
        let callee = self.operand(func, state);
        let arguments: Vec<_> = args.iter().map(|arg| self.operand(arg, state)).collect();
        let actuals = CallActuals {
            callee: &callee,
            arguments: &arguments,
            ty,
            point,
            state,
        };
        let targets = callee.callees.get(&Vec::new());
        let mut outcomes = Vec::new();
        let mut unknown = targets.is_none_or(FxHashSet::is_empty);
        for def in targets.into_iter().flatten() {
            let Some(context) = self.program else {
                unknown = true;
                continue;
            };
            let Some(function) = context.mir.functions.get(def) else {
                unknown = true;
                continue;
            };
            let roots = &context.ports[def];
            let locations = roots
                .iter()
                .map(|port| {
                    let input = self.port_input(port, function, actuals);
                    (port.clone(), self.reference_locations(&input, port.pointee))
                })
                .collect();
            for aliases in Aliases::call_variants(roots, self.table, &locations) {
                let key = Key {
                    function: *def,
                    aliases,
                };
                let Some(summary) = context.summaries.get(&key) else {
                    self.requests.insert(key);
                    continue;
                };
                self.observe(summary, function, actuals);
                self.observe_writes(summary, function, actuals);
                if !summary.normal_return {
                    continue;
                }
                outcomes.push(self.bind_outcome(summary, function, actuals, &key.aliases));
            }
        }
        if unknown {
            outcomes.push(CallOutcome {
                returned: self.opaque_result(args, actuals),
                memory: Vec::new(),
            });
        }
        (!outcomes.is_empty()).then_some(outcomes)
    }

    fn observe_writes(
        &mut self,
        summary: &Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) {
        for port in &summary.written_inputs {
            let input = self.port_input(port, function, actuals);
            let targets = self.project_reference(&input, port.pointee, &Vec::new());
            for endpoint in targets.endpoints {
                if let Endpoint::Input(selector) = endpoint {
                    if let Some(port) = Port::from_selector(&selector, self.function, self.table) {
                        self.written_inputs
                            .insert(self.aliases.representative(&port));
                    }
                }
            }
        }
    }

    fn port_input(&self, port: &Port, function: &MirFunction, actuals: CallActuals<'_>) -> Value {
        let source = Self::actual_input(port.input, function, actuals);
        self.select_input(
            source,
            function.locals[port.input.index()].ty,
            &port.selection,
            actuals.state,
        )
    }

    fn bind_outcome(
        &mut self,
        summary: &Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        aliases: &Aliases,
    ) -> CallOutcome {
        let returned = self.bind(&summary.returned, function, actuals);
        let mut memory = Vec::new();
        for port in &summary.modified {
            let Some(snapshot) = summary.memory.get(port) else {
                continue;
            };
            let inner = port.pointee;
            let mut target = super::value::Leaf::default();
            for member in aliases.members(port) {
                let input = self.port_input(&member, function, actuals);
                target.join(&self.project_reference(&input, inner, &Vec::new()));
            }
            let value = self.bind(
                snapshot,
                function,
                CallActuals {
                    ty: inner,
                    ..actuals
                },
            );
            memory.push((target, value));
        }
        CallOutcome { returned, memory }
    }

    fn opaque_result(&mut self, args: &[Operand], actuals: CallActuals<'_>) -> Value {
        let CallActuals {
            callee,
            arguments,
            ty,
            point,
            state,
        } = actuals;
        let mut origins = callee.origins();
        for arg in arguments {
            origins.extend(arg.origins());
        }
        let mut accesses = Accesses::default();
        for (argument, operand) in arguments.iter().zip(args) {
            let ty = match operand {
                Operand::Copy(place) | Operand::Move(place) => Some(self.place_type(place)),
                Operand::Constant(_) => None,
            };
            accesses.observe_reference(argument, ty, &[], AccessKind::Value);
        }
        self.record_accesses(point, accesses, state);
        origins.insert(Origin::Unproven(point));
        let mut tainted = value::tainted(ty, &origins, self.table);
        let parents: Origins = arguments
            .iter()
            .chain(std::iter::once(callee))
            .flat_map(|value| {
                value.origins().into_iter().chain(
                    value
                        .leaves
                        .values()
                        .flat_map(|leaf| leaf.parents.iter().cloned()),
                )
            })
            .collect();
        let sources: super::demand::Demand = arguments
            .iter()
            .chain(std::iter::once(callee))
            .flat_map(Value::all_sources)
            .collect();
        let mut dependencies = FxHashSet::default();
        for value in arguments.iter().chain(std::iter::once(callee)) {
            for leaf in value.leaves.values() {
                dependencies.extend(leaf.dependencies.iter().cloned());
                dependencies.extend(
                    leaf.endpoints
                        .iter()
                        .cloned()
                        .map(super::address::Dependency::Address),
                );
            }
        }
        for (key, leaf) in &mut tainted.leaves {
            leaf.parents.extend(parents.iter().cloned());
            leaf.dependencies.extend(dependencies.iter().cloned());
            leaf.opaque.insert(super::address::OpaqueSite {
                function: self.function.def_id,
                point,
            });
            tainted.sources.insert(key.clone(), sources.clone());
        }
        tainted
    }

    fn summary(&mut self) -> Summary {
        let mut summary = Summary {
            written_inputs: self.written_inputs.clone(),
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
            summary.returned.join(&export_value(
                &state.value(Local::RETURN_PLACE),
                self.function.def_id,
            ));
            let mut ports = self
                .program
                .map(|context| context.ports[&self.function.def_id].clone())
                .unwrap_or_default();
            ports.extend(self.written_inputs.iter().cloned());
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
                let value = self.symbolic_pointee(&port.selector(), port.pointee, &state);
                summary
                    .memory
                    .entry(port.clone())
                    .or_default()
                    .join(&export_value(&value, self.function.def_id));
                if state.modified_ports.contains(&port) {
                    summary.modified.insert(port);
                }
            }
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

        summary
    }

    fn bind(&mut self, summary: &Value, function: &MirFunction, actuals: CallActuals<'_>) -> Value {
        let mut result = Value {
            callees: summary.callees.clone(),
            ..Default::default()
        };
        for ((path, origin), leaf) in &summary.leaves {
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
                        let selected = self.select_selector(&selector, function, actuals);
                        if !leaf.opaque.is_empty() {
                            retain_selected(&selected, &mut retained, &mut retained_sources);
                        } else if path.last() == Some(&Slot::Tail) {
                            result.join(&selected.prefixed_path(
                                &path[..path.len() - 1],
                                actuals.ty,
                                self.table,
                            ));
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
                result.insert_leaf(path.clone(), identity.clone(), &retained);
                result
                    .sources
                    .entry((path.clone(), identity))
                    .or_default()
                    .extend(retained_sources);
            }
        }
        result
    }

    fn observe(&mut self, summary: &Summary, function: &MirFunction, actuals: CallActuals<'_>) {
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
        self.record_accesses(actuals.point, accesses, actuals.state);
    }

    fn select_selector(
        &mut self,
        selector: &Selector,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> Value {
        let source = Self::actual_input(selector.input, function, actuals);
        let source_ty = function.locals[selector.input.index()].ty;
        let mut selected = self.select_input(source, source_ty, &selector.selection, actuals.state);
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
        if selector.selection.contains(&Slot::Tail) && expected.is_some() {
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
        if let Intent::Address(payload) = &selector.intent {
            return self.bind_reference(&selected, payload.pointee, &payload.projection, actuals);
        }
        if let Intent::Payload(payload) = &selector.intent {
            let value = self.read_pointee(&selected, payload.pointee, actuals.state);
            return self.project_path(value, payload.pointee, &payload.projection, actuals.state);
        }
        selected
    }

    fn actual_input(local: Local, function: &MirFunction, actuals: CallActuals<'_>) -> Value {
        if local.0 > 0 && local.0 <= function.param_count {
            actuals
                .arguments
                .get(local.index() - 1)
                .cloned()
                .unwrap_or_default()
        } else if local.0 > function.param_count {
            actuals
                .callee
                .capture(local.index() - function.param_count as usize - 1)
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

    fn select_input(&self, mut value: Value, mut ty: TypeId, path: &Path, state: &State) -> Value {
        for slot in path {
            if *slot == Slot::Tail {
                break;
            }
            value = if *slot == Slot::Deref {
                self.dereference(&value, ty, state)
            } else {
                value.project(slot)
            };
            ty = child_type(ty, slot, self.table).unwrap_or(TypeId::ERROR);
        }
        value
    }
}

fn export_value(value: &Value, function: DefId) -> Value {
    let mut result = Value {
        callees: value.callees.clone(),
        ..Default::default()
    };
    for ((path, origin), leaf) in &value.leaves {
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
        result.insert_leaf(path.clone(), origin, &leaf);
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
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod write_tests;
