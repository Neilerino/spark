// TAG [SPARK-114]

//! Callback progress is a finite table, separate from flat memory and history payloads.

use rustc_hash::{FxHashMap, FxHashSet};
use spark_types::TypeId;

use super::{Carrier, Subject, View, Views};
use crate::provenance::{address::OpaqueSite, Path, Value};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::provenance) struct NodeKey {
    pub subject: Subject,
    pub ty: TypeId,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Progress {
    callbacks: FxHashSet<TypeId>,
    fallback: bool,
    history: crate::provenance::observations::History,
    memory: crate::provenance::tasks::memory::Effects,
}

type FlatViews = FxHashMap<Path, FxHashMap<Subject, Progress>>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Node {
    carrier: Carrier,
    progress: FlatViews,
    sources: crate::provenance::demand::Sources,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::provenance) struct Callback {
    root: Node,
    nodes: FxHashMap<NodeKey, Node>,
}

impl Callback {
    pub fn seal(mut value: Value) -> Self {
        let mut result = Self::default();
        for (path, subjects) in std::mem::take(&mut value.task_views) {
            for (subject, view) in subjects {
                let mut progress = Progress {
                    callbacks: FxHashSet::default(),
                    fallback: view.fallback,
                    history: view.history,
                    memory: view.memory,
                };
                for (ty, callback) in view.callbacks {
                    progress.callbacks.insert(ty);
                    result.insert_node(
                        NodeKey {
                            subject: subject.clone(),
                            ty,
                        },
                        &callback,
                    );
                }
                result
                    .root
                    .progress
                    .entry(path.clone())
                    .or_default()
                    .entry(subject)
                    .or_default()
                    .join(&progress);
            }
        }
        result.root.sources = std::mem::take(&mut value.sources);
        result.root.carrier = Carrier::seal(value);
        result
    }

    pub fn value(&self) -> Value {
        self.expand(&self.root, false)
    }

    pub fn node_value(&self, key: Option<&NodeKey>) -> Option<Value> {
        let node = match key {
            None => &self.root,
            Some(key) => self.nodes.get(key)?,
        };
        Some(self.expand(node, false))
    }

    pub fn replace_node(&mut self, key: Option<&NodeKey>, value: Value) -> bool {
        if key.is_some_and(|key| !self.nodes.contains_key(key)) {
            return false;
        }
        let replacement = Self::seal(value);
        let before = self.clone();
        match key {
            None => *self = replacement,
            Some(key) => {
                for (key, node) in replacement.nodes {
                    self.nodes.entry(key).or_default().join(&node);
                }
                self.nodes.insert(key.clone(), replacement.root);
                self.prune();
            }
        }
        *self != before
    }

    pub fn shallow_value(&self) -> Value {
        self.expand(&self.root, true)
    }

    pub fn nodes(&self) -> impl Iterator<Item = (&NodeKey, Value)> {
        self.nodes
            .iter()
            .map(|(key, node)| (key, self.expand(node, true)))
    }

    pub fn source_values(&self) -> impl Iterator<Item = (Option<&NodeKey>, Value)> {
        std::iter::once((None, self.root.data_value())).chain(
            self.nodes
                .iter()
                .map(|(key, node)| (Some(key), node.data_value())),
        )
    }

    pub fn prune(&mut self) {
        self.nodes = self.reachable_nodes(&self.root.progress);
    }

    pub fn visit_progress(
        &mut self,
        mut visit: impl FnMut(
            &mut crate::provenance::observations::History,
            &mut crate::provenance::tasks::memory::Effects,
        ),
    ) {
        for node in std::iter::once(&mut self.root).chain(self.nodes.values_mut()) {
            for progress in node
                .progress
                .values_mut()
                .flat_map(|subjects| subjects.values_mut())
            {
                visit(&mut progress.history, &mut progress.memory);
            }
        }
    }

    pub fn visit_sources(
        &mut self,
        mut visit: impl FnMut(&mut crate::provenance::demand::Sources),
    ) {
        visit(&mut self.root.sources);
        for node in self.nodes.values_mut() {
            visit(&mut node.sources);
        }
    }

    pub fn source_at(&mut self, location: &crate::provenance::tasks::activity::Location) {
        self.root.sources = self
            .root
            .carrier
            .value()
            .sourced(&crate::provenance::demand::Storage::CallbackNode(Box::new(
                location.clone(),
            )))
            .sources;
        for (key, node) in &mut self.nodes {
            node.sources = node
                .carrier
                .value()
                .sourced(&crate::provenance::demand::Storage::CallbackNode(Box::new(
                    location.for_node(key.clone()),
                )))
                .sources;
        }
    }

    pub fn insert_node(&mut self, key: NodeKey, callback: &Self) -> bool {
        let mut changed = false;
        for (key, node) in &callback.nodes {
            let absent = !self.nodes.contains_key(key);
            changed |= self.nodes.entry(key.clone()).or_default().join(node) || absent;
        }
        let absent = !self.nodes.contains_key(&key);
        changed | self.nodes.entry(key).or_default().join(&callback.root) | absent
    }

    pub fn join(&mut self, other: &Self) -> bool {
        let mut changed = self.root.join(&other.root);
        for (key, node) in &other.nodes {
            let absent = !self.nodes.contains_key(key);
            changed |= self.nodes.entry(key.clone()).or_default().join(node) || absent;
        }
        changed
    }

    pub fn retire_frames(&mut self, boundary: OpaqueSite) {
        self.root.retire_frames(boundary);
        for node in self.nodes.values_mut() {
            node.retire_frames(boundary);
        }
    }

    pub fn remap_queues(
        &mut self,
        bindings: &FxHashMap<
            crate::provenance::channels::Target,
            crate::provenance::channels::Target,
        >,
    ) {
        self.root.carrier.remap_queues(bindings);
        for node in self.nodes.values_mut() {
            node.carrier.remap_queues(bindings);
        }
    }

    fn expand(&self, node: &Node, shallow: bool) -> Value {
        let mut value = node.data_value();
        value.task_views = node
            .progress
            .iter()
            .map(|(path, subjects)| {
                let subjects = subjects
                    .iter()
                    .map(|(subject, progress)| {
                        let mut missing = false;
                        let callbacks = progress
                            .callbacks
                            .iter()
                            .filter_map(|ty| {
                                let Some(node) = self.nodes.get(&NodeKey {
                                    subject: subject.clone(),
                                    ty: *ty,
                                }) else {
                                    missing = true;
                                    return None;
                                };
                                let callback = if shallow {
                                    Self {
                                        root: Node {
                                            carrier: node.carrier.clone(),
                                            progress: FlatViews::default(),
                                            sources: node.sources.clone(),
                                        },
                                        nodes: FxHashMap::default(),
                                    }
                                } else {
                                    Self {
                                        root: node.clone(),
                                        nodes: self.reachable_nodes(&node.progress),
                                    }
                                };
                                Some((*ty, callback))
                            })
                            .collect();
                        (
                            subject.clone(),
                            View {
                                callbacks,
                                fallback: progress.fallback || missing,
                                history: progress.history.clone(),
                                memory: progress.memory.clone(),
                            },
                        )
                    })
                    .collect();
                (path.clone(), subjects)
            })
            .collect::<Views>();
        value
    }

    fn reachable_nodes(&self, progress: &FlatViews) -> FxHashMap<NodeKey, Node> {
        let mut result = FxHashMap::default();
        let mut pending = node_keys(progress);
        while let Some(key) = pending.pop() {
            if result.contains_key(&key) {
                continue;
            }
            if let Some(node) = self.nodes.get(&key) {
                pending.extend(node_keys(&node.progress));
                result.insert(key, node.clone());
            }
        }
        result
    }
}

fn node_keys(progress: &FlatViews) -> Vec<NodeKey> {
    progress
        .values()
        .flat_map(|subjects| subjects.iter())
        .flat_map(|(subject, progress)| {
            progress.callbacks.iter().map(move |ty| NodeKey {
                subject: subject.clone(),
                ty: *ty,
            })
        })
        .collect()
}

impl Progress {
    fn join(&mut self, other: &Self) -> bool {
        let before = self.callbacks.len();
        self.callbacks.extend(other.callbacks.iter().copied());
        let mut changed = self.callbacks.len() != before || other.fallback && !self.fallback;
        self.fallback |= other.fallback;
        changed |= self.history.join(&other.history);
        changed |= self.memory.join(&other.memory);
        changed
    }
}

impl Node {
    fn data_value(&self) -> Value {
        let mut value = self.carrier.value();
        value.sources.clone_from(&self.sources);
        value
    }

    fn join(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (key, incoming) in &other.sources {
            let current = self.sources.entry(key.clone()).or_default();
            let before = current.len();
            current.extend(incoming.iter().cloned());
            changed |= current.len() != before;
        }
        for (path, subjects) in &other.progress {
            let current = self.progress.entry(path.clone()).or_default();
            for (subject, progress) in subjects {
                let absent = !current.contains_key(subject);
                let entry = current.entry(subject.clone()).or_default();
                changed |= entry.join(progress) || absent;
                if absent && has_pending(&self.carrier, path, subject) {
                    entry.fallback = true;
                }
            }
        }
        for (path, subjects) in &mut self.progress {
            for (subject, progress) in subjects {
                if has_pending(&other.carrier, path, subject)
                    && !other
                        .progress
                        .get(path)
                        .is_some_and(|views| views.contains_key(subject))
                {
                    changed |= !progress.fallback;
                    progress.fallback = true;
                }
            }
        }
        changed |= self.carrier.join(&other.carrier);
        changed
    }

    fn retire_frames(&mut self, boundary: OpaqueSite) {
        self.carrier.retire_frames(boundary);
        for progress in self
            .progress
            .values_mut()
            .flat_map(|views| views.values_mut())
        {
            progress.history.retire_frames(boundary);
            progress.memory.retire_frames(boundary);
        }
    }
}

fn has_pending(carrier: &Carrier, path: &Path, subject: &Subject) -> bool {
    carrier
        .futures
        .get(path)
        .into_iter()
        .flatten()
        .any(|handle| handle.pending && Subject::from(handle) == *subject)
}
