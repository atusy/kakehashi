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

/// Geometry reached by discovery whose grammar or parse was unavailable.
/// Only this branch can hide additional current scopes.
#[derive(Clone, Debug)]
pub(crate) struct UnresolvedTreeScope {
    pub(crate) language: Option<Arc<str>>,
    pub(crate) depth: usize,
    pub(crate) ranges: Vec<(usize, usize)>,
}

impl UnresolvedTreeScope {
    fn may_contain(&self, scope: &NodeTreeScope) -> bool {
        if scope.depth < self.depth {
            return false;
        }
        if scope.depth == self.depth {
            return self.ranges == scope.ranges
                && self
                    .language
                    .as_ref()
                    .is_none_or(|language| *language == scope.language);
        }
        // Descendants inherit every exclusion from their parents. Check the
        // full range union, not a bounding span that fills excluded gaps.
        scope.ranges.iter().all(|&(start, end)| {
            let mut covered = start;
            for &(left, right) in &self.ranges {
                if left > covered {
                    break;
                }
                covered = covered.max(right);
                if covered >= end {
                    return true;
                }
            }
            false
        })
    }
}

/// Tokens are never reused within a document incarnation: navigation may retain one
/// across an await after its scope has been retired.
#[derive(Default)]
pub(super) struct TreeScopes {
    by_scope: HashMap<Arc<NodeTreeScope>, usize>,
    by_token: HashMap<usize, Arc<NodeTreeScope>>,
    next: usize,
    /// New scopes can coexist with obsolete boundary geometries until a
    /// complete, current walk is admitted. Edits/reloads cannot erase this debt.
    pub(super) reconciliation_pending: bool,
    pub(super) reconciled_query_generation: Option<u64>,
}

pub(super) const TREE_SCOPE_BASE: usize = crate::language::injection::MAX_INJECTION_DEPTH + 1;

impl TreeScopes {
    pub(super) fn is_empty(&self) -> bool {
        self.by_token.is_empty()
    }

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
        // A pre-reload compute may register after a newer empty walk was
        // admitted. Even the first new token must leave reconciliation debt.
        self.reconciliation_pending = true;
        self.next += 1;
        let scope = Arc::new(scope.clone());
        self.by_scope.insert(Arc::clone(&scope), token);
        self.by_token.insert(token, scope);
        Some(token)
    }

    pub(super) fn shift(&mut self, edit: &EditInfo) {
        // Query predicates can remove a tree even when its content ranges do
        // not move (for example, an edit to the enclosing function name).
        self.reconciliation_pending |= !self.by_token.is_empty();
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

    pub(super) fn retire_absent(
        &mut self,
        current: &HashSet<NodeTreeScope>,
        unresolved: &[UnresolvedTreeScope],
    ) -> HashSet<usize> {
        self.reconciliation_pending = false;
        let mut retired = HashSet::new();
        self.by_token.retain(|token, scope| {
            let known = current.contains(scope.as_ref());
            let uncertain = !known && unresolved.iter().any(|branch| branch.may_contain(scope));
            self.reconciliation_pending |= uncertain;
            let keep = known || uncertain;
            if !keep {
                retired.insert(*token);
            }
            keep
        });
        if !retired.is_empty() {
            self.reindex();
        }
        retired
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_branches_preserve_only_possible_scopes_and_keep_retry_debt() {
        let branch = UnresolvedTreeScope {
            language: Some("rust".into()),
            depth: 1,
            ranges: vec![(10, 20), (30, 40)],
        };
        let scope = |language: &str, depth, ranges: &[(usize, usize)]| NodeTreeScope {
            language: language.into(),
            depth,
            ranges: ranges.to_vec(),
        };
        let root = scope("rust", 1, &[(10, 20), (30, 40)]);
        let descendant = scope("python", 2, &[(12, 18), (32, 38)]);
        assert!(branch.may_contain(&root));
        assert!(branch.may_contain(&descendant));
        for absent in [
            scope("go", 1, &[(10, 20), (30, 40)]),
            scope("rust", 1, &[(10, 19), (30, 40)]),
            scope("rust", 2, &[(12, 38)]),
            scope("rust", 2, &[(50, 60)]),
            scope("rust", 0, &[(12, 18)]),
        ] {
            assert!(!branch.may_contain(&absent));
        }
        let mut scopes = TreeScopes::default();
        let protected = scopes.register(&descendant).unwrap();
        let obsolete = scopes.register(&scope("rust", 1, &[(0, 8)])).unwrap();
        let retired = scopes.retire_absent(&HashSet::new(), &[branch]);
        assert_eq!(retired, HashSet::from([obsolete]));
        assert!(scopes.contains(protected));
        assert!(
            scopes.reconciliation_pending,
            "an unresolved retained scope must be revisited after grammar recovery"
        );
        assert!(
            scopes
                .retire_absent(&HashSet::from([descendant]), &[])
                .is_empty()
        );
        assert!(!scopes.reconciliation_pending);
    }
}
