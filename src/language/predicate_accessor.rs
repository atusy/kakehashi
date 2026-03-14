use tree_sitter::{Query, QueryPredicate, QueryProperty};

/// Get all predicates for a pattern, including both general predicates and property settings
pub(crate) fn get_all_predicates(query: &Query, pattern_index: usize) -> PredicateIterator<'_> {
    PredicateIterator {
        general_predicates: query.general_predicates(pattern_index),
        property_settings: query.property_settings(pattern_index),
        general_index: 0,
        property_index: 0,
    }
}

/// Iterator over all predicates (both general and property-based)
pub(crate) struct PredicateIterator<'a> {
    general_predicates: &'a [QueryPredicate],
    property_settings: &'a [QueryProperty],
    general_index: usize,
    property_index: usize,
}

/// Unified predicate type that can represent both general predicates and property settings
#[derive(Debug, Clone)]
pub(crate) enum UnifiedPredicate<'a> {
    General(&'a QueryPredicate),
    Property(&'a QueryProperty),
}

impl<'a> UnifiedPredicate<'a> {
    /// Get the operator/key of the predicate
    pub(crate) fn operator(&self) -> &str {
        match self {
            UnifiedPredicate::General(p) => p.operator.as_ref(),
            UnifiedPredicate::Property(p) => p.key.as_ref(),
        }
    }
}

impl<'a> Iterator for PredicateIterator<'a> {
    type Item = UnifiedPredicate<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        // First yield all general predicates
        if self.general_index < self.general_predicates.len() {
            let predicate = &self.general_predicates[self.general_index];
            self.general_index += 1;
            return Some(UnifiedPredicate::General(predicate));
        }

        // Then yield all property settings
        if self.property_index < self.property_settings.len() {
            let property = &self.property_settings[self.property_index];
            self.property_index += 1;
            return Some(UnifiedPredicate::Property(property));
        }

        None
    }
}

// Note: This module is tested through integration tests in injection.rs
// where actual Query objects with predicates are created.
