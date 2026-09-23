use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::{EditInfo, NodeTracker, PositionKey, adjust_position_for_edit};

/// Parse inputs that distinguish injected trees sharing a nesting depth.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct NodeTreeScope {
    pub(crate) language: Arc<str>,
    pub(crate) depth: usize,
    pub(crate) ranges: Vec<(usize, usize)>,
}

impl NodeTreeScope {
    pub(crate) fn new(language: &str, depth: usize, tree: &tree_sitter::Tree) -> Self {
        Self {
            language: language.into(),
            depth,
            ranges: tree
                .included_ranges()
                .iter()
                .map(|r| (r.start_byte, r.end_byte))
                .collect(),
        }
    }

    fn shifted(&self, edit: &EditInfo) -> Option<Self> {
        let ranges = self
            .ranges
            .iter()
            .map(|&(start, end)| {
                let key = PositionKey::new(start, end, "", 0);
                if NodeTracker::should_invalidate_node(&key, edit) {
                    return None;
                }
                let shifted = adjust_position_for_edit(key, edit, edit.delta())?;
                Some((shifted.start_byte, shifted.end_byte))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            ranges,
            language: Arc::clone(&self.language),
            depth: self.depth,
        })
    }
}

/// Tokens are never reused within a URI entry: navigation may retain one
/// across an await after its scope has been retired.
#[derive(Default)]
pub(super) struct TreeScopes {
    by_scope: HashMap<Arc<NodeTreeScope>, usize>,
    by_token: HashMap<usize, Arc<NodeTreeScope>>,
    next: usize,
}

pub(super) const TREE_SCOPE_BASE: usize = crate::language::injection::MAX_INJECTION_DEPTH + 1;

impl TreeScopes {
    pub(super) fn get(&self, scope: &NodeTreeScope) -> Option<usize> {
        self.by_scope.get(scope).copied()
    }

    pub(super) fn contains(&self, token: usize) -> bool {
        self.by_token.contains_key(&token)
    }

    pub(super) fn scope(&self, token: usize) -> Option<Arc<NodeTreeScope>> {
        self.by_token.get(&token).cloned()
    }

    pub(super) fn register(&mut self, scope: &NodeTreeScope) -> Option<usize> {
        if let Some(token) = self.get(scope) {
            return Some(token);
        }
        let token = TREE_SCOPE_BASE.checked_add(self.next)?;
        // Bridge injection alternatives own the upper half of the token space.
        if token >= crate::language::injection::REGION_IDENTITY_LAYER_BASE {
            return None;
        }
        self.next += 1;
        let scope = Arc::new(scope.clone());
        self.by_scope.insert(Arc::clone(&scope), token);
        self.by_token.insert(token, scope);
        Some(token)
    }

    pub(super) fn shift(&mut self, edit: &EditInfo) {
        self.by_token.retain(|_, scope| {
            if let Some(shifted) = scope.shifted(edit) {
                *scope = Arc::new(shifted);
                true
            } else {
                false
            }
        });
        self.reindex();
    }

    pub(super) fn retain(&mut self, live: &HashSet<usize>) {
        self.by_token.retain(|token, _| live.contains(token));
        self.reindex();
    }

    fn reindex(&mut self) {
        self.by_scope.clear();
        for (&token, scope) in &self.by_token {
            // Edits can make formerly distinct scopes equal. Keep both old
            // tokens resolvable; future mints use a canonical surviving token.
            self.by_scope
                .entry(Arc::clone(scope))
                .and_modify(|old| *old = (*old).min(token))
                .or_insert(token);
        }
    }
}
