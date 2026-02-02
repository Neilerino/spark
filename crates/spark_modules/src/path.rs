//! Module path representation.
//!
//! A module path is a sequence of identifiers representing a module's location
//! in the module hierarchy (e.g., `user.auth` = `["user", "auth"]`).

use std::fmt;
use std::path::PathBuf;

use spark_intern::Symbol;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// A module path like `["user", "auth"]` representing `user.auth`.
///
/// Module paths are used to identify modules in the dependency graph and
/// correlate them with file system paths.
///
/// # Examples
///
/// ```ignore
/// use spark_modules::ModulePath;
/// use spark_intern::sym;
///
/// let path = ModulePath::new(vec![sym("user"), sym("auth")]);
/// assert_eq!(path.to_string(), "user.auth");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ModulePath(pub Vec<Symbol>);

impl ModulePath {
    /// Creates a new module path from a vector of symbols.
    #[must_use]
    pub const fn new(segments: Vec<Symbol>) -> Self {
        Self(segments)
    }

    /// Creates an empty (root) module path.
    #[must_use]
    pub const fn root() -> Self {
        Self(Vec::new())
    }

    /// Creates a module path from a single segment.
    #[must_use]
    pub fn from_single(name: Symbol) -> Self {
        Self(vec![name])
    }

    /// Returns true if this is the root/empty path.
    #[must_use]
    pub const fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns true if this path is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the number of segments in the path.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns the segments of the path.
    #[must_use]
    pub fn segments(&self) -> &[Symbol] {
        &self.0
    }

    /// Returns the parent path, or None if this is root or has only one segment.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        if self.0.len() <= 1 {
            None
        } else {
            Some(Self(self.0[..self.0.len() - 1].to_vec()))
        }
    }

    /// Returns the last segment (the module name), or None if empty.
    #[must_use]
    pub fn name(&self) -> Option<Symbol> {
        self.0.last().copied()
    }

    /// Returns the first segment, or None if empty.
    #[must_use]
    pub fn first(&self) -> Option<Symbol> {
        self.0.first().copied()
    }

    /// Creates a child path by appending a segment.
    #[must_use]
    pub fn child(&self, name: Symbol) -> Self {
        let mut segments = self.0.clone();
        segments.push(name);
        Self(segments)
    }

    /// Joins this path with another path.
    #[must_use]
    pub fn join(&self, other: &Self) -> Self {
        let mut segments = self.0.clone();
        segments.extend_from_slice(&other.0);
        Self(segments)
    }

    /// Converts to a relative file path.
    ///
    /// For example, `["user", "auth"]` becomes `user/auth`.
    #[must_use]
    pub fn to_relative_path(&self) -> PathBuf {
        let mut path = PathBuf::new();
        for seg in &self.0 {
            path.push(seg.as_str());
        }
        path
    }

    /// Returns true if `other` is a child of this path.
    ///
    /// A path is a child if it starts with this path and has additional segments.
    #[must_use]
    pub fn is_ancestor_of(&self, other: &Self) -> bool {
        other.0.len() > self.0.len() && other.0.starts_with(&self.0)
    }

    /// Returns true if `other` is a parent of this path.
    #[must_use]
    pub fn is_descendant_of(&self, other: &Self) -> bool {
        other.is_ancestor_of(self)
    }

    /// Returns true if this path starts with the given path.
    #[must_use]
    pub fn starts_with(&self, other: &Self) -> bool {
        self.0.starts_with(&other.0)
    }
}

impl Default for ModulePath {
    fn default() -> Self {
        Self::root()
    }
}

impl fmt::Display for ModulePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<_> = self.0.iter().map(|s| s.as_str()).collect();
        write!(f, "{}", parts.join("."))
    }
}

impl From<Vec<Symbol>> for ModulePath {
    fn from(segments: Vec<Symbol>) -> Self {
        Self(segments)
    }
}

