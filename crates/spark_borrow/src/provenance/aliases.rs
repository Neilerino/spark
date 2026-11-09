//! Exact equality contexts for incoming reference storage.

// TAG [SPARK-114]

use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::{Local, MirFunction};
use spark_resolve::DefId;
use spark_types::{TypeId, TypeTable};

use super::address::{Endpoint, Intent};
use super::{child_type, Flow, Path, Slot, Value};

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub(super) struct Aliases(Vec<(Local, Local)>);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Key {
    pub function: DefId,
    pub aliases: Aliases,
}

impl Key {
    pub fn distinct(function: DefId) -> Self {
        Self {
            function,
            aliases: Aliases::default(),
        }
    }
}

impl Aliases {
    pub fn representative(&self, input: Local) -> Local {
        self.0
            .iter()
            .find_map(|(local, representative)| (*local == input).then_some(*representative))
            .unwrap_or(input)
    }

    pub fn body_variants(function: &MirFunction, ports: &[Local], table: &TypeTable) -> Vec<Self> {
        let exclusive = |local: Local| {
            let ty = function.locals[local.index()].ty;
            matches!(table.resolve(ty), spark_types::Ty::Mutable(_))
                || matches!(
                    table.resolve(super::value::strip_mutable(ty, table)),
                    spark_types::Ty::Ref { mutable: true, .. }
                )
        };
        Self::variants(function, ports, table, |left, right, equal| {
            !(equal && exclusive(left) && exclusive(right))
        })
    }

    pub fn call_variants(
        function: &MirFunction,
        ports: &[Local],
        table: &TypeTable,
        locations: &FxHashMap<Local, Option<FxHashSet<Location>>>,
    ) -> Vec<Self> {
        Self::variants(function, ports, table, |left, right, equal| {
            match (&locations[&left], &locations[&right]) {
                (Some(left), Some(right)) if left.len() == 1 && right.len() == 1 => {
                    equal == (left == right)
                }
                (Some(left), Some(right)) if left.is_disjoint(right) => !equal,
                _ => true,
            }
        })
    }

    pub fn members(&self, representative: Local) -> impl Iterator<Item = Local> + '_ {
        std::iter::once(representative).chain(
            self.0
                .iter()
                .filter_map(move |(local, root)| (*root == representative).then_some(*local)),
        )
    }

    fn variants(
        function: &MirFunction,
        ports: &[Local],
        table: &TypeTable,
        allowed: impl Fn(Local, Local, bool) -> bool,
    ) -> Vec<Self> {
        fn visit(
            function: &MirFunction,
            ports: &[Local],
            table: &TypeTable,
            index: usize,
            current: &mut Aliases,
            allowed: &impl Fn(Local, Local, bool) -> bool,
            result: &mut Vec<Aliases>,
        ) {
            if index == ports.len() {
                result.push(current.clone());
                return;
            }
            let local = ports[index];
            let ty = child_type(function.locals[local.index()].ty, &Slot::Deref, table)
                .unwrap_or(TypeId::ERROR);
            let mut representatives = FxHashSet::default();
            let mut candidates = vec![local];
            for previous in &ports[..index] {
                let representative = current.representative(*previous);
                if !representatives.insert(representative) {
                    continue;
                }
                let other = child_type(
                    function.locals[representative.index()].ty,
                    &Slot::Deref,
                    table,
                )
                .unwrap_or(TypeId::ERROR);
                if matches!(
                    spark_types::check_expr::cast_kind(ty, other, table),
                    Ok(spark_types::check_expr::CastKind::Identity
                        | spark_types::check_expr::CastKind::Deferred)
                ) {
                    candidates.push(representative);
                }
            }
            for representative in candidates {
                if ports[..index].iter().any(|previous| {
                    !allowed(
                        local,
                        *previous,
                        representative == current.representative(*previous),
                    )
                }) {
                    continue;
                }
                if representative != local {
                    current.0.push((local, representative));
                }
                visit(function, ports, table, index + 1, current, allowed, result);
                if representative != local {
                    current.0.pop();
                }
            }
        }
        let mut result = Vec::new();
        visit(
            function,
            ports,
            table,
            0,
            &mut Self::default(),
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
    Input(Local),
}

impl Flow<'_> {
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
                Endpoint::Input(selector)
                    if selector.selection.is_empty()
                        && matches!(&selector.intent, Intent::Address(payload) if payload.projection.is_empty()) =>
                {
                    Location::Input(self.aliases.representative(selector.input))
                }
                Endpoint::Frame { .. } | Endpoint::Input(_) => return None,
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
        let ports: Vec<_> = (1..=20).map(Local).collect();
        assert_eq!(
            Aliases::body_variants(&function, &ports, &table),
            vec![Aliases::default()]
        );
        let locations = ports
            .iter()
            .map(|local| {
                (
                    *local,
                    Some(std::iter::once(Location::Input(*local)).collect()),
                )
            })
            .collect();
        assert_eq!(
            Aliases::call_variants(&function, &ports, &table, &locations),
            vec![Aliases::default()]
        );
        let same = ports
            .iter()
            .map(|local| {
                (
                    *local,
                    Some(std::iter::once(Location::Input(Local(1))).collect()),
                )
            })
            .collect();
        let variants = Aliases::call_variants(&function, &ports, &table, &same);
        assert_eq!(variants.len(), 1, "actual calls remain modeled even if existing permission checks reject aliasing mutable inputs");
        assert!(ports
            .iter()
            .all(|local| variants[0].representative(*local) == Local(1)));
    }
}
