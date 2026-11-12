// TAG [SPARK-114]

use super::closure::{Activity, Import};
use crate::provenance::{
    demand::Coordinate,
    tasks::views::{Callback, View},
    Flow, Point, State,
};

impl Flow<'_> {
    pub(in crate::provenance) fn prepare_task_activity(
        &mut self,
        point: Point,
        state: &mut State,
    ) -> Activity {
        loop {
            let activity = state.rooted_activity();
            let before = self.transfers[&point].clone();
            let basis = state.activity_basis();
            let mut changed = false;
            for import in activity.imports() {
                changed |= self.import_callback(import, point, state);
            }
            if !changed
                && basis == state.activity_basis()
                && before.same_sources(&self.transfers[&point])
            {
                return activity;
            }
        }
    }

    fn import_callback(&mut self, import: Import, point: Point, state: &mut State) -> bool {
        let Some(current) = state.stored_value(&import.at.storage) else {
            return false;
        };
        let mut current =
            self.freeze_sources(point, current.into_owned().sourced(&import.at.storage));
        let mut before = current.clone();
        before.clear_sources();
        let mut callback = self.freeze_sources(point, import.record.callback);
        self.import_task_sources(&mut callback, point, state);
        current
            .task_views
            .entry(import.at.path)
            .or_default()
            .entry(import.actor)
            .or_insert_with(View::unknown)
            .callbacks
            .entry(import.record.callback_ty)
            .or_default()
            .join(&Callback::seal(callback));
        self.record_path_store(
            point,
            Coordinate {
                storage: import.at.storage.clone(),
                path: Vec::new(),
            },
            &current,
            false,
        );
        current.clear_sources();
        let changed = current != before;
        Self::replace_task_carrier(&import.at.storage, current, state);
        changed
    }
}
