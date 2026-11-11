// TAG [SPARK-114]

use super::{CallActuals, Flow, MirFunction, Value};
use crate::provenance::tasks::{
    views::{self, Carrier, Subject, View, Views},
    Target,
};

impl Flow<'_> {
    fn bind_new_task_view(
        &mut self,
        view: &View,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        actor: &Subject,
    ) -> View {
        let (history, unbound) = self.bind_history(&view.history, function, actuals, actor);
        let mut bound_view = View {
            fallback: view.fallback || unbound,
            history,
            ..View::default()
        };
        for (ty, callback) in &view.callbacks {
            let callback = self.bind(
                &callback.value(),
                function,
                CallActuals { ty: *ty, ..actuals },
            );
            bound_view.callbacks.insert(*ty, Carrier::seal(callback));
        }

        bound_view
    }

    pub(super) fn bind_task_views(
        &mut self,
        summary: &Value,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> Views {
        let mut result = Views::default();
        for (path, handles) in &summary.futures {
            for handle in handles {
                let mut bound = Vec::new();
                if let Target::Input {
                    input,
                    path: selection,
                    ty,
                    capture,
                    queue,
                } = &handle.target
                {
                    let selected = self.select_input_value(
                        &super::Selector {
                            input: *input,
                            selection: selection.clone(),
                            intent: super::Intent::Value,
                            expected: Some(*ty),
                            capture: None,
                            channel_capture: None,
                            queue: queue.clone(),
                        },
                        function,
                        actuals,
                    );
                    for (selected_path, incoming) in &selected.futures {
                        for current in incoming.iter().filter(|current| {
                            (capture.is_none()
                                || current.capture.is_none()
                                || current.capture == *capture)
                                && matches!(
                                    spark_types::check_expr::cast_kind(
                                        current.target.ty(),
                                        *ty,
                                        self.table
                                    ),
                                    Ok(spark_types::check_expr::CastKind::Identity
                                        | spark_types::check_expr::CastKind::Deferred)
                                )
                        }) {
                            let view = views::contribution(&selected, selected_path, current);
                            bound.push((
                                Subject {
                                    target: current.target.clone(),
                                    capture: handle.capture,
                                },
                                view,
                            ));
                        }
                    }
                } else {
                    let view = views::contribution(summary, path, handle);
                    for current in self.bind_task_target(&handle.target, function, actuals) {
                        let actor = Subject {
                            target: current.target.clone(),
                            capture: handle.capture,
                        };
                        let bound_view = self.bind_new_task_view(&view, function, actuals, &actor);
                        bound.push((
                            Subject {
                                target: current.target,
                                capture: handle.capture,
                            },
                            bound_view,
                        ));
                    }
                }
                if bound.is_empty() {
                    for current in self.bind_task_target(&handle.target, function, actuals) {
                        bound.push((
                            Subject {
                                target: current.target,
                                capture: handle.capture,
                            },
                            View::unknown(),
                        ));
                    }
                }
                for (subject, mut view) in bound {
                    if !handle.pending {
                        view = View::default();
                    }
                    result
                        .entry(path.clone())
                        .or_default()
                        .entry(subject)
                        .or_default()
                        .join(&view);
                }
            }
        }
        result
    }
}

pub(super) fn export(value: &Value, function: spark_resolve::DefId) -> Views {
    value
        .task_views
        .iter()
        .map(|(path, subjects)| {
            let subjects = subjects
                .iter()
                .map(|(subject, view)| {
                    let callbacks = view
                        .callbacks
                        .iter()
                        .map(|(ty, callback)| {
                            (
                                *ty,
                                Carrier::seal(super::export_value(&callback.value(), function)),
                            )
                        })
                        .collect();
                    (
                        subject.clone(),
                        View {
                            callbacks,
                            fallback: view.fallback,
                            history: view.history.exported(function),
                        },
                    )
                })
                .collect();
            (path.clone(), subjects)
        })
        .collect()
}

#[cfg(test)]
mod tests;
