// TAG [SPARK-114]

//! Bind possible executed-prefix effects independently of normal return.

use super::{Accesses, CallActuals, Flow, MirFunction, Point, State, Summary, Value};
use crate::provenance::{channels::Write, obligations::Boundary};

#[derive(Default)]
pub(in crate::provenance) struct Prefix {
    accesses: Accesses,
    pub queues: Vec<Write>,
    obligations: Vec<(Boundary, Value)>,
}

impl Prefix {
    pub fn merge(&mut self, other: Self) {
        self.accesses.merge(other.accesses);
        self.queues.extend(other.queues);
        self.obligations.extend(other.obligations);
    }
}

impl Flow<'_> {
    pub(super) fn bind_prefix(
        &mut self,
        summary: &Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> Prefix {
        let obligations = summary
            .obligations
            .iter()
            .map(|(boundary, value)| {
                let bound = self.bind(
                    value,
                    function,
                    CallActuals {
                        ty: boundary.ty(),
                        ..actuals
                    },
                );
                (boundary.clone(), bound)
            })
            .collect();
        Prefix {
            accesses: self.bound_accesses(summary, function, actuals),
            queues: self.bind_queue_writes(summary, function, actuals),
            obligations,
        }
    }

    pub(in crate::provenance) fn observe_prefix(
        &mut self,
        prefix: &Prefix,
        point: Point,
        state: &State,
    ) {
        self.record_accesses(point, prefix.accesses.clone(), state);
        self.record_queue_prefix(&prefix.queues, point, state);
        for (boundary, value) in &prefix.obligations {
            self.record_obligation(point, boundary.clone(), value);
        }
    }
}
