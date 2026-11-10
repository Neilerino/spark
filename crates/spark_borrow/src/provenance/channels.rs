// TAG [SPARK-114]

//! Shared channel handles and weak payload storage.

use crate::provenance::tasks::proofs::CallProof;
use rustc_hash::{FxHashMap, FxHashSet};
use spark_intern::kw;
use spark_mir::{Local, Operand};
use spark_resolve::{DefId, DefinitionKind};
use spark_types::{Ty, TypeId, TypeTable};

use super::{value, Flow, Path, Point, Slot, State, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Capture {
    pub owner: DefId,
    pub index: usize,
    pub ty: TypeId,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Handle {
    pub target: Target,
    pub capture: Option<Capture>,
}

impl Handle {
    pub(super) const fn new(target: Target) -> Self {
        Self {
            target,
            capture: None,
        }
    }
    pub(super) const fn ty(&self) -> TypeId {
        self.target.ty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum OutputStorage {
    Return,
    Memory(super::aliases::Port),
    Queue(super::aliases::Port),
    Opaque(super::address::OpaqueSite, TypeId),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct OutputRole {
    pub storage: OutputStorage,
    pub ty: TypeId,
    pub selection: Path,
    pub capture: Option<Capture>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Constructor {
    pub site: super::address::OpaqueSite,
    pub role: Option<OutputRole>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Target {
    Input {
        capture: Option<Capture>,
        queue: Option<super::address::QueueSelection>,
        local: Local,
        path: Path,
        ty: TypeId,
    },
    Allocation {
        ty: TypeId,
        function: DefId,
        point: Point,
        constructor: Option<Constructor>,
    },
    Validation {
        site: super::address::OpaqueSite,
        ty: TypeId,
        selection: Path,
    },
    Unknown {
        site: super::address::OpaqueSite,
        ty: TypeId,
        constructor: Option<Constructor>,
    },
}

impl Target {
    const fn may_outlive_function(&self) -> bool {
        matches!(self, Self::Input { .. } | Self::Unknown { .. })
    }

    pub(super) fn with_output_role(&self, role: OutputRole) -> Self {
        let mut target = self.clone();
        let (site, constructor) = match &mut target {
            Self::Allocation {
                function,
                point,
                constructor,
                ..
            } => (
                super::address::OpaqueSite {
                    function: *function,
                    point: *point,
                },
                constructor,
            ),
            Self::Unknown {
                site, constructor, ..
            } => (*site, constructor),
            Self::Input { .. } | Self::Validation { .. } => return target,
        };
        let constructor = constructor.get_or_insert(Constructor { site, role: None });
        constructor.role = Some(role);
        target
    }

    pub(super) const fn ty(&self) -> TypeId {
        match self {
            Self::Input { ty, .. }
            | Self::Allocation { ty, .. }
            | Self::Unknown { ty, .. }
            | Self::Validation { ty, .. } => *ty,
        }
    }

    pub(super) fn port(&self, table: &TypeTable) -> Option<super::aliases::Port> {
        let Self::Input {
            local,
            path,
            ty,
            queue,
            capture,
        } = self
        else {
            return None;
        };
        let Ty::Channel { inner } = table.resolve(value::strip_mutable(*ty, table)) else {
            return None;
        };
        Some(super::aliases::Port {
            kind: super::aliases::StorageKind::Channel(*ty),
            channel_capture: *capture,
            queue: queue.clone(),
            input: *local,
            selection: path.clone(),
            pointee: *inner,
            capture: None,
        })
    }

    fn from_port(port: &super::aliases::Port) -> Option<Self> {
        let super::aliases::StorageKind::Channel(ty) = port.kind else {
            return None;
        };
        Some(Self::Input {
            capture: port.channel_capture,
            queue: port.queue.clone(),
            local: port.input,
            path: port.selection.clone(),
            ty,
        })
    }
}

pub(super) struct Write {
    pub target: Target,
    pub ty: TypeId,
    pub value: Value,
}

pub(super) fn allocation(function: DefId, point: Point, ty: TypeId) -> Value {
    let mut value = Value::default();
    value.queues.insert(
        Vec::new(),
        FxHashSet::from_iter([Handle::new(Target::Allocation {
            ty,
            function,
            point,
            constructor: None,
        })]),
    );
    value
}

pub(super) fn channel_slots(ty: TypeId, table: &TypeTable) -> Vec<(Path, TypeId)> {
    fn visit(
        ty: TypeId,
        path: &mut Path,
        seen: &mut FxHashSet<TypeId>,
        table: &TypeTable,
        result: &mut Vec<(Path, TypeId)>,
    ) {
        let ty = value::strip_mutable(ty, table);
        if !seen.insert(ty) {
            return;
        }
        match table.resolve(ty) {
            Ty::Channel { .. } => result.push((path.clone(), ty)),
            Ty::List(inner) | Ty::Set(inner) => {
                path.push(Slot::Element);
                visit(*inner, path, seen, table, result);
                path.pop();
            }
            Ty::Map(key, payload) => {
                for (slot, ty) in [(Slot::Key, *key), (Slot::Value, *payload)] {
                    path.push(slot);
                    visit(ty, path, seen, table, result);
                    path.pop();
                }
            }
            _ => {
                for (slot, ty) in value::inline_slots(ty, table) {
                    path.push(slot);
                    visit(ty, path, seen, table, result);
                    path.pop();
                }
            }
        }
        seen.remove(&ty);
    }
    let mut result = Vec::new();
    visit(
        ty,
        &mut Vec::new(),
        &mut FxHashSet::default(),
        table,
        &mut result,
    );
    result
}

pub(super) fn seed_inputs(
    value: &mut Value,
    ty: TypeId,
    local: Local,
    source_ty: TypeId,
    prefix: &[Slot],
    table: &TypeTable,
) {
    for (path, channel_ty) in channel_slots(ty, table) {
        let full = [prefix.to_vec(), path.clone()].concat();
        let capture = prefix.last().and_then(|slot| match slot {
            Slot::Capture(owner, index, ty)
                if matches!(
                    table.resolve(value::strip_mutable(*ty, table)),
                    Ty::Channel { .. }
                ) =>
            {
                Some(Capture {
                    owner: *owner,
                    index: *index,
                    ty: *ty,
                })
            }
            _ => None,
        });
        value.queues.entry(path).or_default().insert(Handle {
            target: Target::Input {
                capture,
                queue: None,
                local,
                path: value::normalize(source_ty, full, table),
                ty: channel_ty,
            },
            capture,
        });
    }
}

pub(super) fn opaque_handles(
    ty: TypeId,
    site: super::address::OpaqueSite,
    table: &TypeTable,
) -> Value {
    let mut result = Value::default();
    for (path, channel_ty) in channel_slots(ty, table) {
        result
            .queues
            .entry(path)
            .or_default()
            .insert(Handle::new(Target::Unknown {
                site,
                ty: channel_ty,
                constructor: None,
            }));
    }
    result
}

pub(super) fn validation_handles(
    ty: TypeId,
    site: super::address::OpaqueSite,
    table: &TypeTable,
) -> Value {
    let mut value = Value::default();
    for (path, ty) in channel_slots(ty, table) {
        value
            .queues
            .entry(path.clone())
            .or_default()
            .insert(Handle::new(Target::Validation {
                site,
                ty,
                selection: path,
            }));
    }
    value
}

impl Flow<'_> {
    fn queue_type_matches(&self, actual: TypeId, expected: TypeId) -> bool {
        matches!(
            spark_types::check_expr::cast_kind(
                value::strip_mutable(actual, self.table),
                value::strip_mutable(expected, self.table),
                self.table
            ),
            Ok(spark_types::check_expr::CastKind::Identity
                | spark_types::check_expr::CastKind::Deferred)
        )
    }

    pub(super) fn channel_call(
        &mut self,
        func: &Operand,
        args: &[Operand],
        result_ty: TypeId,
        point: Point,
        state: &State,
    ) -> Option<super::effects::CallOutcome> {
        let def = self.function.resolve_callee_def_id(func)?;
        let info = self.program?.definitions?.get(def)?;
        if !matches!(info.kind, DefinitionKind::BuiltinMethod) {
            return None;
        }
        let receiver = args.first()?;
        let Ty::Channel { inner } = self.table.resolve(value::strip_mutable(
            super::operand_type(receiver, self.function),
            self.table,
        )) else {
            return None;
        };
        let source = self.operand(receiver, state);
        let mut targets: FxHashSet<_> = source
            .root_queue_handles()
            .filter(|handle| {
                self.queue_type_matches(handle.ty(), super::operand_type(receiver, self.function))
            })
            .cloned()
            .collect();
        if targets.is_empty() {
            targets.extend(
                opaque_handles(
                    super::operand_type(args.first()?, self.function),
                    super::address::OpaqueSite {
                        function: self.function.def_id,
                        point,
                    },
                    self.table,
                )
                .root_queue_handles()
                .cloned(),
            );
        }
        let mut receiver = Value::default();
        receiver.queues.insert(Vec::new(), targets.clone());
        for target in &targets {
            if let Some(port) = target.target.port(self.table) {
                self.queue_ports.insert(port);
            }
        }
        let mut queues = Vec::new();
        let returned = if info.name == kw::SEND {
            let payload = self.operand(args.get(1)?, state);
            queues.extend(targets.into_iter().map(|handle| Write {
                target: handle.target,
                ty: *inner,
                value: payload.clone(),
            }));
            Value::default()
        } else if info.name == kw::RECV || info.name == kw::TRY_RECV {
            let payload = self.queue_payload(&receiver, *inner, state);
            if info.name == kw::TRY_RECV {
                payload.prefixed(Slot::Optional, result_ty, self.table)
            } else {
                payload
            }
        } else if info.name == kw::CLOSE {
            Value::default()
        } else {
            return None;
        };
        Some(super::effects::CallOutcome {
            tasks: Vec::new(),
            completed_task: None,
            clobbered_copies: Vec::new(),
            future_proof: CallProof::default(),
            place_writes: Vec::new(),
            returned,
            memory: Vec::new(),
            queues,
            queue_contents: Vec::new(),
        })
    }

    pub(super) fn queue_payload(&self, handle: &Value, inner: TypeId, state: &State) -> Value {
        let mut result = Value::default();
        for (path, targets) in &handle.queues {
            if !path.is_empty() && *path != [Slot::Tail] {
                continue;
            }
            for target in targets
                .iter()
                .flat_map(|handle| self.queue_targets(&handle.target))
            {
                if let Some(value) = state.queues.get(&(target.clone(), inner)) {
                    result.join(
                        &value
                            .clone()
                            .sourced(&super::demand::Storage::Queue(target.clone(), inner)),
                    );
                }
                match &target {
                    Target::Input {
                        local,
                        path,
                        ty,
                        queue,
                        capture,
                    } => {
                        result.join(&self.symbolic_queue_payload(
                            *local,
                            path,
                            *ty,
                            queue.as_ref(),
                            *capture,
                            inner,
                        ));
                    }
                    Target::Validation { site, .. } => {
                        let mut hole = value::validation_hole(inner, self.table);
                        hole.join(&validation_handles(inner, *site, self.table));
                        result.join(&hole);
                    }
                    Target::Unknown { site, .. } => {
                        let mut unknown = value::tainted(
                            inner,
                            &FxHashSet::from_iter([super::Origin::Opaque {
                                function: site.function,
                                point: site.point,
                            }]),
                            self.table,
                        );
                        for leaf in unknown.leaves.values_mut() {
                            leaf.opaque.insert(*site);
                        }
                        unknown.join(&opaque_handles(inner, *site, self.table));
                        result.join(&unknown);
                    }
                    Target::Allocation { .. } => {}
                }
            }
        }
        result
    }

    fn symbolic_queue_payload(
        &self,
        local: Local,
        path: &Path,
        channel: TypeId,
        parent: Option<&super::address::QueueSelection>,
        capture: Option<Capture>,
        inner: TypeId,
    ) -> Value {
        use super::address::{Dependency, Intent, QueueSelection, Selector};
        let mut prefix = path.clone();
        prefix.push(Slot::Element);
        let mut result = value::inputs(
            inner,
            local,
            self.function.locals[local.index()].ty,
            &prefix,
            self.table,
        );
        let basis = |relative: &Path| {
            let (channel, projection) = parent.map_or_else(
                || (channel, relative.clone()),
                |parent| {
                    (
                        parent.channel,
                        [
                            parent.projection.clone(),
                            vec![Slot::Element],
                            relative.clone(),
                        ]
                        .concat(),
                    )
                },
            );
            let payload_ty =
                value::child_type(channel, &Slot::Element, self.table).unwrap_or(TypeId::ERROR);
            QueueSelection {
                capture: parent.map_or(capture, |parent| parent.capture),
                channel,
                projection: value::normalize(payload_ty, projection, self.table),
            }
        };
        for ((relative, _, _), leaf) in &mut result.leaves {
            leaf.dependencies.clear();
            leaf.dependencies.insert(Dependency::InputValue(Selector {
                channel_capture: None,
                queue: Some(basis(relative)),
                input: local,
                selection: path.clone(),
                intent: Intent::Value,
                expected: Some(self.selected_type(inner, relative)).filter(|ty| {
                    matches!(
                        self.table.resolve(value::strip_mutable(*ty, self.table)),
                        Ty::Ref { .. }
                    )
                }),
                capture: None,
            }));
        }
        for (relative, handles) in &mut result.futures {
            *handles = handles
                .iter()
                .map(|handle| super::tasks::Handle {
                    target: super::tasks::Target::Input {
                        input: local,
                        path: path.clone(),
                        ty: handle.target.ty(),
                        capture: None,
                        queue: Some(basis(relative)),
                    },
                    pending: handle.pending,
                    capture: handle.capture,
                })
                .collect();
        }
        for (relative, targets) in &mut result.queues {
            *targets = targets
                .iter()
                .map(|handle| Handle {
                    target: Target::Input {
                        capture: None,
                        queue: Some(basis(relative)),
                        local,
                        path: path.clone(),
                        ty: handle.ty(),
                    },
                    capture: None,
                })
                .collect();
        }
        result
    }

    pub(super) fn qualify_queued_pointee_handles(
        &self,
        value: &mut Value,
        address: &super::address::Selector,
        payload: &super::address::TypedPath,
    ) {
        let Some(queue) = &address.queue else {
            return;
        };
        let root =
            value::child_type(queue.channel, &Slot::Element, self.table).unwrap_or(TypeId::ERROR);
        for (relative, handles) in &mut value.queues {
            let projection = value::normalize(
                root,
                [
                    queue.projection.clone(),
                    vec![payload.kind.slot()],
                    payload.projection.clone(),
                    relative.clone(),
                ]
                .concat(),
                self.table,
            );
            *handles = handles
                .iter()
                .map(|handle| Handle {
                    target: Target::Input {
                        capture: None,
                        queue: Some(super::address::QueueSelection {
                            capture: queue.capture,
                            channel: queue.channel,
                            projection: projection.clone(),
                        }),
                        local: address.input,
                        path: address.selection.clone(),
                        ty: handle.ty(),
                    },
                    capture: handle.capture,
                })
                .collect();
        }
    }

    pub(super) fn reachable_queue_payloads(&self, root: &Value, state: &State) -> Value {
        let mut result = root.clone();
        let mut pending: Vec<_> = root
            .queues
            .values()
            .flatten()
            .flat_map(|handle| self.queue_targets(&handle.target))
            .collect();
        let mut seen = FxHashSet::default();
        while let Some(target) = pending.pop() {
            if !seen.insert(target.clone()) {
                continue;
            }
            let Some(inner) = value::child_type(target.ty(), &Slot::Element, self.table) else {
                continue;
            };
            let mut handle = Value::default();
            handle
                .queues
                .insert(Vec::new(), FxHashSet::from_iter([Handle::new(target)]));
            let payload = self.queue_payload(&handle, inner, state);
            pending.extend(
                payload
                    .queues
                    .values()
                    .flatten()
                    .flat_map(|handle| self.queue_targets(&handle.target)),
            );
            result.join(&payload.prefixed(Slot::Tail, TypeId::ERROR, self.table));
        }
        result
    }

    pub(super) fn report_queue_sends(&self, diagnostics: &mut spark_diagnostics::Diagnostics) {
        use spark_diagnostics::{Diagnostic, Label};
        let mut reported = FxHashSet::default();
        for (point, payload) in &self.queue_escapes {
            for root in payload.frame_sources(self.function.def_id) {
                if !reported.insert((*point, root)) {
                    continue;
                }
                diagnostics.push(Diagnostic::error("cannot send a reference to local storage through a channel that may outlive this function")
                    .with_label(Label::primary(crate::errors::span_for_point(*point, self.function), "this queue retains the borrowed value"))
                    .with_label(Label::secondary(crate::errors::span_for_point(root, self.function), "borrowed storage belongs to this function")));
            }
        }
    }

    pub(super) fn record_queue_prefix(&mut self, writes: &[Write], point: Point, state: &State) {
        let mut prefix = State {
            queues: state.queues.clone(),
            exposed_queues: state.exposed_queues.clone(),
            ..State::default()
        };
        for write in writes {
            for target in self.queue_targets(&write.target) {
                prefix
                    .queues
                    .entry((target, write.ty))
                    .or_default()
                    .join(&write.value);
            }
        }
        for write in writes {
            if self.queue_targets(&write.target).iter().any(|target| {
                target.may_outlive_function() || prefix.exposed_queues.contains(target)
            }) {
                self.expose_queue_handles(&write.value, &mut prefix);
            }
        }
        for write in writes {
            self.record_queue_send(&write.target, &write.value, write.ty, point, &prefix);
        }
    }

    pub(super) fn record_queue_send(
        &mut self,
        target: &Target,
        payload: &Value,
        ty: TypeId,
        point: Point,
        state: &State,
    ) {
        self.ensure_transfer(point, state);
        if let Some(port) = target.port(self.table) {
            self.queue_ports.insert(port);
        }
        if target.may_outlive_function() || state.exposed_queues.contains(target) {
            let reachable = self.reachable_queue_payloads(payload, state);
            self.queue_escapes
                .entry(point)
                .or_default()
                .join(&reachable);
            self.transfers
                .entry(point)
                .or_default()
                .seed(&reachable.all_sources());
        }
        self.queue_sends
            .entry((point, target.clone(), ty))
            .or_default()
            .join(payload);
    }

    pub(super) fn expose_queue_handles(&self, payload: &Value, state: &mut State) {
        let mut pending: Vec<_> = payload
            .queues
            .values()
            .flatten()
            .map(|handle| handle.target.clone())
            .collect();
        while let Some(target) = pending.pop() {
            for target in self.queue_targets(&target) {
                if !state.exposed_queues.insert(target.clone()) {
                    continue;
                }
                for ((candidate, _), payload) in &state.queues {
                    if *candidate == target {
                        pending.extend(
                            payload
                                .queues
                                .values()
                                .flatten()
                                .map(|handle| handle.target.clone()),
                        );
                    }
                }
            }
        }
    }

    pub(super) fn import_queue_contents(
        &mut self,
        target: &Target,
        payload: &Value,
        ty: TypeId,
        point: Point,
        state: &mut State,
    ) {
        self.ensure_transfer(point, state);
        let payload = &self.freeze_sources(point, payload.clone());
        for target in self.queue_targets(target) {
            self.record_path_store(
                point,
                super::demand::Coordinate {
                    storage: super::demand::Storage::Queue(target.clone(), ty),
                    path: Vec::new(),
                },
                payload,
                false,
            );
            let stored = state.queues.entry((target, ty)).or_default();
            stored.join(payload);
            stored.sources.clear();
        }
    }

    pub(super) fn queue_send(
        &mut self,
        target: &Target,
        payload: &Value,
        ty: TypeId,
        point: Point,
        state: &mut State,
    ) {
        self.ensure_transfer(point, state);
        let payload = &self.freeze_sources(point, payload.clone());
        if let Some(port) = target.port(self.table) {
            self.queue_ports.insert(port);
        }
        for target in self.queue_targets(target) {
            self.queue_send_one(target, payload, ty, point, state);
        }
    }

    fn queue_targets(&self, target: &Target) -> FxHashSet<Target> {
        let Some(port) = target.port(self.table) else {
            return FxHashSet::from_iter([target.clone()]);
        };
        if self.aliases.is_cell(&port) {
            return Target::from_port(&self.aliases.representative(&port))
                .into_iter()
                .collect();
        }
        let family = self.family(&port);
        let mut result: FxHashSet<_> = family.roots.iter().filter_map(Target::from_port).collect();
        if family.residual {
            result.insert(target.clone());
        }
        result
    }

    pub(super) fn queue_locations(
        &self,
        handle: &Value,
    ) -> Option<FxHashSet<super::aliases::Location>> {
        let mut result = FxHashSet::default();
        for (path, targets) in &handle.queues {
            if !path.is_empty() && *path != [Slot::Tail] {
                continue;
            }
            for target in targets {
                let target = &target.target;
                match target {
                    Target::Input { .. } => {
                        let port = target.port(self.table)?;
                        if !self.aliases.is_cell(&port) {
                            let family = self.family(&port);
                            result.extend(
                                family
                                    .roots
                                    .into_iter()
                                    .map(super::aliases::Location::Input),
                            );
                            if family.residual {
                                result.insert(super::aliases::Location::Residual {
                                    port,
                                    excluded: family.excluded,
                                });
                            }
                            continue;
                        }
                        result.insert(super::aliases::Location::Input(
                            self.aliases.representative(&port),
                        ));
                    }
                    Target::Allocation { .. } => {
                        result.insert(super::aliases::Location::ChannelAllocation(target.clone()));
                    }
                    Target::Unknown { .. } | Target::Validation { .. } => return None,
                }
            }
        }
        (!result.is_empty()).then_some(result)
    }

    fn queue_send_one(
        &mut self,
        target: Target,
        payload: &Value,
        ty: TypeId,
        point: Point,
        state: &mut State,
    ) {
        self.record_queue_send(&target, payload, ty, point, state);
        if target.may_outlive_function() || state.exposed_queues.contains(&target) {
            self.expose_queue_handles(payload, state);
        }
        self.record_path_store(
            point,
            super::demand::Coordinate {
                storage: super::demand::Storage::Queue(target.clone(), ty),
                path: Vec::new(),
            },
            payload,
            false,
        );
        let stored = state.queues.entry((target, ty)).or_default();
        stored.join(payload);
        stored.sources.clear();
    }
}

pub(in crate::provenance) fn remap_queues(
    queues: &mut FxHashMap<Path, FxHashSet<Handle>>,
    bindings: &FxHashMap<Target, Target>,
) {
    for handles in queues.values_mut() {
        *handles = handles
            .iter()
            .map(|handle| Handle {
                target: bindings
                    .get(&handle.target)
                    .cloned()
                    .unwrap_or_else(|| handle.target.clone()),
                capture: handle.capture,
            })
            .collect();
    }
}
