// TAG [SPARK-114]

use crate::provenance::tasks::proofs::{Proof, Proofs};

use super::{
    Context, DefId, Diagnostics, EventKey, Flow, FxHashMap, FxHashSet, Key, Mir, Ports, Summaries,
    Summary, TypeTable,
};

type EventVocabulary = FxHashMap<Key, FxHashSet<EventKey>>;

fn discover_events(events: &mut EventVocabulary, key: &Key, summary: &Summary) -> bool {
    if summary.prefix_memory.is_empty() && summary.queues.is_empty() {
        return false;
    }
    let known = events.entry(key.clone()).or_default();
    let before = known.len();
    known.extend(summary.prefix_memory.keys().cloned().map(EventKey::Memory));
    known.extend(
        summary
            .queues
            .keys()
            .map(|(site, _, _)| EventKey::Queue(*site)),
    );
    known.len() != before
}

fn discover_summary_events(events: &mut EventVocabulary, summaries: &Summaries) -> bool {
    let mut changed = false;
    for (key, summary) in summaries {
        changed |= discover_events(events, key, summary);
    }
    changed
}

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
    let mut events = EventVocabulary::default();
    'vocabulary: loop {
        let (base, frozen_ports) = program.may(&keys, ports, None);
        keys = base.keys().cloned().collect();
        discover_summary_events(&mut events, &base);
        let (mut proofs, discovered, requests, new_events) =
            program.proofs(&base, &frozen_ports, &mut events, None);
        if new_events
            || discovered != frozen_ports
            || requests.iter().any(|key| !keys.contains(key))
        {
            keys.extend(requests);
            program.expand_keys(&mut keys, &discovered);
            ports = discovered;
            continue;
        }
        if !proofs.values().any(Proof::has_effect) {
            return (base, frozen_ports, proofs);
        }
        loop {
            let (refined, refined_ports) = program.may(&keys, frozen_ports.clone(), Some(&proofs));
            if discover_summary_events(&mut events, &refined)
                || refined_ports != frozen_ports
                || refined.keys().any(|key| !keys.contains(key))
            {
                keys.extend(refined.keys().cloned());
                program.expand_keys(&mut keys, &refined_ports);
                ports = refined_ports;
                continue 'vocabulary;
            }
            let (checked, discovered, requests, new_events) =
                program.proofs(&refined, &refined_ports, &mut events, Some(&proofs));
            if new_events
                || discovered != refined_ports
                || requests.iter().any(|key| !keys.contains(key))
            {
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
        events: &mut EventVocabulary,
        bounds: Option<&Proofs>,
    ) -> (Proofs, Ports, FxHashSet<Key>, bool) {
        let mut proofs: Proofs = summaries
            .iter()
            .map(|(key, summary)| {
                (
                    key.clone(),
                    bounds
                        .and_then(|bounds| bounds.get(key))
                        .cloned()
                        .unwrap_or_else(|| {
                            Proof::universe(
                                &self.mir.functions[&key.function],
                                self.table,
                                summary.normal_return,
                                events.get(key),
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
            let mut new_events = false;
            for (key, proof) in &mut next {
                if !proof.has_effect() {
                    continue;
                }
                let function = &self.mir.functions[&key.function];
                let mut flow = Flow::new(function, self.table, Some(context));
                flow.aliases = key.aliases.clone();
                flow.callables = key.callables.clone();
                flow.solve(&self.facts[&key.function]);
                let returns = flow.future_guarantee();
                let summary = flow.summary();
                changed |= proof.restrict(&returns, &flow.event_guards);
                new_events |= discover_events(events, key, &summary);
                flow.discover_ports(&summary, discovered.entry(key.function).or_default());
                requests.extend(flow.requests);
            }
            proofs = next;
            if new_events || !changed {
                return (proofs, discovered, requests, new_events);
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
