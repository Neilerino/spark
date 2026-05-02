//! Reusable integer handle table for opaque runtime resources.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

pub(crate) struct HandleTable<T> {
    next: AtomicI64,
    resources: Mutex<HashMap<i64, T>>,
}

impl<T> HandleTable<T> {
    pub(crate) fn new() -> Self {
        Self {
            next: AtomicI64::new(1),
            resources: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn insert(&self, resource: T) -> i64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut resources = self
            .resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        resources.insert(id, resource);
        id
    }

    pub(crate) fn remove(&self, id: i64) -> Option<T> {
        let mut resources = self
            .resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        resources.remove(&id)
    }

    pub(crate) fn with<R>(&self, id: i64, f: impl FnOnce(&T) -> R) -> Option<R> {
        let resources = self
            .resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        resources.get(&id).map(f)
    }
}
