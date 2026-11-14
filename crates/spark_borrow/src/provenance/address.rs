// TAG [SPARK-114]

use spark_mir::Local;
use spark_resolve::DefId;
use spark_types::TypeId;

use super::{Path, Point};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Intent {
    Value,
    Payload(TypedPath),
    Address(TypedPath),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum DerefKind {
    All,
    Residual,
}

impl DerefKind {
    pub const fn slot(self) -> super::Slot {
        match self {
            Self::All => super::Slot::Deref,
            Self::Residual => super::Slot::ResidualDeref,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct TypedPath {
    pub kind: DerefKind,
    pub pointee: TypeId,
    pub projection: Path,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Selector {
    pub input: Local,
    pub selection: Path,
    pub intent: Intent,
    pub expected: Option<TypeId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Endpoint {
    Frame {
        function: DefId,
        root: Point,
        local: Local,
        root_ty: TypeId,
        pointee: TypeId,
        location: Path,
    },
    Input(Selector),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Dependency {
    Address(Endpoint),
    InputValue(Selector),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct OpaqueSite {
    pub function: DefId,
    pub point: Point,
}

impl super::Flow<'_> {
    pub(super) fn reference_metadata(
        &self,
        place: &spark_mir::Place,
        point: Point,
        state: &super::State,
    ) -> super::value::Leaf {
        use spark_mir::{Place, Projection};
        place
            .projections
            .iter()
            .rposition(|p| matches!(p, Projection::Deref))
            .map_or_else(
                || {
                    let root_ty = self.function.locals[place.local.index()].ty;
                    let (location, _) = self.projection_path(root_ty, &place.projections);
                    let mut leaf = super::value::Leaf::default();
                    leaf.endpoints.insert(Endpoint::Frame {
                        function: self.function.def_id,
                        root: point,
                        local: place.local,
                        root_ty,
                        pointee: self.place_type(place),
                        location: super::value::normalize(root_ty, location, self.table),
                    });
                    leaf
                },
                |index| {
                    let prefix = Place {
                        local: place.local,
                        projections: place.projections[..index].to_vec(),
                    };
                    let reference = self.read(&prefix, state);
                    let pointee = super::child_type(
                        self.place_type(&prefix),
                        &super::Slot::Deref,
                        self.table,
                    )
                    .unwrap_or(TypeId::ERROR);
                    let (projection, _) =
                        self.projection_path(pointee, &place.projections[index + 1..]);
                    self.project_reference(&reference, pointee, &projection)
                },
            )
    }

    pub(super) fn project_reference(
        &self,
        reference: &super::Value,
        pointee: TypeId,
        projection: &Path,
    ) -> super::value::Leaf {
        let mut result = super::value::Leaf {
            parents: reference.address_support(),
            ..Default::default()
        };
        for ((path, _), leaf) in &reference.leaves {
            if !path.is_empty() && *path != [super::Slot::Tail] {
                continue;
            }
            result.opaque.extend(leaf.opaque.iter().copied());
            for endpoint in &leaf.endpoints {
                result
                    .endpoints
                    .insert(self.project_endpoint(endpoint, pointee, projection));
            }
            for dependency in &leaf.dependencies {
                match dependency {
                    Dependency::InputValue(_) if !leaf.opaque.is_empty() => {
                        result.dependencies.insert(dependency.clone());
                    }
                    Dependency::InputValue(selector) => {
                        let mut selector = selector.clone();
                        if let Intent::Payload(payload) = &selector.intent {
                            selector.selection = super::value::normalize(
                                self.function.locals[selector.input.index()].ty,
                                [
                                    selector.selection,
                                    vec![payload.kind.slot()],
                                    payload.projection.clone(),
                                ]
                                .concat(),
                                self.table,
                            );
                        }
                        selector.expected = Some(self.selected_type(pointee, projection));
                        selector.intent = Intent::Address(TypedPath {
                            kind: DerefKind::All,
                            pointee,
                            projection: super::value::normalize(
                                pointee,
                                projection.clone(),
                                self.table,
                            ),
                        });
                        result.endpoints.insert(Endpoint::Input(selector));
                    }
                    Dependency::Address(endpoint) => {
                        result
                            .dependencies
                            .insert(Dependency::Address(endpoint.clone()));
                    }
                }
            }
        }
        result
    }

    fn project_endpoint(
        &self,
        endpoint: &Endpoint,
        pointee: TypeId,
        projection: &Path,
    ) -> Endpoint {
        match endpoint {
            Endpoint::Frame {
                function,
                root,
                local,
                root_ty,
                pointee: source_pointee,
                location,
            } => Endpoint::Frame {
                function: *function,
                root: *root,
                local: *local,
                root_ty: *root_ty,
                pointee: self.selected_type(*source_pointee, projection),
                location: super::value::normalize(
                    *root_ty,
                    [location.clone(), projection.clone()].concat(),
                    self.table,
                ),
            },
            Endpoint::Input(selector) => {
                let mut selector = selector.clone();
                selector.expected = Some(self.selected_type(pointee, projection));
                selector.intent = match &selector.intent {
                    Intent::Address(payload) => Intent::Address(TypedPath {
                        kind: payload.kind,
                        pointee: payload.pointee,
                        projection: super::value::normalize(
                            payload.pointee,
                            [payload.projection.clone(), projection.clone()].concat(),
                            self.table,
                        ),
                    }),
                    Intent::Value | Intent::Payload(_) => Intent::Address(TypedPath {
                        kind: DerefKind::All,
                        pointee,
                        projection: super::value::normalize(
                            pointee,
                            projection.clone(),
                            self.table,
                        ),
                    }),
                };
                Endpoint::Input(selector)
            }
        }
    }
}