impl From<Symbol> for ModulePath {
    fn from(symbol: Symbol) -> Self {
        Self::from_single(symbol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_intern::sym;

    #[test]
    fn root_path() {
        let path = ModulePath::root();
        assert!(path.is_root());
        assert!(path.is_empty());
        assert_eq!(path.len(), 0);
        assert!(path.name().is_none());
        assert!(path.parent().is_none());
    }

    #[test]
    fn single_segment() {
        let path = ModulePath::from_single(sym("user"));
        assert!(!path.is_root());
        assert_eq!(path.len(), 1);
        assert_eq!(path.name(), Some(sym("user")));
        assert!(path.parent().is_none());
    }

    #[test]
    fn multi_segment() {
        let path = ModulePath::new(vec![sym("user"), sym("auth")]);
        assert_eq!(path.len(), 2);
        assert_eq!(path.name(), Some(sym("auth")));
        assert_eq!(path.first(), Some(sym("user")));

        let parent = path.parent().unwrap();
        assert_eq!(parent.len(), 1);
        assert_eq!(parent.name(), Some(sym("user")));
    }

    #[test]
    fn child() {
        let path = ModulePath::from_single(sym("user"));
        let child = path.child(sym("auth"));
        assert_eq!(child.len(), 2);
        assert_eq!(child.to_string(), "user.auth");
    }

    #[test]
    fn join() {
        let a = ModulePath::from_single(sym("user"));
        let b = ModulePath::new(vec![sym("auth"), sym("token")]);
        let joined = a.join(&b);
        assert_eq!(joined.to_string(), "user.auth.token");
    }

    #[test]
    fn display() {
        let path = ModulePath::new(vec![sym("user"), sym("auth"), sym("token")]);
        assert_eq!(path.to_string(), "user.auth.token");

        let empty = ModulePath::root();
        assert_eq!(empty.to_string(), "");
    }

    #[test]
    fn to_relative_path() {
        let path = ModulePath::new(vec![sym("user"), sym("auth")]);
        let file_path = path.to_relative_path();
        assert_eq!(file_path, PathBuf::from("user/auth"));
    }

    #[test]
    fn ancestry() {
        let parent = ModulePath::from_single(sym("user"));
        let child = ModulePath::new(vec![sym("user"), sym("auth")]);
        let grandchild = ModulePath::new(vec![sym("user"), sym("auth"), sym("token")]);
        let unrelated = ModulePath::from_single(sym("utils"));

        assert!(parent.is_ancestor_of(&child));
        assert!(parent.is_ancestor_of(&grandchild));
        assert!(child.is_ancestor_of(&grandchild));
        assert!(!parent.is_ancestor_of(&parent)); // Not ancestor of self
        assert!(!parent.is_ancestor_of(&unrelated));

        assert!(child.is_descendant_of(&parent));
        assert!(grandchild.is_descendant_of(&parent));
        assert!(!parent.is_descendant_of(&child));
    }

    #[test]
    fn starts_with() {
        let path = ModulePath::new(vec![sym("user"), sym("auth")]);
        let prefix = ModulePath::from_single(sym("user"));

        assert!(path.starts_with(&prefix));
        assert!(path.starts_with(&path));
        assert!(!prefix.starts_with(&path));
    }

    #[test]
    fn from_impls() {
        let from_vec: ModulePath = vec![sym("a"), sym("b")].into();
        assert_eq!(from_vec.len(), 2);

        let from_symbol: ModulePath = sym("single").into();
        assert_eq!(from_symbol.len(), 1);
    }

    #[test]
    fn equality() {
        let a = ModulePath::new(vec![sym("user"), sym("auth")]);
        let b = ModulePath::new(vec![sym("user"), sym("auth")]);
        let c = ModulePath::new(vec![sym("user"), sym("other")]);

        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn hash() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(ModulePath::from_single(sym("user")));
        set.insert(ModulePath::new(vec![sym("user"), sym("auth")]));

        assert!(set.contains(&ModulePath::from_single(sym("user"))));
        assert!(set.contains(&ModulePath::new(vec![sym("user"), sym("auth")])));
        assert!(!set.contains(&ModulePath::from_single(sym("other"))));
    }
}
