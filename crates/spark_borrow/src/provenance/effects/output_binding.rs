// TAG [SPARK-114]

use rustc_hash::FxHashMap;

use super::{CallActuals, Flow, MirFunction, Operand, Summary};
use crate::provenance::tasks::{
    lifecycle::{Tags, Token},
    outputs::{CallbackOutput, CallbackOutputs},
};

impl Flow<'_> {
    pub(super) fn retained_capture_witnesses(
        &self,
        target: Option<(&Summary, &MirFunction)>,
        actuals: CallActuals<'_>,
    ) -> Vec<crate::provenance::tasks::Witness> {
        use crate::provenance::tasks::Role;
        let Some(place) = actuals.callee_place else {
            return Vec::new();
        };
        let state = actuals.read.state;
        let (sources, _) = self.environment_sources(&Operand::Copy(place.clone()), state);
        let roles = if let Some((summary, function)) = target {
            summary
                .retained_owner_tags
                .iter()
                .filter_map(|tag| {
                    let index = tag.input().0.checked_sub(function.param_count + 1)? as usize;
                    let (_, ty) = *function.captures.get(index)?;
                    Some(
                        sources
                            .iter()
                            .map(move |source| Role::capture(*source, function.def_id, index, ty)),
                    )
                })
                .flatten()
                .collect::<rustc_hash::FxHashSet<_>>()
        } else {
            sources
                .iter()
                .flat_map(|source| state.future_copies.anchored(*source))
                .filter(|role| !role.selection().is_empty())
                .collect()
        };
        roles
            .iter()
            .map(|role| state.future_copies.role_witness(role))
            .collect()
    }

    pub(super) fn bind_owner_tags(
        &self,
        tags: &Tags,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        args: &[Operand],
    ) -> Tags {
        let mut mapped = Tags::default();
        for tag in tags {
            let (source, copied) = self.owner_input(tag.input(), function, actuals, args);
            mapped.extend(source.into_iter().map(|actual| {
                if copied || matches!(tag, Token::Derived(_)) {
                    actual.derived()
                } else {
                    actual
                }
            }));
        }
        mapped
    }

    pub(super) fn bind_callback_outputs(
        &mut self,
        summary: &Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        args: &[Operand],
    ) -> CallbackOutputs {
        summary
            .returned_callbacks
            .iter()
            .map(|(ty, output)| {
                let callback = self.bind(
                    &output.callback,
                    function,
                    CallActuals { ty: *ty, ..actuals },
                );
                let mut lineage = FxHashMap::default();
                for (path, tags) in &output.lineage {
                    let mapped = self.bind_owner_tags(tags, function, actuals, args);
                    lineage.insert(path.clone(), mapped);
                }
                (*ty, CallbackOutput { callback, lineage })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
