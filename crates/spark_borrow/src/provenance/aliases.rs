//! Exact equality contexts for incoming reference storage.

// TAG [SPARK-114]

use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::{Local, MirFunction};
use spark_resolve::DefId;
use spark_types::{TypeId, TypeTable};

use super::address::{DerefKind, Endpoint, Intent, Selector, TypedPath};
use super::{child_type, Flow, Path, Slot, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum StorageKind {
    Reference,
    Channel(TypeId),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Port {
    pub channel_capture: Option<super::channels::Capture>,
    pub kind: StorageKind,
    pub queue: Option<super::address::QueueSelection>,
    pub input: Local,
    pub selection: Path,
    pub pointee: TypeId,
    pub capture: Option<super::value::ReferenceCapture>,
}

impl Port {
    pub fn compatible(&self, other: &Self, table: &TypeTable) -> bool {
        matches!(
            (self.kind, other.kind),
            (StorageKind::Reference, StorageKind::Reference)
                | (StorageKind::Channel(_), StorageKind::Channel(_))
        ) && compatible(self.pointee, other.pointee, table)
    }

    pub fn from_selector(
        selector: &Selector,
        function: &MirFunction,
        table: &TypeTable,
    ) -> Option<Self> {
        let root = function.locals[selector.input.index()].ty;
        let mut basis = selector.clone();
        let (selection, pointee) = match &selector.intent {
            Intent::Address(payload) => (selector.selection.clone(), payload.pointee),
            Intent::Value => {
                let ty = selector
                    .expected
                    .unwrap_or_else(|| Self::selected_type(root, &selector.selection, table));
                (
                    selector.selection.clone(),
                    child_type(ty, &Slot::Deref, table)?,
                )
            }
            Intent::Payload(payload) => {
                let ty = selector.expected.unwrap_or_else(|| {
                    Self::selected_type(payload.pointee, &payload.projection, table)
                });
                basis.select_payload(payload, root, table);
                (
                    basis.selection.clone(),
                    child_type(ty, &Slot::Deref, table)?,
                )
            }
        };
        Some(Self {
            kind: StorageKind::Reference,
            channel_capture: None,
            queue: basis.queue,
            input: selector.input,
            selection,
            pointee,
            capture: selector.address_capture(),
        })
    }

    pub const fn root(input: Local, pointee: TypeId) -> Self {
        Self {
            kind: StorageKind::Reference,
            channel_capture: None,
            queue: None,
            input,
            selection: Vec::new(),
            pointee,
            capture: None,
        }
    }

    pub fn selector(&self) -> Selector {
        if let StorageKind::Channel(ty) = self.kind {
            return Selector {
                channel_capture: self.channel_capture,
                queue: self.queue.clone(),
                capture: self.capture,
                input: self.input,
                selection: self.selection.clone(),
                intent: Intent::Value,
                expected: Some(ty),
            };
        }
        Selector {
            channel_capture: None,
            queue: self.queue.clone(),
            capture: self.capture,
            input: self.input,
            selection: self.selection.clone(),
            intent: Intent::Address(TypedPath {
                kind: DerefKind::All,
                pointee: self.pointee,
                projection: Vec::new(),
            }),
            expected: Some(self.pointee),
        }
    }

    pub fn exact(&self) -> bool {
        !self.selection.iter().any(|slot| {
            matches!(
                slot,
                Slot::Element | Slot::Key | Slot::Value | Slot::Tail | Slot::ResidualDeref
            )
        })
    }

    fn selected_type(mut ty: TypeId, path: &[Slot], table: &TypeTable) -> TypeId {
        for slot in path {
            ty = child_type(ty, slot, table).unwrap_or(TypeId::ERROR);
        }
        ty
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct View {
    pub root: Port,
    pub path: Path,
}

impl View {
    pub const fn whole(root: Port) -> Self {
        Self {
            root,
            path: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub(super) struct Aliases {
    exact: Vec<(Port, Port)>,
    families: Vec<Family>,
    disjoint: Vec<(Port, Port)>,
    single_cells: Vec<Port>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Family {
    pub port: Port,
    pub roots: Vec<Port>,
    pub residual: bool,
    pub excluded: Vec<Port>,
}

fn compatible(left: TypeId, right: TypeId, table: &TypeTable) -> bool {
    matches!(
        spark_types::check_expr::cast_kind(left, right, table),
        Ok(spark_types::check_expr::CastKind::Identity
            | spark_types::check_expr::CastKind::Deferred)
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Key {
    pub function: DefId,
    pub aliases: Aliases,
    pub callables: super::callbacks::Inputs,
}

impl Key {
    pub fn distinct(function: DefId) -> Self {
        Self {
            function,
            aliases: Aliases::default(),
            callables: super::callbacks::Inputs::default(),
        }
    }
}

impl Aliases {
    pub fn is_cell(&self, port: &Port) -> bool {
        port.exact() || self.single_cells.contains(port)
    }

    pub fn representative(&self, input: &Port) -> Port {
        self.exact
            .iter()
            .find_map(|(port, representative)| (port == input).then(|| representative.clone()))
            .unwrap_or_else(|| input.clone())
    }

    pub fn body_variants(
        &self,
        function: &MirFunction,
        ports: &[Port],
        table: &TypeTable,
    ) -> Vec<Self> {
        let exclusive = |port: &Port| {
            if matches!(port.kind, StorageKind::Channel(_)) {
                return false;
            }
            let ty = Port::selected_type(
                function.locals[port.input.index()].ty,
                &port.selection,
                table,
            );
            matches!(table.resolve(ty), spark_types::Ty::Mutable(_))
                || matches!(
                    table.resolve(super::value::strip_mutable(ty, table)),
                    spark_types::Ty::Ref { mutable: true, .. }
                )
        };
        let mut variants =
            Self::variants(ports, table, &self.single_cells, |left, right, equal| {
                !(equal && exclusive(left) && exclusive(right))
            });
        for aliases in &mut variants {
            aliases.set_families(ports, table, None, &Self::default());
        }
        variants
    }

    pub fn call_variants(
        ports: &[Port],
        table: &TypeTable,
        locations: &FxHashMap<Port, Option<FxHashSet<Location>>>,
        caller: &Self,
    ) -> Vec<Self> {
        let single_cells: Vec<_> = ports
            .iter()
            .filter(|port| !port.exact() && locations[*port].as_ref().and_then(singleton).is_some())
            .cloned()
            .collect();
        let mut variants =
            Self::variants(ports, table, &single_cells, |left, right, equal| {
                match (&locations[left], &locations[right]) {
                    (Some(left), Some(right))
                        if singleton(left).is_some() && singleton(right).is_some() =>
                    {
                        equal == (left == right)
                    }
                    (Some(left), Some(right)) if disjoint(left, right, caller) => !equal,
                    _ => true,
                }
            });
        for aliases in &mut variants {
            aliases.set_families(ports, table, Some(locations), caller);
        }
        variants
    }

    fn set_families(
        &mut self,
        ports: &[Port],
        table: &TypeTable,
        locations: Option<&FxHashMap<Port, Option<FxHashSet<Location>>>>,
        caller: &Self,
    ) {
        for port in ports {
            if self.is_cell(port) {
                continue;
            }
            let mut family = self.universal_family(port, ports, table);
            if let Some(locations) = locations {
                if let Some(actual) = &locations[port] {
                    family.roots.retain(|root| {
                        locations[root]
                            .as_ref()
                            .is_none_or(|named| !disjoint(actual, named, caller))
                    });
                    family.residual = !actual.iter().all(|target| {
                        target.unique()
                            && family.excluded.iter().any(|root| {
                                locations[root]
                                    .as_ref()
                                    .and_then(singleton)
                                    .is_some_and(|named| named == target)
                            })
                    });
                }
            }
            self.families.push(family);
        }
        if let Some(locations) = locations {
            for (index, left) in self.families.iter().enumerate() {
                for right in &self.families[index + 1..] {
                    if let (Some(left_targets), Some(right_targets)) =
                        (&locations[&left.port], &locations[&right.port])
                    {
                        if disjoint(left_targets, right_targets, caller) {
                            self.disjoint.push((left.port.clone(), right.port.clone()));
                        }
                    }
                }
            }
        }
    }

    fn universal_family(&self, port: &Port, ports: &[Port], table: &TypeTable) -> Family {
        let mut roots = Vec::new();
        for named in ports
            .iter()
            .filter(|named| self.is_cell(named) && port.compatible(named, table))
        {
            let root = self.representative(named);
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        Family {
            port: port.clone(),
            excluded: roots.clone(),
            roots,
            residual: true,
        }
    }

    pub fn family(&self, port: &Port, ports: &[Port], table: &TypeTable) -> Family {
        if self.is_cell(port) {
            let root = self.representative(port);
            return Family {
                port: port.clone(),
                roots: vec![root.clone()],
                excluded: vec![root],
                residual: false,
            };
        }
        self.families
            .iter()
            .find(|family| &family.port == port)
            .cloned()
            .unwrap_or_else(|| self.universal_family(port, ports, table))
    }

    pub fn residuals_overlap(&self, left: &Port, right: &Port) -> bool {
        if self.is_cell(left) && self.is_cell(right) {
            return self.representative(left) == self.representative(right);
        }
        !self
            .disjoint
            .iter()
            .any(|(a, b)| (a == left && b == right) || (a == right && b == left))
    }

    pub fn members<'a>(&'a self, representative: &'a Port) -> impl Iterator<Item = Port> + 'a {
        std::iter::once(representative.clone()).chain(
            self.exact
                .iter()
                .filter(move |(_, root)| root == representative)
                .map(|(port, _)| port.clone()),
        )
    }

    fn variants(
        ports: &[Port],
        table: &TypeTable,
        single_cells: &[Port],
        allowed: impl Fn(&Port, &Port, bool) -> bool,
    ) -> Vec<Self> {
        fn visit(
            ports: &[Port],
            table: &TypeTable,
            index: usize,
            current: &mut Aliases,
            allowed: &impl Fn(&Port, &Port, bool) -> bool,
            result: &mut Vec<Aliases>,
        ) {
            if index == ports.len() {
                result.push(current.clone());
                return;
            }
            let local = ports[index].clone();
            let mut representatives = FxHashSet::default();
            let mut candidates = vec![local.clone()];
            for previous in &ports[..index] {
                let representative = current.representative(previous);
                if !representatives.insert(representative.clone()) {
                    continue;
                }
                if local.compatible(&representative, table) {
                    candidates.push(representative);
                }
            }
            for representative in candidates {
                if ports[..index].iter().any(|previous| {
                    !allowed(
                        &local,
                        previous,
                        representative == current.representative(previous),
                    )
                }) {
                    continue;
                }
                if representative != local {
                    current.exact.push((local.clone(), representative.clone()));
                }
                visit(ports, table, index + 1, current, allowed, result);
                if representative != local {
                    current.exact.pop();
                }
            }
        }
        let cells: Vec<_> = ports
            .iter()
            .filter(|port| single_cells.contains(port))
            .cloned()
            .collect();
        let ports: Vec<_> = ports
            .iter()
            .filter(|port| port.exact() || cells.contains(port))
            .cloned()
            .collect();
        let mut result = Vec::new();
        visit(
            &ports,
            table,
            0,
            &mut Self {
                single_cells: cells,
                ..Self::default()
            },
            &allowed,
            &mut result,
        );
        result
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Location {
    Frame {
        function: DefId,
        local: Local,
        path: Path,
    },
    Input(Port),
    ChannelAllocation(super::channels::Target),
    Residual {
        port: Port,
        excluded: Vec<Port>,
    },
}

impl Location {
    pub(super) const fn unique(&self) -> bool {
        !matches!(self, Self::Residual { .. } | Self::ChannelAllocation(_))
    }

    fn may_equal(&self, other: &Self, aliases: &Aliases) -> bool {
        match (self, other) {
            (Self::Residual { port: left, .. }, Self::Residual { port: right, .. }) => {
                aliases.residuals_overlap(left, right)
            }
            (Self::Residual { excluded, .. }, Self::Input(port))
            | (Self::Input(port), Self::Residual { excluded, .. }) => !excluded.contains(port),
            (Self::Residual { .. }, Self::Frame { .. })
            | (Self::Frame { .. }, Self::Residual { .. })
            | (Self::Residual { .. }, Self::ChannelAllocation(_))
            | (Self::ChannelAllocation(_), Self::Residual { .. }) => false,
            _ => self == other,
        }
    }
}

fn singleton(locations: &FxHashSet<Location>) -> Option<&Location> {
    if locations.len() != 1 {
        return None;
    }
    locations.iter().next().filter(|location| location.unique())
}

fn disjoint(left: &FxHashSet<Location>, right: &FxHashSet<Location>, aliases: &Aliases) -> bool {
    !left
        .iter()
        .any(|left| right.iter().any(|right| left.may_equal(right, aliases)))
}

impl Flow<'_> {
    pub(super) fn family(&self, port: &Port) -> Family {
        let ports = self
            .program
            .map(|context| context.ports[&self.function.def_id].as_slice())
            .unwrap_or_default();
        self.aliases.family(port, ports, self.table)
    }

    pub(super) fn residual_peers(&self, port: &Port) -> Vec<Port> {
        self.program
            .into_iter()
            .flat_map(|context| context.ports[&self.function.def_id].iter())
            .filter(|other| {
                !self.aliases.is_cell(other)
                    && *other != port
                    && port.compatible(other, self.table)
                    && self.family(other).residual
                    && self.aliases.residuals_overlap(port, other)
            })
            .cloned()
            .collect()
    }

    pub(super) fn reference_locations(
        &self,
        value: &Value,
        inner: TypeId,
    ) -> Option<FxHashSet<Location>> {
        let metadata = self.project_reference(value, inner, &Vec::new());
        if !metadata.opaque.is_empty() {
            return None;
        }
        let mut locations = FxHashSet::default();
        for endpoint in &metadata.endpoints {
            let location = match endpoint {
                Endpoint::Frame {
                    lifetime: crate::provenance::address::FrameLifetime::Local,
                    function,
                    local,
                    location,
                    ..
                } if !location.iter().any(|slot| {
                    matches!(slot, Slot::Element | Slot::Key | Slot::Value | Slot::Tail)
                }) =>
                {
                    Location::Frame {
                        function: *function,
                        local: *local,
                        path: location.clone(),
                    }
                }
                Endpoint::Input(selector, _) if matches!(&selector.intent, Intent::Address(payload) if payload.projection.is_empty()) =>
                {
                    let port = Port::from_selector(selector, self.function, self.table)?;
                    if !self.aliases.is_cell(&port) {
                        let family = self.family(&port);
                        if matches!(&selector.intent, Intent::Address(payload) if payload.kind == DerefKind::All)
                        {
                            locations.extend(family.roots.into_iter().map(Location::Input));
                        }
                        if family.residual {
                            locations.insert(Location::Residual {
                                port,
                                excluded: family.excluded,
                            });
                        }
                        continue;
                    }
                    Location::Input(self.aliases.representative(&port))
                }
                Endpoint::Frame { .. } | Endpoint::Input(..) => return None,
            };
            locations.insert(location);
        }
        (!locations.is_empty()).then_some(locations)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{value::Leaf, Origin};
    use super::*;
    use crate::facts::Point;
    use spark_mir::{BasicBlock, BlockId, LocalDecl, Terminator};
    use spark_span::Span;
    use spark_types::{IntWidth, LifetimeId, Ty};

    #[test]
    fn locations_merge_issuance_witnesses_without_equating_alternative_targets() {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let reference = table.intern(Ty::Ref {
            inner: int,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let string = table.intern(Ty::String);
        let map = table.intern(Ty::Map(string, reference));
        let function = MirFunction {
            def_id: DefId::new(9810),
            param_count: 0,
            return_type: int,
            locals: [int, map, map]
                .into_iter()
                .enumerate()
                .map(|(index, ty)| LocalDecl {
                    local: Local(index as u32),
                    ty,
                    name: None,
                    is_user_variable: false,
                    mutable: true,
                    span: Span::DUMMY,
                })
                .collect(),
            blocks: vec![BasicBlock {
                statements: vec![],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };
        let flow = Flow::new(&function, &table, None);
        let mut value = Value::default();
        for (index, local) in [(0, Local(1)), (1, Local(1)), (2, Local(2))] {
            let point = Point::new(BlockId::ENTRY, index);
            value.join(
                &Value::reference(Origin::Loan(point)).with_metadata(&Leaf {
                    endpoints: std::iter::once(Endpoint::Frame {
                        lifetime: crate::provenance::address::FrameLifetime::Local,
                        function: function.def_id,
                        root: point,
                        local,
                        root_ty: map,
                        pointee: map,
                        location: Vec::new(),
                    })
                    .collect(),
                    ..Default::default()
                }),
            );
            assert_eq!(
                flow.reference_locations(&value, map)
                    .map(|locations| locations.len()),
                Some(if index < 2 { 1 } else { 2 }),
                "issuance points are witnesses, but independent cells remain alternatives"
            );
        }
    }
    #[test]
    fn exclusive_and_known_location_constraints_prune_before_partition_expansion() {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let reference = table.intern(Ty::Ref {
            inner: int,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let string = table.intern(Ty::String);
        let map = table.intern(Ty::Map(string, reference));
        let map_ref = table.intern(Ty::Ref {
            inner: map,
            mutable: true,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let function = MirFunction {
            def_id: DefId::new(9813),
            param_count: 20,
            return_type: int,
            locals: std::iter::once(int)
                .chain(std::iter::repeat_n(map_ref, 20))
                .enumerate()
                .map(|(index, ty)| LocalDecl {
                    local: Local(index as u32),
                    ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                })
                .collect(),
            blocks: vec![],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };
        let ports: Vec<_> = (1..=20)
            .map(|index| Port::root(Local(index), map))
            .collect();
        assert_eq!(
            Aliases::default().body_variants(&function, &ports, &table),
            vec![Aliases::default()]
        );
        let locations = ports
            .iter()
            .map(|local| {
                (
                    local.clone(),
                    Some(std::iter::once(Location::Input(local.clone())).collect()),
                )
            })
            .collect();
        assert_eq!(
            Aliases::call_variants(&ports, &table, &locations, &Aliases::default()),
            vec![Aliases::default()]
        );
        let same = ports
            .iter()
            .map(|local| {
                (
                    local.clone(),
                    Some(std::iter::once(Location::Input(Port::root(Local(1), map))).collect()),
                )
            })
            .collect();
        let variants = Aliases::call_variants(&ports, &table, &same, &Aliases::default());
        assert_eq!(variants.len(), 1, "actual calls remain modeled even if existing permission checks reject aliasing mutable inputs");
        assert!(ports
            .iter()
            .all(|local| variants[0].representative(local) == Port::root(Local(1), map)));
    }

    fn family_fixture() -> (TypeTable, MirFunction, Port, Port) {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let reference = table.intern(Ty::Ref {
            inner: int,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let string = table.intern(Ty::String);
        let map = table.intern(Ty::Map(string, reference));
        let map_ref = table.intern(Ty::Ref {
            inner: map,
            mutable: true,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let list = table.intern(Ty::List(map_ref));
        let function = MirFunction {
            def_id: DefId::new(9840),
            param_count: 2,
            return_type: map,
            locals: [map, list, map_ref]
                .into_iter()
                .enumerate()
                .map(|(index, ty)| LocalDecl {
                    local: Local(index as u32),
                    ty,
                    name: None,
                    is_user_variable: false,
                    mutable: true,
                    span: Span::DUMMY,
                })
                .collect(),
            blocks: vec![],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };
        let family = Port {
            kind: crate::provenance::aliases::StorageKind::Reference,
            channel_capture: None,
            queue: None,
            capture: None,
            input: Local(1),
            selection: vec![Slot::Element],
            pointee: map,
        };
        (table, function, family, Port::root(Local(2), map))
    }

    fn frame_location(local: u32) -> Location {
        Location::Frame {
            function: DefId::new(9841),
            local: Local(local),
            path: Vec::new(),
        }
    }

    #[test]
    fn family_support_preserves_many_targets_and_refines_proven_singletons() {
        let (table, function, family, root) = family_fixture();
        let ports = [family.clone(), root.clone()];
        let body = Aliases::default().body_variants(&function, &ports, &table);
        assert_eq!(
            body.len(),
            1,
            "families are not singleton equality partitions"
        );
        let support = body[0].family(&family, &ports, &table);
        assert_eq!(support.roots, vec![root.clone()]);
        assert!(support.residual);
        for (family_locations, root_locations, overlaps, residual) in [
            (
                vec![frame_location(1)],
                vec![frame_location(1)],
                true,
                false,
            ),
            (
                vec![frame_location(1)],
                vec![frame_location(2)],
                false,
                true,
            ),
            (
                vec![frame_location(1), frame_location(2)],
                vec![frame_location(1), frame_location(2)],
                true,
                true,
            ),
        ] {
            let locations = FxHashMap::from_iter([
                (family.clone(), Some(family_locations.into_iter().collect())),
                (root.clone(), Some(root_locations.into_iter().collect())),
            ]);
            let variants = Aliases::call_variants(&ports, &table, &locations, &Aliases::default());
            assert_eq!(variants.len(), 1);
            let support = variants[0].family(&family, &ports, &table);
            if variants[0].is_cell(&family) {
                assert_eq!(
                    variants[0].representative(&family) == variants[0].representative(&root),
                    overlaps
                );
                assert_eq!(support.roots, vec![variants[0].representative(&family)]);
                assert!(!support.residual);
            } else {
                assert_eq!(support.roots.contains(&root), overlaps);
                assert_eq!(support.residual, residual);
            }
        }
    }

    #[test]
    fn residual_overlap_does_not_merge_proven_disjoint_families_through_a_bridge() {
        let (table, _, family, _) = family_fixture();
        let ports: Vec<_> = (1..=3)
            .map(|input| Port {
                kind: crate::provenance::aliases::StorageKind::Reference,
                channel_capture: None,
                queue: None,
                capture: None,
                input: Local(input),
                ..family.clone()
            })
            .collect();
        let locations = FxHashMap::from_iter([
            (
                ports[0].clone(),
                Some(FxHashSet::from_iter([frame_location(1)])),
            ),
            (
                ports[1].clone(),
                Some(FxHashSet::from_iter([frame_location(1), frame_location(2)])),
            ),
            (
                ports[2].clone(),
                Some(FxHashSet::from_iter([frame_location(2)])),
            ),
        ]);
        let variants = Aliases::call_variants(&ports, &table, &locations, &Aliases::default());
        assert_eq!(variants.len(), 1);
        assert!(variants[0].residuals_overlap(&ports[0], &ports[1]));
        assert!(variants[0].residuals_overlap(&ports[1], &ports[2]));
        assert!(!variants[0].residuals_overlap(&ports[0], &ports[2]));
    }

    #[test]
    fn residual_outer_payload_selection_keeps_its_role_before_inner_reference_addressing() {
        let (table, function, family, _) = family_fixture();
        let reference = child_type(family.pointee, &Slot::Value, &table).unwrap();
        let inner = child_type(reference, &Slot::Deref, &table).unwrap();
        let selector = Selector {
            channel_capture: None,
            queue: None,
            capture: None,
            input: family.input,
            selection: family.selection,
            intent: Intent::Payload(TypedPath {
                kind: DerefKind::Residual,
                pointee: family.pointee,
                projection: vec![Slot::Value],
            }),
            expected: Some(reference),
        };
        let pointer = Value::reference(Origin::Input {
            local: Local(1),
            path: Vec::new(),
        })
        .with_metadata(&Leaf {
            dependencies: std::iter::once(super::super::address::Dependency::InputValue(
                selector, None,
            ))
            .collect(),
            ..Default::default()
        });
        let flow = Flow::new(&function, &table, None);
        let metadata = flow.project_reference(&pointer, inner, &Vec::new());
        let expected = vec![Slot::Element, Slot::ResidualDeref, Slot::Value];
        assert_eq!(metadata.endpoints.len(), 1);
        let Endpoint::Input(address, _) = metadata.endpoints.iter().next().unwrap() else {
            panic!("expected symbolic input address");
        };
        assert_eq!(address.selection, expected);
        assert!(
            matches!(&address.intent, Intent::Address(payload) if payload.kind == DerefKind::All && payload.pointee == inner)
        );
        let source = function.locals[1].ty;
        assert_eq!(
            super::super::value::normalize(source, expected.clone(), &table),
            expected
        );
        assert_eq!(Port::selected_type(source, &expected, &table), reference);
    }

    #[test]
    fn only_a_proven_single_target_can_define_storage_after_a_family_write() {
        let (table, function, family, root) = family_fixture();
        let ports = [family.clone(), root.clone()];
        let body = Aliases::default()
            .body_variants(&function, &ports, &table)
            .remove(0);
        let locations = FxHashMap::from_iter([
            (
                family.clone(),
                Some(FxHashSet::from_iter([frame_location(1)])),
            ),
            (
                root.clone(),
                Some(FxHashSet::from_iter([frame_location(1)])),
            ),
        ]);
        let exact =
            Aliases::call_variants(&ports, &table, &locations, &Aliases::default()).remove(0);
        let point = Point::new(BlockId::ENTRY, 0);
        let metadata = Leaf {
            endpoints: std::iter::once(Endpoint::Input(family.selector(), None)).collect(),
            ..Default::default()
        };
        for (aliases, strong) in [(body, false), (exact, true)] {
            let mut flow = Flow::new(&function, &table, None);
            flow.aliases = aliases;
            let mut state = super::super::State::default();
            flow.store_references(
                &metadata,
                &Value::default(),
                family.pointee,
                point,
                &mut state,
            );
            let mut coverage = super::super::writes::Coverage::default();
            if strong {
                coverage.insert(View::whole(flow.aliases.representative(&root)));
            }
            assert_eq!(state.external_defined, coverage);
            assert_eq!(state.actual_definitions, coverage);
            let mut snapshot = super::super::State::default();
            flow.apply_memory_snapshot(
                &metadata,
                &Value::default(),
                family.pointee,
                point,
                &mut snapshot,
            );
            assert_eq!(snapshot.external_defined, coverage);
            assert!(snapshot.actual_definitions.is_empty());
        }
    }
}
