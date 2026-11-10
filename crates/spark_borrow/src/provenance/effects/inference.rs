// TAG [SPARK-114]

use super::{
    Context, DefId, Diagnostics, Flow, FxHashMap, FxHashSet, Key, Mir, Ports, Summaries, Summary,
    TypeTable,
};

pub fn analyze_program(
    mir: &Mir,
    table: &TypeTable,
    diagnostics: &mut Diagnostics,
) -> FxHashMap<DefId, Vec<Vec<crate::regions::Region>>> {
    let facts: FxHashMap<_, _> = mir
        .functions
        .iter()
        .map(|(def, function)| (*def, crate::facts::extract_facts(function)))
        .collect();
    let (summaries, ports) = infer_summaries(mir, table, &facts);
    let context = Context {
        mir,
        summaries: &summaries,
        ports: &ports,
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
        regions
            .entry(key.function)
            .or_default()
            .push(flow.carrier_regions(&facts[&key.function]));
    }
    regions
}

pub(super) fn infer_summaries(
    mir: &Mir,
    table: &TypeTable,
    facts: &FxHashMap<DefId, crate::facts::BorrowFacts>,
) -> (Summaries, Ports) {
    let mut summaries: Summaries = mir
        .functions
        .keys()
        .map(|def| (Key::distinct(*def), Summary::default()))
        .collect();
    let mut ports: Ports = mir.functions.keys().map(|def| (*def, Vec::new())).collect();
    loop {
        let context = Context {
            mir,
            summaries: &summaries,
            ports: &ports,
        };
        let mut next = summaries.clone();
        let mut discovered = ports.clone();
        let mut requests = FxHashSet::default();
        let mut changed = false;
        for key in summaries.keys() {
            let function = &mir.functions[&key.function];
            let mut flow = Flow::new(function, table, Some(context));
            flow.aliases = key.aliases.clone();
            flow.callables = key.callables.clone();
            flow.solve(&facts[&key.function]);
            let summary = flow.summary();
            flow.discover_ports(&summary, discovered.entry(key.function).or_default());
            changed |= next.entry(key.clone()).or_default().join(&summary);
            requests.extend(flow.requests);
        }
        if discovered != ports {
            for key in next.keys() {
                for aliases in key.aliases.body_variants(
                    &mir.functions[&key.function],
                    &discovered[&key.function],
                    table,
                ) {
                    requests.insert(Key {
                        aliases,
                        ..key.clone()
                    });
                }
            }
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
