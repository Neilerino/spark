// TAG [SPARK-114]

use crate::provenance::tasks::proofs::{Guarantee, Proofs};

use super::{
    Context, DefId, Diagnostics, Flow, FxHashMap, FxHashSet, Key, Mir, Ports, Summaries, Summary,
    TypeTable,
};

pub fn analyze_program(
    mir: &Mir,
    table: &TypeTable,
    definitions: Option<&spark_resolve::DefinitionTable>,
    diagnostics: &mut Diagnostics,
) -> FxHashMap<DefId, Vec<Vec<crate::regions::Region>>> {
    let facts: FxHashMap<_, _> = mir
        .functions
        .iter()
        .map(|(def, function)| (*def, crate::facts::extract_facts(function)))
        .collect();
    let (summaries, ports, proofs) =
        infer_summaries_with_definitions(mir, table, &facts, definitions);
    let context = Context {
        mir,
        summaries: &summaries,
        proofs: Some(&proofs),
        ports: &ports,
        definitions,
    };
    let mut regions: FxHashMap<_, Vec<_>> = FxHashMap::default();
    for key in summaries.keys() {
        let function = &mir.functions[&key.function];
        let mut flow = Flow::new(function, table, Some(context));
        flow.aliases = key.aliases.clone();
        flow.callables = key.callables.clone();
        flow.solve(&facts[&key.function]);
        flow.report_returns(&facts[&key.function], diagnostics);
        flow.report_reads(diagnostics);
        flow.report_obligations(diagnostics);
        flow.report_queue_sends(diagnostics);
        regions
            .entry(key.function)
            .or_default()
            .push(flow.carrier_regions(&facts[&key.function]));
    }
    regions
}

#[cfg(test)]
pub(super) fn infer_summaries(
    mir: &Mir,
    table: &TypeTable,
    facts: &FxHashMap<DefId, crate::facts::BorrowFacts>,
) -> (Summaries, Ports) {
    let (summaries, ports, _) = infer_summaries_with_definitions(mir, table, facts, None);
    (summaries, ports)
}

fn infer_summaries_with_definitions(
    mir: &Mir,
    table: &TypeTable,
    facts: &FxHashMap<DefId, crate::facts::BorrowFacts>,
    definitions: Option<&spark_resolve::DefinitionTable>,
) -> (Summaries, Ports, Proofs) {
    let program = Program {
        mir,
        table,
        facts,
        definitions,
    };
    let mut keys = mir
        .functions
        .keys()
        .map(|def| Key::distinct(*def))
        .collect();
    let mut ports = mir.functions.keys().map(|def| (*def, Vec::new())).collect();
    'vocabulary: loop {
        let (base, frozen_ports) = program.may(&keys, ports, None);
        keys = base.keys().cloned().collect();
        let (mut proofs, discovered, requests) = program.proofs(&base, &frozen_ports, None);
        if discovered != frozen_ports || requests.iter().any(|key| !keys.contains(key)) {
            keys.extend(requests);
            program.expand_keys(&mut keys, &discovered);
            ports = discovered;
            continue;
        }
        if !proofs.values().any(Guarantee::has_effect) {
            return (base, frozen_ports, proofs);
        }
        loop {
            let (refined, refined_ports) = program.may(&keys, frozen_ports.clone(), Some(&proofs));
            if refined_ports != frozen_ports || refined.keys().any(|key| !keys.contains(key)) {
                keys.extend(refined.keys().cloned());
                program.expand_keys(&mut keys, &refined_ports);
                ports = refined_ports;
                continue 'vocabulary;
            }
            let (checked, discovered, requests) =
                program.proofs(&refined, &refined_ports, Some(&proofs));
            if discovered != refined_ports || requests.iter().any(|key| !keys.contains(key)) {
                keys.extend(requests);
                program.expand_keys(&mut keys, &discovered);
                ports = discovered;
                continue 'vocabulary;
            }
            if checked == proofs {
                return (refined, refined_ports, proofs);
            }
            proofs = checked;
        }
    }
}

