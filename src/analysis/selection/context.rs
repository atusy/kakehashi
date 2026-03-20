//! Context structs for SelectionRange building.
//!
//! This module provides context structs that bundle related parameters together,
//! reducing the number of arguments needed for injection-aware selection building.
//!
//! ## Design Rationale
//!
//! The selection building functions previously had 8-11 parameters, which indicated
//! missing abstractions. By grouping related parameters into context structs, we:
//!
//! 1. **Reduce cognitive load**: Function signatures become clearer
//! 2. **Make dependencies explicit**: Each context has a clear responsibility
//! 3. **Enable easier testing**: Contexts can be mocked or stubbed
//! 4. **Improve code reuse**: Contexts can be passed through call chains

use crate::language::injection::MAX_INJECTION_DEPTH;
use crate::language::{DocumentParserPool, LanguageCoordinator};
use crate::text::PositionMapper;
use tree_sitter::Node;

/// Document-level context for SelectionRange building.
///
/// Bundles the host document's text, position mapper, and AST root node.
/// These values remain constant throughout the selection building process,
/// even when recursively processing nested injections.
///
/// # Lifetime
///
/// The `'a` lifetime ties this context to the document's text and AST.
/// The context should not outlive the document it references.
#[derive(Clone, Copy)]
pub struct DocumentContext<'a> {
    /// The full host document text
    pub text: &'a str,
    /// Position mapper for byte-to-UTF16 conversion
    pub mapper: &'a PositionMapper,
    /// Root node of the host document's AST
    pub root: Node<'a>,
    /// Base language identifier of the host document
    pub base_language: &'a str,
}

impl<'a> DocumentContext<'a> {
    /// Create a new DocumentContext.
    ///
    /// # Arguments
    ///
    /// * `text` - The full host document text
    /// * `mapper` - Position mapper for byte-to-UTF16 conversion
    /// * `root` - Root node of the host document's AST
    /// * `base_language` - Language identifier (e.g., "rust", "markdown")
    pub fn new(
        text: &'a str,
        mapper: &'a PositionMapper,
        root: Node<'a>,
        base_language: &'a str,
    ) -> Self {
        Self {
            text,
            mapper,
            root,
            base_language,
        }
    }
}

/// Injection-aware context for managing language resources and recursion depth.
///
/// This struct bundles:
/// - Language coordinator for loading parsers and injection queries
/// - Parser pool for acquiring/releasing parsers efficiently
/// - Current recursion depth for nested injections
///
/// Unlike `DocumentContext`, this context is mutable because:
/// - Parser pool state changes on acquire/release
/// - Depth increments on each recursion level
pub struct InjectionContext<'a> {
    /// Language coordinator for getting parsers and injection queries
    pub coordinator: &'a LanguageCoordinator,
    /// Parser pool for efficient parser reuse
    pub parser_pool: &'a mut DocumentParserPool,
    /// Current recursion depth (0 = host document, 1+ = nested injection)
    depth: usize,
}

impl<'a> InjectionContext<'a> {
    /// Create a new InjectionContext at depth 0.
    ///
    /// # Arguments
    ///
    /// * `coordinator` - Language coordinator for language services
    /// * `parser_pool` - Parser pool for parser acquisition
    pub fn new(
        coordinator: &'a LanguageCoordinator,
        parser_pool: &'a mut DocumentParserPool,
    ) -> Self {
        Self {
            coordinator,
            parser_pool,
            depth: 0,
        }
    }

    /// Check if we can descend into another injection level.
    ///
    /// Returns `true` if the current depth is less than `MAX_INJECTION_DEPTH`.
    pub fn can_descend(&self) -> bool {
        self.depth < MAX_INJECTION_DEPTH
    }

    /// Increment depth and return the new depth, or None if at max.
    ///
    /// Unlike `descend()`, this mutates in place for use in loops.
    pub fn increment_depth(&mut self) -> Option<usize> {
        if self.depth >= MAX_INJECTION_DEPTH {
            return None;
        }
        self.depth += 1;
        Some(self.depth)
    }

    /// Ensure a language is loaded and return whether it succeeded.
    pub fn ensure_language_loaded(&self, language: &str) -> bool {
        self.coordinator.ensure_language_loaded(language).success
    }

    /// Get the injection query for a language, if available.
    pub fn injection_query(&self, language: &str) -> Option<std::sync::Arc<tree_sitter::Query>> {
        self.coordinator.injection_query(language)
    }

    /// Acquire a parser for the given language.
    ///
    /// Returns `None` if the language is not loaded or no parser is available.
    pub fn acquire_parser(&mut self, language: &str) -> Option<tree_sitter::Parser> {
        self.parser_pool.acquire(language)
    }

    /// Release a parser back to the pool.
    pub fn release_parser(&mut self, language: String, parser: tree_sitter::Parser) {
        self.parser_pool.release(language, parser);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_injection_context_can_descend_initially() {
        let coordinator = LanguageCoordinator::new();
        let mut parser_pool = coordinator.create_document_parser_pool();
        let ctx = InjectionContext::new(&coordinator, &mut parser_pool);

        assert!(ctx.can_descend());
    }

    #[test]
    fn test_injection_context_increment_depth() {
        let coordinator = LanguageCoordinator::new();
        let mut parser_pool = coordinator.create_document_parser_pool();
        let mut ctx = InjectionContext::new(&coordinator, &mut parser_pool);

        let new_depth = ctx.increment_depth();
        assert_eq!(new_depth, Some(1));

        let new_depth = ctx.increment_depth();
        assert_eq!(new_depth, Some(2));
    }

    #[test]
    fn test_injection_context_respects_max_depth() {
        let coordinator = LanguageCoordinator::new();
        let mut parser_pool = coordinator.create_document_parser_pool();
        let mut ctx = InjectionContext::new(&coordinator, &mut parser_pool);

        // Increment to max depth
        for _ in 0..MAX_INJECTION_DEPTH {
            assert!(ctx.increment_depth().is_some());
        }

        // Should not be able to increment further
        assert!(ctx.increment_depth().is_none());
        assert!(!ctx.can_descend());
    }
}
