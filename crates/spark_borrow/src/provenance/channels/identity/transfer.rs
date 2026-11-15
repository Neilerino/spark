// TAG [SPARK-114]

use super::{Role, Witness};
use crate::provenance::{channels::Capture, value, Flow, State};
use spark_mir::{AggregateKind, Local, Operand, Place, Rvalue};
use spark_types::{Ty, TypeId};

pub(in crate::provenance) struct Captured {
    source: Local,
    role: Role,
    witness: Witness,
}

impl Captured {
    pub fn execution(
        source: Local,
        destination: Local,
        ty: TypeId,
        adopting: bool,
        state: &State,
    ) -> Vec<Self> {
        state
            .channel_copies
            .anchored(source)
            .filter(|role| {
                if adopting {
                    role.callback_type().is_none()
                } else {
                    role.callback_type() == Some(ty)
                }
            })
            .filter_map(|role| {
                Some(Self {
                    source,
                    role: role.in_execution(destination, ty)?,
                    witness: state.channel_copies.witness(role),
                })
            })
            .collect()
    }

    pub fn install_all(
        mut captures: Vec<Self>,
        destination: Local,
        excluded: &[Local],
        state: &mut State,
    ) {
        if state.invalidated_copies.contains(&destination) {
            return;
        }
        captures.retain(|capture| !state.invalidated_copies.contains(&capture.source));
        for capture in &mut captures {
            capture.role = capture.role.reanchor(destination);
        }
        let peers = crate::provenance::identity::transferred_peers(
            captures
                .iter()
                .map(|capture| (&capture.role, &capture.witness.peers)),
        );
        for (capture, peers) in captures.iter_mut().zip(peers) {
            capture.witness.peers.retain(|peer| {
                peer.local() != destination
                    && !excluded.contains(&peer.local())
                    && !state.invalidated_copies.contains(&peer.local())
            });
            capture.witness.peers.extend(peers);
        }
        for capture in captures {
            state
                .channel_copies
                .assign_role(capture.role, capture.witness, &state.live);
        }
    }
}

pub(in crate::provenance) enum Transfer {
    Fresh,
    Copy { source: Local, witness: Witness },
    Captures(Vec<Captured>),
}

impl Transfer {
    pub fn install(self, destination: Local, state: &mut State) {
        if state.invalidated_copies.contains(&destination) {
            return;
        }
        match self {
            Self::Fresh => state.channel_copies.seed(destination),
            Self::Copy {
                source,
                mut witness,
            } => {
                if state.invalidated_copies.contains(&source) {
                    return;
                }
                witness
                    .peers
                    .retain(|peer| !state.invalidated_copies.contains(&peer.local()));
                state
                    .channel_copies
                    .assign_role(Role::Local(destination), witness, &state.live);
            }
            Self::Captures(captures) => Captured::install_all(captures, destination, &[], state),
        }
    }
}

impl Flow<'_> {
    pub(in crate::provenance) fn channel_transfer(
        &self,
        rvalue: &Rvalue,
        destination: &Place,
        state: &State,
    ) -> Option<Transfer> {
        if !destination.projections.is_empty() {
            return None;
        }
        if let Rvalue::Aggregate(AggregateKind::Channel, _) = rvalue {
            return Some(Transfer::Fresh);
        }
        if let Rvalue::Aggregate(AggregateKind::Closure { func_def_id }, operands) = rvalue {
            let captures = operands
                .iter()
                .enumerate()
                .filter_map(|(index, operand)| {
                    let (Operand::Copy(source) | Operand::Move(source)) = operand else {
                        return None;
                    };
                    if !source.projections.is_empty() || !state.live.contains(&source.local) {
                        return None;
                    }
                    let ty = self.place_type(source);
                    if !matches!(
                        self.table.resolve(value::strip_mutable(ty, self.table)),
                        Ty::Channel { .. }
                    ) {
                        return None;
                    }
                    Some(Captured {
                        source: source.local,
                        role: Role::Capture {
                            local: destination.local,
                            capture: Capture {
                                owner: *func_def_id,
                                index,
                                ty,
                            },
                            callback_ty: None,
                        },
                        witness: state.channel_copies.witness(&Role::Local(source.local)),
                    })
                })
                .collect();
            return Some(Transfer::Captures(captures));
        }
        let (Rvalue::Use(operand) | Rvalue::Cast { operand, .. }) = rvalue else {
            return None;
        };
        let (Operand::Copy(source) | Operand::Move(source)) = operand else {
            return None;
        };
        if !source.projections.is_empty() || !state.live.contains(&source.local) {
            return None;
        }
        let ty = value::strip_mutable(self.place_type(source), self.table);
        if ty != value::strip_mutable(self.place_type(destination), self.table) {
            return None;
        }
        match self.table.resolve(ty) {
            Ty::Channel { .. } => Some(Transfer::Copy {
                source: source.local,
                witness: state.channel_copies.witness(&Role::Local(source.local)),
            }),
            Ty::Function { .. } => Some(Transfer::Captures(
                state
                    .channel_copies
                    .anchored(source.local)
                    .filter(|role| {
                        matches!(
                            role,
                            Role::Capture {
                                callback_ty: None,
                                ..
                            }
                        )
                    })
                    .map(|role| Captured {
                        source: source.local,
                        role: role.reanchor(destination.local),
                        witness: state.channel_copies.witness(role),
                    })
                    .collect(),
            )),
            _ => None,
        }
    }
}