struct Program<'a> {
    mir: &'a Mir,
    table: &'a TypeTable,
    facts: &'a FxHashMap<DefId, crate::facts::BorrowFacts>,
    definitions: Option<&'a spark_resolve::DefinitionTable>,
}

impl Program<'_> {
    fn expand_keys(&self, keys: &mut FxHashSet<Key>, ports: &Ports) {
        let mut expanded = FxHashSet::default();
        for key in keys.iter() {
            for aliases in key.aliases.body_variants(
                &self.mir.functions[&key.function],
                &ports[&key.function],
                self.table,
            ) {
                expanded.insert(Key {
                    aliases,
                    ..key.clone()
                });
            }
        }
        keys.extend(expanded);
    }

    fn proofs(
        &self,
        summaries: &Summaries,
        ports: &Ports,
        bounds: Option<&Proofs>,
    ) -> (Proofs, Ports, FxHashSet<Key>) {
        let mut proofs: Proofs = summaries
            .iter()
            .map(|(key, summary)| {
                (
                    key.clone(),
                    bounds
                        .and_then(|bounds| bounds.get(key))
                        .cloned()
                        .unwrap_or_else(|| {
                            Guarantee::universe(
                                &self.mir.functions[&key.function],
                                self.table,
                                summary.normal_return,
                            )
                        }),
                )
            })
            .collect();
        let mut discovered = ports.clone();
        let mut requests = FxHashSet::default();
        loop {
            let context = Context {
                mir: self.mir,
                summaries,
                ports,
                definitions: self.definitions,
                proofs: Some(&proofs),
            };
            let mut next = proofs.clone();
            let mut changed = false;
            for (key, proof) in &mut next {
                if !proof.has_effect() {
                    continue;
                }
                let function = &self.mir.functions[&key.function];
                let mut flow = Flow::new(function, self.table, Some(context));
                flow.aliases = key.aliases.clone();
                flow.callables = key.callables.clone();
                flow.solve(&self.facts[&key.function]);
                changed |= proof.intersect(&flow.future_guarantee());
                let summary = flow.summary();
                flow.discover_ports(&summary, discovered.entry(key.function).or_default());
                requests.extend(flow.requests);
            }
            proofs = next;
            if !changed {
                return (proofs, discovered, requests);
            }
        }
    }

    fn may(
        &self,
        keys: &FxHashSet<Key>,
        mut ports: Ports,
        proofs: Option<&Proofs>,
    ) -> (Summaries, Ports) {
        let mut summaries: Summaries = keys
            .iter()
            .cloned()
            .map(|key| (key, Summary::default()))
            .collect();
        loop {
            let context = Context {
                mir: self.mir,
                summaries: &summaries,
                ports: &ports,
                definitions: self.definitions,
                proofs,
            };
            let mut next = summaries.clone();
            let mut discovered = ports.clone();
            let mut requests = FxHashSet::default();
            let mut changed = false;
            for key in summaries.keys() {
                let function = &self.mir.functions[&key.function];
                let mut flow = Flow::new(function, self.table, Some(context));
                flow.aliases = key.aliases.clone();
                flow.callables = key.callables.clone();
                flow.solve(&self.facts[&key.function]);
                let summary = flow.summary();
                flow.discover_ports(&summary, discovered.entry(key.function).or_default());
                changed |= next.entry(key.clone()).or_default().join(&summary);
                requests.extend(flow.requests);
            }
            if discovered != ports {
                let mut body_keys = next.keys().cloned().collect();
                self.expand_keys(&mut body_keys, &discovered);
                requests.extend(body_keys);
                ports = discovered;
                for summary in next.values_mut() {
                    *summary = Summary::default();
                }
                changed = true;
            }
            for key in requests {
                if let std::collections::hash_map::Entry::Vacant(entry) = next.entry(key) {
                    entry.insert(Summary::default());
                    changed = true;
                }
            }
            summaries = next;
            if !changed {
                return (summaries, ports);
            }
        }
    }
}

#[cfg(test)]
mod tests;
