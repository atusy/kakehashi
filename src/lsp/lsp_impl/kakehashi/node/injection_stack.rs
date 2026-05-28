//! Compute the injection layer stack at a byte offset (ADR-0025 PR-4 helper).
//!
//! The host language tree is layer 0; each enclosing `@injection.content`
//! that contains the cursor adds a deeper layer. Layers are returned in
//! outermost-to-innermost order so callers can index by ADR-0025's formulas:
//! `stack[n]` for positive `n` and `stack[stack.len() + n]` for negative `n`.
//!
//! Trees within each layer are parsed against the **full host text** with
//! tree-sitter's `set_included_ranges`, which means every node's
//! `start_byte` / `end_byte` is already in original-document coordinates.
//! That property is load-bearing: the entry-point handler issues ULIDs via
//! `NodeTracker::get_or_create(uri, start_byte, end_byte, kind)`, and the
//! tracker keys must stay in the host's byte space so subsequent
//! `parent` / `children` / `text` calls and `didChange` adjustments line
//! up across layers.

use crate::language::LanguageCoordinator;
use crate::language::injection::{
    MAX_INJECTION_DEPTH, collect_all_injections, compute_included_ranges,
};

/// One layer in the injection stack at a position.
///
/// `tree` is owned so the caller can keep using it after this helper returns,
/// and so the host layer can carry the document tree without cloning lifetime
/// dependencies. Byte coordinates inside `tree` are in original-document
/// space (the same space the host text uses).
pub(super) struct InjectionLayer {
    /// Tree-sitter syntax tree for this layer.
    pub(super) tree: tree_sitter::Tree,
}

/// Enumerate the injection stack at `byte` in `host_text`.
///
/// Returns `[host, layer₁, ..., deepest]`. Always contains at least the host
/// layer when `host_tree` parses successfully; deeper layers are only added
/// when the byte lies strictly inside an injection's content range
/// (half-open `[start, end)` per ADR-0025).
///
/// `host_language` selects the injection query used at each level — the same
/// query is consulted recursively for nested injections, matching the
/// semantic-tokens parallel collector's behavior.
///
/// Returns `None` if the host layer cannot be cloned (`tree_sitter::Tree`
/// clone is cheap and infallible in practice, so this currently never fires
/// but is kept as a signature concession to future failure modes).
pub(super) fn injection_stack_at(
    coordinator: &LanguageCoordinator,
    host_language: &str,
    host_text: &str,
    host_tree: &tree_sitter::Tree,
    byte: usize,
) -> Vec<InjectionLayer> {
    let mut stack: Vec<InjectionLayer> = Vec::new();
    stack.push(InjectionLayer {
        tree: host_tree.clone(),
    });

    let registry = coordinator.language_registry_for_parallel();
    let mut current_language: String = host_language.to_string();

    // Walk one injection deeper per iteration. The depth cap mirrors the
    // semantic-tokens recursion limit so misconfigured grammars cannot make
    // this helper loop indefinitely.
    for _depth in 0..MAX_INJECTION_DEPTH {
        let Some(injection_query) = coordinator.injection_query(&current_language) else {
            break;
        };

        // Take the **current deepest** layer's tree, ask it which injections
        // overlap the cursor, and pick the smallest containing one. Using
        // the deepest tree (rather than always the host) ensures we discover
        // injections nested inside an already-injected region.
        let deepest_tree = &stack
            .last()
            .expect("stack always contains at least the host layer")
            .tree;
        let root = deepest_tree.root_node();
        let Some(injections) = collect_all_injections(&root, host_text, Some(&injection_query))
        else {
            break;
        };

        let mut containing: Vec<_> = injections
            .into_iter()
            .filter(|r| {
                let s = r.content_node.start_byte();
                let e = r.content_node.end_byte();
                // ADR-0025 §"End-of-Document Exception": when the cursor sits
                // exactly at the document end (`byte == L && L > 0`), inclusion
                // is widened to `byte == e` for nodes whose `e == L`. Everywhere
                // else, the half-open `byte < e` rule applies. The exception is
                // gated on byte == host_text.len() so it never affects interior
                // boundaries (e.g., the byte just before a closing code fence).
                if byte == host_text.len() {
                    s <= byte && byte <= e
                } else {
                    s <= byte && byte < e
                }
            })
            .collect();
        // Smallest range wins — that's the most specific injection at the cursor.
        containing.sort_by_key(|r| r.content_node.end_byte() - r.content_node.start_byte());
        let Some(region) = containing.into_iter().next() else {
            break;
        };

        // Pass the actual injection content to the language resolver so its
        // shebang / first-line heuristics (ADR-0005) can fire for nested
        // injections — passing "" would silently disable detection.
        let content = &host_text[region.content_node.start_byte()..region.content_node.end_byte()];
        let Some((resolved_lang, _)) =
            coordinator.resolve_injection_language(&region.language, content)
        else {
            break;
        };
        let Some(language) = registry.get(&resolved_lang) else {
            break;
        };

        // Build absolute included ranges (doc-coord) for parsing. When the
        // injection has `compute_included_ranges` gaps (e.g., blockquote
        // prefix exclusion), shift each gap by `content_node.start_byte()`
        // and `content_node.start_position()` to land in host coordinates.
        let content_start = region.content_node.start_byte();
        let content_start_pos = region.content_node.start_position();
        let absolute_ranges: Vec<tree_sitter::Range> =
            match compute_included_ranges(&region.content_node, region.include_children) {
                Some(gaps) => gaps
                    .into_iter()
                    .map(|r| tree_sitter::Range {
                        start_byte: content_start + r.start_byte,
                        end_byte: content_start + r.end_byte,
                        start_point: tree_sitter::Point {
                            row: content_start_pos.row + r.start_point.row,
                            column: if r.start_point.row == 0 {
                                content_start_pos.column + r.start_point.column
                            } else {
                                r.start_point.column
                            },
                        },
                        end_point: tree_sitter::Point {
                            row: content_start_pos.row + r.end_point.row,
                            column: if r.end_point.row == 0 {
                                content_start_pos.column + r.end_point.column
                            } else {
                                r.end_point.column
                            },
                        },
                    })
                    .collect(),
                None => vec![tree_sitter::Range {
                    start_byte: region.content_node.start_byte(),
                    end_byte: region.content_node.end_byte(),
                    start_point: region.content_node.start_position(),
                    end_point: region.content_node.end_position(),
                }],
            };

        let Some(injected_tree) =
            parse_with_absolute_ranges(&language, host_text, &absolute_ranges)
        else {
            break;
        };

        stack.push(InjectionLayer {
            tree: injected_tree,
        });
        current_language = resolved_lang;
    }

    stack
}

/// Parse `text` with a fresh tree-sitter parser configured for `language`,
/// restricted to `ranges` (absolute doc-coord). Returns `None` if the parser
/// cannot be initialised, the ranges are rejected, or parsing fails.
///
/// We construct a one-off parser rather than reaching into the document
/// parser pool because the pool is async-locked and this helper runs from
/// synchronous LSP handler code paths. Parser construction is cheap (each
/// call instantiates a fresh `tree_sitter::Parser` and reads a few pointers
/// from the registry); we re-evaluate if profiling shows this on a hot path.
fn parse_with_absolute_ranges(
    language: &tree_sitter::Language,
    text: &str,
    ranges: &[tree_sitter::Range],
) -> Option<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(language).is_err() {
        return None;
    }
    if parser.set_included_ranges(ranges).is_err() {
        return None;
    }
    parser.parse(text, None)
}
