//! Resolved AST with `DefId` mappings.
//!
//! This module provides types for storing resolution results: mappings
//! from AST node spans to their resolved `DefId`s.

use std::collections::HashMap;

use rustc_hash::FxHashMap;
use spark_ast::Ast;
use spark_span::{FileId, Span};

use crate::def::DefId;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Resolution map: maps AST node spans to their resolutions.
///
/// We use spans as keys because AST nodes don't have IDs. The span
/// uniquely identifies where in the source the reference occurred.
#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ResolutionMap {
    /// Identifier resolutions (for `ExprKind::Ident`).
    ident_resolutions: FxHashMap<Span, DefId>,
    /// Path resolutions (for `ExprKind::Path`, `TypeExpr::Named`).
    path_resolutions: FxHashMap<Span, DefId>,
    /// Pattern binding introductions (where new names are defined).
    pattern_bindings: FxHashMap<Span, DefId>,
}

impl ResolutionMap {
    /// Creates a new empty resolution map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an identifier resolution.
    pub fn record_ident(&mut self, span: Span, def_id: DefId) {
        self.ident_resolutions.insert(span, def_id);
    }

    /// Records a path resolution.
    pub fn record_path(&mut self, span: Span, def_id: DefId) {
        self.path_resolutions.insert(span, def_id);
    }

    /// Records a pattern binding (where a new name is introduced).
    pub fn record_binding(&mut self, span: Span, def_id: DefId) {
        self.pattern_bindings.insert(span, def_id);
    }

    /// Gets the resolution for an identifier.
    #[must_use]
    pub fn get_ident(&self, span: Span) -> Option<DefId> {
        self.ident_resolutions.get(&span).copied()
    }

    /// Gets the resolution for a path.
    #[must_use]
    pub fn get_path(&self, span: Span) -> Option<DefId> {
        self.path_resolutions.get(&span).copied()
    }

    /// Gets the `DefId` for a pattern binding.
    #[must_use]
    pub fn get_binding(&self, span: Span) -> Option<DefId> {
        self.pattern_bindings.get(&span).copied()
    }

    /// Finds the `DefId` for the most specific (smallest) resolved span
    /// that contains the given byte offset.
    ///
    /// Searches identifier resolutions, path resolutions, and pattern
    /// bindings. When multiple spans match, returns the narrowest one.
    #[must_use]
    pub fn find_at_offset(&self, offset: u32) -> Option<(Span, DefId)> {
        let candidates = self
            .ident_resolutions
            .iter()
            .chain(self.path_resolutions.iter())
            .chain(self.pattern_bindings.iter());

        candidates
            .filter(|(span, _)| span.contains(offset))
            .min_by_key(|(span, _)| span.end - span.start)
            .map(|(span, def_id)| (*span, *def_id))
    }

    /// Finds all reference spans that resolve to the given `DefId`.
    ///
    /// Searches identifier and path resolutions (usage sites). Pattern
    /// bindings are excluded because they are definition sites, not usages.
    #[must_use]
    pub fn find_references(&self, target: DefId) -> Vec<Span> {
        self.ident_resolutions
            .iter()
            .chain(self.path_resolutions.iter())
            .filter(|(_, &def_id)| def_id == target)
            .map(|(&span, _)| span)
            .collect()
    }

    /// Returns the total number of resolutions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ident_resolutions.len() + self.path_resolutions.len() + self.pattern_bindings.len()
    }

    /// Returns true if there are no resolutions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ident_resolutions.is_empty()
            && self.path_resolutions.is_empty()
            && self.pattern_bindings.is_empty()
    }
}

/// The resolved AST output.
///
/// Contains the original ASTs along with resolution mappings for each file.
#[derive(Debug, Default, Clone)]
pub struct ResolvedAst {
    /// The original ASTs (unchanged).
    pub asts: HashMap<FileId, Ast>,
    /// Resolution mappings per file.
    pub resolutions: FxHashMap<FileId, ResolutionMap>,
}

impl ResolvedAst {
    /// Creates a new resolved AST from the original ASTs.
    #[must_use]
    pub fn new(asts: HashMap<FileId, Ast>) -> Self {
        Self {
            asts,
            resolutions: FxHashMap::default(),
        }
    }

