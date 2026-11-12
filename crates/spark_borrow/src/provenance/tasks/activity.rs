// TAG [SPARK-114]

use spark_types::TypeId;

use super::views::{NodeKey, Subject};
use crate::provenance::{demand::Storage, Path};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::provenance) struct Location {
    root: Box<Storage>,
    path: Path,
    actor: Subject,
    callback_ty: TypeId,
    node: Option<NodeKey>,
}

impl Location {
    pub fn new(storage: &Storage, path: Path, actor: Subject, ty: TypeId) -> Self {
        if let Storage::CallbackNode(parent) = storage {
            return Self {
                node: Some(NodeKey { subject: actor, ty }),
                ..(**parent).clone()
            };
        }
        Self {
            root: Box::new(storage.clone()),
            path,
            actor,
            callback_ty: ty,
            node: None,
        }
    }

    pub fn root(&self) -> &Storage {
        &self.root
    }
    pub fn path(&self) -> &[crate::provenance::Slot] {
        &self.path
    }
    pub const fn actor(&self) -> &Subject {
        &self.actor
    }
    pub const fn callback_type(&self) -> TypeId {
        self.callback_ty
    }
    pub const fn node(&self) -> Option<&NodeKey> {
        self.node.as_ref()
    }
    pub fn ty(&self) -> TypeId {
        self.node.as_ref().map_or(self.callback_ty, |key| key.ty)
    }

    pub fn for_node(&self, key: NodeKey) -> Self {
        Self {
            node: Some(key),
            ..self.clone()
        }
    }

    pub fn is_below(&self, storage: &Storage, prefix: &[crate::provenance::Slot]) -> bool {
        if prefix.contains(&crate::provenance::Slot::Tail) {
            return false;
        }
        if storage == self.root() {
            return self.path.starts_with(prefix);
        }
        matches!(storage, Storage::CallbackNode(parent) if parent.root == self.root && parent.path == self.path && parent.actor == self.actor && parent.callback_ty == self.callback_ty && parent.node.is_none() && prefix.is_empty())
    }
}

#[cfg(test)]
mod tests;
