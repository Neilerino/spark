// TAG [SPARK-114]

//! Conservative reads and result lineage at calls without a Spark body.

use super::{
    value, AccessKind, Accesses, CallActuals, Flow, FxHashSet, Operand, Origin, Origins, TypeId,
    Value,
};

impl Flow<'_> {
    fn is_builtin_callee(&self, callee: &Value) -> bool {
        let Some(definitions) = self.program.and_then(|context| context.definitions) else {
            return false;
        };
        !callee.callees.is_empty()
            && callee.callees.values().all(|targets| {
                !targets.open
                    && !targets.unresolved
                    && !targets.known.is_empty()
                    && targets.known.iter().all(|target| {
                        definitions.get(*target).is_some_and(|definition| {
                            matches!(
                                definition.kind,
                                spark_resolve::DefinitionKind::BuiltinFunction
                                    | spark_resolve::DefinitionKind::BuiltinMethod
                            )
                        })
                    })
            })
    }

    pub(super) fn opaque_result(&mut self, args: &[Operand], actuals: CallActuals<'_>) -> Value {
        let CallActuals {
            callee,
            arguments,
            ty,
            point,
            state,
            ..
        } = actuals;
        let expanded: Vec<_> = arguments
            .iter()
            .map(|value| self.reachable_queue_payloads(value, state))
            .collect();
        let inspected = if self.is_builtin_callee(callee) {
            arguments
        } else {
            expanded.as_slice()
        };
        let arguments = expanded.as_slice();
        let mut origins = callee.origins();
        for arg in arguments {
            origins.extend(arg.origins());
        }
        let accesses = self.opaque_accesses(callee, args, inspected, state);
        self.record_accesses(point, accesses, state);
        origins.remove(&Origin::ValidationHole);
        origins.insert(Origin::Unproven(point));
        let mut tainted = value::tainted(ty, &origins, self.table);
        let parents: Origins = arguments
            .iter()
            .chain(std::iter::once(callee))
            .flat_map(|value| {
                value.origins().into_iter().chain(
                    value
                        .leaves
                        .values()
                        .flat_map(|leaf| leaf.parents.iter().cloned()),
                )
            })
            .filter(|origin| *origin != Origin::ValidationHole)
            .collect();
        let sources: super::super::demand::Frontier = arguments
            .iter()
            .chain(std::iter::once(callee))
            .flat_map(Value::all_sources)
            .collect();
        let mut dependencies = FxHashSet::default();
        for value in arguments.iter().chain(std::iter::once(callee)) {
            for leaf in value.leaves.values() {
                dependencies.extend(leaf.dependencies.iter().cloned());
                dependencies.extend(
                    leaf.endpoints
                        .iter()
                        .cloned()
                        .map(super::super::address::Dependency::Address),
                );
            }
        }
        for (key, leaf) in &mut tainted.leaves {
            leaf.parents.extend(parents.iter().cloned());
            leaf.dependencies.extend(dependencies.iter().cloned());
            leaf.opaque.insert(super::super::address::OpaqueSite {
                function: self.function.def_id,
                point,
            });
            tainted.sources.insert(key.clone(), sources.clone());
        }
        tainted
    }

    pub(super) fn opaque_copy_clobbers(
        &mut self,
        func: &Operand,
        args: &[Operand],
        actuals: CallActuals<'_>,
    ) -> Vec<spark_mir::Local> {
        if self.is_builtin_callee(actuals.callee) {
            return Vec::new();
        }
        let mut clobbered = FxHashSet::default();
        let callee_ty = super::super::operand_type(func, self.function);
        let parameters = match self
            .table
            .resolve(value::strip_mutable(callee_ty, self.table))
        {
            spark_types::Ty::Function { params, .. } => params.as_slice(),
            _ => &[],
        };
        for (index, (operand, argument)) in args.iter().zip(actuals.arguments).enumerate() {
            let ty = parameters
                .get(index)
                .copied()
                .unwrap_or_else(|| super::super::operand_type(operand, self.function));
            for target in self.tail_write_targets(argument.clone(), ty, actuals.state) {
                clobbered.extend(self.possibly_written_locals(&target, actuals.state));
            }
        }
        clobbered.into_iter().collect()
    }

    pub(super) fn bound_copy_clobbers(
        &mut self,
        summary: &super::Summary,
        function: &super::MirFunction,
        actuals: CallActuals<'_>,
    ) -> Vec<spark_mir::Local> {
        let mut clobbered = FxHashSet::default();
        if summary.unknown_copy_clobber {
            self.unknown_copy_clobber = true;
            clobbered.extend(actuals.state.live.iter().copied());
        }
        for view in &summary.written_inputs {
            let mut input = self.port_input(&view.root, function, actuals);
            if !actuals.is_cell(&view.root) {
                input = self.residual_reference(&input, &view.root, function, actuals);
            }
            let target = self.project_reference(&input, view.root.pointee, &view.path);
            clobbered.extend(self.possibly_written_locals(&target, actuals.state));
        }
        clobbered.into_iter().collect()
    }

    fn opaque_accesses(
        &self,
        callee: &Value,
        args: &[Operand],
        arguments: &[Value],
        state: &super::super::State,
    ) -> Accesses {
        let builtin = self.is_builtin_callee(callee);
        let mut accesses = Accesses::default();
        for (argument, operand) in arguments.iter().zip(args) {
            let ty = match operand {
                Operand::Copy(place) | Operand::Move(place) => Some(self.place_type(place)),
                Operand::Constant(_) => None,
            };
            if builtin {
                accesses.observe_reference(argument, ty, &[], AccessKind::Value);
            } else {
                let reference = self.tail_reference_value(
                    argument.clone(),
                    ty.unwrap_or(TypeId::ERROR),
                    None,
                    state,
                );
                accesses.observe_reference(&reference, None, &[], AccessKind::Value);
            }
        }
        if !builtin {
            let environment = self.tail_reference_value(callee.clone(), TypeId::ERROR, None, state);
            accesses.observe_reference(&environment, None, &[], AccessKind::Value);
        }
        accesses
    }
}
