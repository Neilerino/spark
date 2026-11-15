// TAG [SPARK-114]

//! Keep publication snapshots in a finite graph outside immutable receive keys.

use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::provenance::{observations::ReadView, read::ReadSite, Point, State};
use spark_types::TypeId;

use super::{
    CallActuals, Flow, Key, MirFunction, Operand, PrefixWrites, PublicationValues, Receive,
    Summary, Value,
};

pub(in crate::provenance) type ReceiveSnapshots = FxHashMap<Arc<Receive>, PublicationValues>;

impl Flow<'_> {
    pub(in crate::provenance) fn receive_payload(
        &mut self,
        receiver: &Value,
        inner: TypeId,
        point: Point,
        state: &State,
    ) -> Value {
        let site = ReadSite {
            function: self.function.def_id,
            point,
        };
        let receive = Receive::at(site, &state.reached_prefix);
        let queries = std::cell::RefCell::default();
        let read = ReadView::current(state)
            .with_receive(Some(&receive))
            .with_receive_queries(&queries);
        let payload = self.queue_payload_in(receiver, inner, read);
        self.record_receive_snapshots(queries.into_inner(), state);
        payload
    }

    fn record_receive_snapshots(&mut self, receipts: FxHashSet<Arc<Receive>>, state: &State) {
        for receive in receipts {
            self.receive_snapshots
                .entry(receive)
                .or_default()
                .join(&state.publications);
        }
    }

    pub(super) fn bind_receive_snapshots(
        &mut self,
        summary: &Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        key: &Key,
        args: Option<&[Operand]>,
        execution: super::prefix::Execution,
    ) -> FxHashMap<Arc<Receive>, PrefixWrites> {
        let mut bound = FxHashMap::default();
        for (receive, values) in &summary.receive_snapshots {
            let writes = self.bind_publications(values, function, actuals, key, args);
            let mut caller_values = actuals.read.state.publications.clone();
            self.remember_publications_as(&writes, execution, &mut caller_values);
            if let Some(source) = &receive.source {
                for target in self.bind_queue_target(source, function, actuals) {
                    let caller_receive = Arc::new(Receive {
                        source: Some(target),
                        ..receive.including(&actuals.read.state.reached_prefix)
                    });
                    self.receive_snapshots
                        .entry(caller_receive)
                        .or_default()
                        .join(&caller_values);
                }
            }
            bound.insert(receive.clone(), writes);
        }
        bound
    }
}