    /// Gets the resolution map for a file.
    #[must_use]
    pub fn get_resolutions(&self, file_id: FileId) -> Option<&ResolutionMap> {
        self.resolutions.get(&file_id)
    }

    /// Gets or creates a resolution map for a file.
    pub fn get_or_create_resolutions(&mut self, file_id: FileId) -> &mut ResolutionMap {
        self.resolutions.entry(file_id).or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_map_ident() {
        let mut map = ResolutionMap::new();
        let span = Span::new(FileId::new(0), 0, 5);
        let def_id = DefId::new(0);

        map.record_ident(span, def_id);

        assert_eq!(map.get_ident(span), Some(def_id));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn resolution_map_path() {
        let mut map = ResolutionMap::new();
        let span = Span::new(FileId::new(0), 10, 20);
        let def_id = DefId::new(1);

        map.record_path(span, def_id);

        assert_eq!(map.get_path(span), Some(def_id));
    }

    #[test]
    fn resolution_map_binding() {
        let mut map = ResolutionMap::new();
        let span = Span::new(FileId::new(0), 5, 10);
        let def_id = DefId::new(2);

        map.record_binding(span, def_id);

        assert_eq!(map.get_binding(span), Some(def_id));
    }

    #[test]
    fn find_at_offset_ident() {
        let mut map = ResolutionMap::new();
        let span = Span::new(FileId::new(0), 10, 15);
        let def_id = DefId::new(0);
        map.record_ident(span, def_id);

        // Inside the span
        let result = map.find_at_offset(12);
        assert_eq!(result, Some((span, def_id)));

        // Outside the span
        assert!(map.find_at_offset(20).is_none());
    }

    #[test]
    fn find_at_offset_picks_narrowest() {
        let mut map = ResolutionMap::new();
        let outer = Span::new(FileId::new(0), 0, 20);
        let inner = Span::new(FileId::new(0), 5, 10);
        let outer_def = DefId::new(0);
        let inner_def = DefId::new(1);
        map.record_path(outer, outer_def);
        map.record_ident(inner, inner_def);

        // Offset 7 is inside both — should pick inner (narrower)
        let result = map.find_at_offset(7);
        assert_eq!(result, Some((inner, inner_def)));

        // Offset 15 is only inside outer
        let result = map.find_at_offset(15);
        assert_eq!(result, Some((outer, outer_def)));
    }

    #[test]
    fn find_references_from_ident_and_path() {
        let mut map = ResolutionMap::new();
        let file = FileId::new(0);
        let target = DefId::new(1);
        let other = DefId::new(2);

        let span_a = Span::new(file, 10, 15);
        let span_b = Span::new(file, 30, 35);
        let span_c = Span::new(file, 50, 55);

        map.record_ident(span_a, target);
        map.record_path(span_b, target);
        map.record_ident(span_c, other);

        let mut refs = map.find_references(target);
        refs.sort_by_key(|s| s.start);
        assert_eq!(refs, vec![span_a, span_b]);
    }

    #[test]
    fn find_references_excludes_pattern_bindings() {
        let mut map = ResolutionMap::new();
        let file = FileId::new(0);
        let target = DefId::new(1);

        let usage = Span::new(file, 20, 25);
        let binding = Span::new(file, 5, 10);

        map.record_ident(usage, target);
        map.record_binding(binding, target);

        let refs = map.find_references(target);
        assert_eq!(refs, vec![usage]);
    }

    #[test]
    fn find_references_unknown_def_returns_empty() {
        let mut map = ResolutionMap::new();
        let file = FileId::new(0);
        map.record_ident(Span::new(file, 0, 5), DefId::new(1));

        let refs = map.find_references(DefId::new(99));
        assert!(refs.is_empty());
    }

    #[test]
    fn resolved_ast_get_or_create() {
        let mut resolved = ResolvedAst::new(HashMap::new());
        let file_id = FileId::new(0);

        let map = resolved.get_or_create_resolutions(file_id);
        map.record_ident(Span::DUMMY, DefId::new(0));

        assert!(resolved.get_resolutions(file_id).is_some());
    }
}
