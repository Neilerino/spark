// TAG [SPARK-114]

use crate::provenance::{
    tasks::{views, Witness},
    Flow, Path, Point, Slot, State, Value,
};

impl Flow<'_> {
    pub(in crate::provenance) fn prepare_completion_descendants(
        &self,
        witness: &Witness,
        point: Point,
        state: &State,
    ) -> Vec<(Value, Path)> {
        if witness
            .locals()
            .any(|local| state.current_execution_record(local).is_some())
        {
            return Vec::new();
        }
        let mut pending = Vec::new();
        for role in &witness.peers {
            let Some(value) = self.current_role_value(role, state) else {
                continue;
            };
            for (path, handles) in &value.futures {
                if !path.is_empty() && *path != [Slot::Tail] {
                    continue;
                }
                for handle in handles.iter().filter(|handle| handle.pending) {
                    let (records, _) = views::pending_records(&value, path, handle, state);
                    for record in records {
                        pending.push((
                            self.freeze_sources(point, record.callback),
                            role.selection().to_vec(),
                        ));
                    }
                }
            }
        }
        pending
    }
}
