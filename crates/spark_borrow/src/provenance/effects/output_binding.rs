// TAG [SPARK-114]

use rustc_hash::FxHashMap;

use super::{CallActuals, Flow, MirFunction, Operand, State, Summary};
use crate::provenance::tasks::{
    lifecycle::{Tags, Token},
    outputs::{CallbackOutput, CallbackOutputs},
    Role,
};

impl Flow<'_> {
    pub(super) fn bind_callback_outputs(
        &mut self,
        summary: &Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        args: &[Operand],
        state: &State,
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
                    let mut mapped = Tags::default();
                    for tag in tags {
                        let Some(index) = tag.input().index().checked_sub(1) else {
                            continue;
                        };
                        let Some(operand) = args.get(index) else {
                            continue;
                        };
                        let (Operand::Copy(place) | Operand::Move(place)) = operand else {
                            continue;
                        };
                        if !place.projections.is_empty() {
                            continue;
                        }
                        for actual in state.input_owners.tags(&Role::root(place.local)) {
                            mapped.insert(
                                if matches!(tag, Token::Derived(_))
                                    || matches!(operand, Operand::Copy(_))
                                {
                                    actual.derived()
                                } else {
                                    actual
                                },
                            );
                        }
                    }
                    lineage.insert(path.clone(), mapped);
                }
                (*ty, CallbackOutput { callback, lineage })
            })
            .collect()
    }
}
