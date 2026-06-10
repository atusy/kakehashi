//! Compute the injection layer stack at a byte offset (node-reference-protocol PR-4 helper).
//!
//! The host language tree is layer 0; each enclosing `@injection.content`
//! that contains the cursor adds a deeper layer. Layers are returned in
//! outermost-to-innermost order so callers can index by the node-reference-protocol decision's formulas:
//! `stack[n]` for positive `n` and `stack[stack.len() + n]` for negative `n`.
//!
//! Trees within each layer are parsed against the **full host text** with
//! tree-sitter's `set_included_ranges`, which means every node's
//! `start_byte` / `end_byte` is already in original-document coordinates.
//! That property is load-bearing: the entry-point handler issues ULIDs via
//! `NodeTracker::get_or_create_in_layer(uri, start_byte, end_byte, kind, layer)`,
//! and the tracker keys must stay in the host's byte space so subsequent
//! `parent` / `children` / `text` calls and `didChange` adjustments line
//! up across layers. The `layer` index distinguishes a host node from an
//! injected node sharing the same span and kind (lazy-node-identity-tracking).

use crate::analysis::offset_calculator::{ByteRange, calculate_effective_range};
use crate::language::LanguageCoordinator;
use crate::language::injection::{
    MAX_INJECTION_DEPTH, collect_all_injections, compute_included_ranges,
    intersect_included_ranges, parse_offset_directive_for_pattern,
};
use crate::lsp::lsp_impl::kakehashi::node::lookup::find_node_at;

/// One layer in the injection stack at a position.
///
/// `tree` is owned so the caller can keep using it after this helper returns,
/// and so the host layer can carry the document tree without cloning lifetime
/// dependencies. Byte coordinates inside `tree` are in original-document
/// space (the same space the host text uses).
pub(super) struct InjectionLayer {
    /// Tree-sitter syntax tree for this layer.
    pub(super) tree: tree_sitter::Tree,
    /// Absolute ranges in host coordinates that this layer's tree was parsed
    /// against. The host layer spans the whole document; each deeper layer's
    /// ranges are the intersection of its own effective ranges with its
    /// parent's, so container exclusions (e.g. blockquote `> ` prefixes) are
    /// inherited down the nesting chain.
    pub(super) ranges: Vec<tree_sitter::Range>,
}

/// Whether `pattern_index` carries an `#offset!` directive. Used to decide if
/// the raw-content-node fast bounds check is safe: an offset can extend the
/// effective range past the raw node, so the shortcut only holds without one.
fn pattern_has_offset(injection_query: &tree_sitter::Query, pattern_index: usize) -> bool {
    parse_offset_directive_for_pattern(injection_query, pattern_index).is_some()
}

/// Build the full-document range used to seed the host layer.
fn whole_document_range(host_text: &str) -> tree_sitter::Range {
    tree_sitter::Range {
        start_byte: 0,
        end_byte: host_text.len(),
        start_point: tree_sitter::Point { row: 0, column: 0 },
        end_point: byte_to_point(host_text, host_text.len()),
    }
}

/// Enumerate the injection stack at `byte` in `host_text`.
///
/// Returns `[host, layer₁, ..., deepest]`. Always contains at least the host
/// layer when `host_tree` parses successfully; deeper layers are only added
/// when the byte lies strictly inside an injection's content range
/// (half-open `[start, end)` per node-reference-protocol).
///
/// `host_language` selects the injection query for layer 0 only. Each deeper
/// level uses the **language resolved for the layer above it** (the previous
/// layer's `@injection.language`) to pick its query, so a Markdown → Python →
/// Regex chain consults the markdown, then python, then regex injection
/// queries in turn — matching the semantic-tokens parallel collector.
///
/// The returned `Vec` always contains at least the host layer (layer 0); the
/// function never fails — a parse/registry miss at any depth simply stops the
/// walk and returns the layers gathered so far.
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
        ranges: vec![whole_document_range(host_text)],
    });

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
        // injections nested inside an already-injected region. We also carry
        // the parent layer's ranges so a nested injection inherits container
        // exclusions (blockquote prefixes etc.) from every ancestor.
        let parent_layer = stack
            .last()
            .expect("stack always contains at least the host layer");
        let parent_ranges = parent_layer.ranges.clone();
        let root = parent_layer.tree.root_node();
        let Some(injections) = collect_all_injections(&root, host_text, Some(&injection_query))
        else {
            break;
        };

        // Materialise the effective absolute ranges for every candidate so the
        // containment check considers the bytes the injection parser will
        // actually see — not the raw `@injection.content` span:
        //   - apply any `#offset!` directive so prefixes / suffixes excluded by
        //     the query (e.g., frontmatter fences, string quotes) are out;
        //   - intersect with `compute_included_ranges` so blockquote `> `
        //     prefixes (`block_continuation` children) are out.
        // A cursor on an excluded byte must NOT push a new injection layer —
        // node-reference-protocol §"Half-Open Intervals" works against the effective ranges.
        let host_len = host_text.len();
        let mut candidates: Vec<(_, Vec<tree_sitter::Range>)> = Vec::new();
        for region in injections {
            // Fast bounds check: when there is no `#offset!` directive the
            // effective ranges can only ever be a *sub*-range of the raw
            // content node (include-children gaps only remove bytes), so a
            // cursor outside the raw span cannot be inside them — reject before
            // the expensive build_effective_ranges call. We must NOT apply this
            // shortcut when an offset directive is present: positive end /
            // negative start offsets can *extend* the effective range past the
            // raw content node, so containment has to be judged on the
            // effective ranges alone.
            if !pattern_has_offset(&injection_query, region.pattern_index) {
                let raw_start = region.content_node.start_byte();
                let raw_end = region.content_node.end_byte();
                let outside_raw = if byte == host_len {
                    byte < raw_start || byte > raw_end
                } else {
                    byte < raw_start || byte >= raw_end
                };
                if outside_raw {
                    continue;
                }
            }
            let own_ranges = build_effective_ranges(&region, host_text, &injection_query);
            if own_ranges.is_empty() {
                continue;
            }
            // Inherit parent exclusions: a byte the parent layer already
            // excluded (e.g. a blockquote `> ` prefix on an intermediate line)
            // must stay excluded for the nested parser. For the host layer the
            // parent range is the whole document, so this is a no-op there.
            let absolute_ranges = intersect_included_ranges(&parent_ranges, &own_ranges);
            if absolute_ranges.is_empty() {
                continue;
            }
            if !ranges_contain_byte(&absolute_ranges, byte, host_len) {
                continue;
            }
            candidates.push((region, absolute_ranges));
        }
        // Smallest effective span wins — that's the most specific injection at
        // the cursor after offset/include adjustments.
        candidates.sort_by_key(|(_, ranges)| total_span(ranges));
        let Some((region, absolute_ranges)) = candidates.into_iter().next() else {
            break;
        };

        // Pass the actual injection content to the language resolver so its
        // shebang / first-line heuristics (language-detection-fallback-chain) can fire for nested
        // injections — passing "" would silently disable detection.
        let content = &host_text[region.content_node.start_byte()..region.content_node.end_byte()];
        let Some((resolved_lang, _)) =
            coordinator.resolve_injection_language(&region.language, content)
        else {
            break;
        };
        // `get` clones the grammar out (owned `Language`) and releases its
        // internal DashMap ref before returning, so there is no read guard to
        // scope around the parse below. Fetched inline (not via a function-
        // level binding) to keep that intent obvious.
        let Some(language) = coordinator
            .language_registry_for_parallel()
            .get(&resolved_lang)
        else {
            break;
        };

        let Some(injected_tree) =
            parse_with_absolute_ranges(&language, host_text, &absolute_ranges)
        else {
            break;
        };

        stack.push(InjectionLayer {
            tree: injected_tree,
            ranges: absolute_ranges,
        });
        current_language = resolved_lang;
    }

    stack
}

/// Resolve a tracked node to a tree-sitter node **in the exact layer that
/// minted it**, identified by the `layer` discriminator recorded in its
/// identity key (lazy-node-identity-tracking §"Node Uniqueness Key").
///
/// `layer == 0` resolves against the host tree directly (the common case, no
/// stack walk). A deeper `layer` rebuilds the injection stack at `start` and
/// searches `stack[layer]` only. We deliberately do **not** fall back to other
/// layers: a node carries the layer it was created in, and resolving it in a
/// different layer would violate node-reference-protocol's per-layer Scope rule.
/// Within a single parse this is exactly the host-vs-injected collision the
/// `layer` key prevents — a host and injected node sharing `(start, end, kind)`
/// would otherwise be indistinguishable here (issue #313).
///
/// Across edits the depth index is a weaker guarantee. If an edit makes the
/// stack shallower than `layer`, `stack.get(layer)` is `None` and we return
/// `None` — a safe "re-acquire" signal. But `layer` is only a depth, not a tree
/// identity: an edit that restructures the nesting while keeping
/// `stack.len() > layer` can leave a *different* tree at that depth. Resolution
/// then succeeds only if that tree happens to hold a node at the identical
/// `(start, end, kind)`, and otherwise returns `None`. We do not (and with a
/// depth index cannot) detect that case, so the "re-acquire on `null`" contract
/// — not a wrong-tree guarantee — is what protects clients. See the
/// layer-discriminator options in lazy-node-identity-tracking for the
/// region-ULID alternative that would close this gap.
///
/// `f` is invoked at most once, with the matching `Node`. Returning `None`
/// from `f` is distinguishable from the "no match" outcome only by
/// the caller's outer `Option` — both surface as `Option<R>` because the
/// outer call returns `None` when nothing matched. Callers that need to
/// distinguish "found node but operation returned nothing" (e.g. parent of
/// a root) from "no match" should use a richer `R` like `Option<T>`.
#[allow(clippy::too_many_arguments)]
pub(super) fn with_resolved_node<R>(
    coordinator: &LanguageCoordinator,
    host_language: &str,
    host_text: &str,
    host_tree: &tree_sitter::Tree,
    start: usize,
    end: usize,
    kind: &'static str,
    layer: usize,
    mut f: impl FnMut(tree_sitter::Node<'_>) -> R,
) -> Option<R> {
    // Reject obviously-invalid ranges up front — same guard `find_node_at`
    // applies internally, but checking here also avoids the expensive
    // `injection_stack_at` walk (which clones and re-parses layers) for a
    // stale tracker entry whose range no longer fits the document.
    if start > end || end > host_text.len() {
        return None;
    }

    // Host layer: resolve against the host tree without the stack walk.
    if layer == 0 {
        let node = find_node_at(host_tree, start, end, kind)?;
        return Some(f(node));
    }

    // Deeper layer: rebuild the stack at `start` and search the minting layer
    // only. `stack.get(layer)` is None when the nesting is now shallower.
    let stack = injection_stack_at(coordinator, host_language, host_text, host_tree, start);
    let layer_tree = &stack.get(layer)?.tree;
    let node = find_node_at(layer_tree, start, end, kind)?;
    Some(f(node))
}

/// Collect the injection languages along the cursor's injection path at
/// `byte`, at all depths that are currently parseable.
///
/// Unlike a whole-document scan, this follows only the single smallest-
/// containing injection at each level — exactly the path `injection_stack_at`
/// would take — so its cost is O(depth at the cursor) rather than O(all
/// injections in the document). That matters for large files with many code
/// blocks: we only need grammars for the layers that actually wrap the cursor.
///
/// A language is *recorded* as soon as its `@injection.language` is resolved,
/// even if its parser is not yet loaded. But the walk only *descends* into a
/// layer whose parser is already in the registry — parsing requires a loaded
/// grammar. This makes the function a single fixpoint step: callers that
/// auto-install the returned set and call again will, on the next pass, be
/// able to parse one level deeper and surface the next language on the path.
/// Iterating to a fixpoint discovers the full nested chain at the cursor
/// (Markdown → Python → Regex …) without ever parsing with a missing grammar.
///
/// Bounded by [`MAX_INJECTION_DEPTH`] so a misconfigured grammar cycle cannot
/// loop forever.
pub(super) fn collect_injection_languages_at(
    coordinator: &LanguageCoordinator,
    host_language: &str,
    host_text: &str,
    host_tree: &tree_sitter::Tree,
    byte: usize,
) -> std::collections::HashSet<String> {
    let mut languages = std::collections::HashSet::new();

    let mut current_lang = host_language.to_string();
    let mut current_tree = host_tree.clone();
    let mut parent_ranges = vec![whole_document_range(host_text)];
    let host_len = host_text.len();

    for _depth in 0..MAX_INJECTION_DEPTH {
        let Some(injection_query) = coordinator.injection_query(&current_lang) else {
            break;
        };
        let root = current_tree.root_node();
        let Some(injections) = collect_all_injections(&root, host_text, Some(&injection_query))
        else {
            break;
        };

        // Pick the smallest injection that actually contains the cursor — the
        // same selection `injection_stack_at` makes — so discovery follows the
        // one path the per-position stack will walk.
        let mut candidates: Vec<(_, Vec<tree_sitter::Range>)> = Vec::new();
        for region in injections {
            if !pattern_has_offset(&injection_query, region.pattern_index) {
                let raw_start = region.content_node.start_byte();
                let raw_end = region.content_node.end_byte();
                let outside_raw = if byte == host_len {
                    byte < raw_start || byte > raw_end
                } else {
                    byte < raw_start || byte >= raw_end
                };
                if outside_raw {
                    continue;
                }
            }
            let own_ranges = build_effective_ranges(&region, host_text, &injection_query);
            if own_ranges.is_empty() {
                continue;
            }
            let absolute_ranges = intersect_included_ranges(&parent_ranges, &own_ranges);
            if absolute_ranges.is_empty() {
                continue;
            }
            if !ranges_contain_byte(&absolute_ranges, byte, host_len) {
                continue;
            }
            candidates.push((region, absolute_ranges));
        }
        candidates.sort_by_key(|(_, ranges)| total_span(ranges));
        let Some((region, absolute_ranges)) = candidates.into_iter().next() else {
            break;
        };

        let content = &host_text[region.content_node.start_byte()..region.content_node.end_byte()];
        let Some((resolved_lang, _)) =
            coordinator.resolve_injection_language(&region.language, content)
        else {
            break;
        };
        languages.insert(resolved_lang.clone());

        // Descend only if the parser is loaded; otherwise stop and let the
        // caller install it, then re-run for the next tier (fixpoint).
        // `get` returns an owned `Language` (clones out, drops its DashMap ref
        // internally), so no read guard spans the parse; fetched inline.
        let Some(language) = coordinator
            .language_registry_for_parallel()
            .get(&resolved_lang)
        else {
            break;
        };
        let Some(injected_tree) =
            parse_with_absolute_ranges(&language, host_text, &absolute_ranges)
        else {
            break;
        };

        current_tree = injected_tree;
        parent_ranges = absolute_ranges;
        current_lang = resolved_lang;
    }

    languages
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

/// Compute the absolute byte-range list that the injection parser would see
/// for `region` given the host's `@injection.content` node, any `#offset!`
/// directive on the pattern, and `compute_included_ranges` gap exclusions.
///
/// Returns an empty `Vec` when:
/// - the offset-adjusted range is degenerate (`start >= end`); or
/// - `compute_included_ranges` returns gaps that all fall outside the
///   offset-adjusted span (intersection is empty).
fn build_effective_ranges(
    region: &crate::language::injection::InjectionRegionInfo<'_>,
    host_text: &str,
    injection_query: &tree_sitter::Query,
) -> Vec<tree_sitter::Range> {
    // 1. Apply #offset! to the raw content_node span.
    let offset = parse_offset_directive_for_pattern(injection_query, region.pattern_index);
    let (eff_start, eff_end) = match offset {
        Some(off) => {
            let byte_range = ByteRange::new(
                region.content_node.start_byte(),
                region.content_node.end_byte(),
            );
            let eff = calculate_effective_range(host_text, byte_range, off);
            (eff.start, eff.end)
        }
        None => (
            region.content_node.start_byte(),
            region.content_node.end_byte(),
        ),
    };
    if eff_start >= eff_end {
        return Vec::new();
    }

    // 2. Compute compute_included_ranges gap list (relative coords inside
    // content_node) and shift to absolute. Intersect each gap with the
    // offset-adjusted span so blockquote prefixes excluded by the gap list and
    // bytes trimmed by the offset directive are both honoured.
    //
    // Skip the include-gap step when an offset directive is active: gaps from
    // `compute_included_ranges` are relative to `content_node.start_byte()`,
    // but the offset may have moved the effective start away from there, and
    // mixing the two coordinate frames would yield wrong ranges. The semantic-
    // tokens parallel collector takes the same trade-off (see parallel.rs).
    let content_start = region.content_node.start_byte();
    let content_start_pos = region.content_node.start_position();
    let absolute_ranges: Vec<tree_sitter::Range> = if offset.is_some() {
        // #offset! shifts are byte arithmetic over i32 deltas, so the effective
        // bounds can land mid-codepoint. Align both ends down to a UTF-8
        // boundary before handing them to tree-sitter — passing a mid-char
        // byte to set_included_ranges is UB / panic territory, and it would
        // also desync from byte_to_point (which aligns internally).
        let aligned_start = align_down(host_text, eff_start);
        let aligned_end = align_down(host_text, eff_end);
        // The alignment could, in pathological cases, collapse the range
        // (e.g. both ends fall inside the same multi-byte char). Guard so we
        // never emit a zero/negative-width range to the parser.
        if aligned_start >= aligned_end {
            return Vec::new();
        }
        vec![tree_sitter::Range {
            start_byte: aligned_start,
            end_byte: aligned_end,
            start_point: byte_to_point(host_text, aligned_start),
            end_point: byte_to_point(host_text, aligned_end),
        }]
    } else {
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
        }
    };

    absolute_ranges
}

/// Half-open containment over a list of disjoint ranges, with the node-reference-protocol decision's
/// end-of-document exception (`byte == host_len` includes nodes whose
/// `end_byte == host_len`).
fn ranges_contain_byte(ranges: &[tree_sitter::Range], byte: usize, host_len: usize) -> bool {
    let at_eod = byte == host_len;
    ranges.iter().any(|r| {
        let s = r.start_byte;
        let e = r.end_byte;
        if at_eod {
            s <= byte && byte <= e
        } else {
            s <= byte && byte < e
        }
    })
}

/// Total byte length covered by `ranges`. Used as the smallest-injection
/// tiebreaker — narrower effective spans win.
fn total_span(ranges: &[tree_sitter::Range]) -> usize {
    ranges
        .iter()
        .map(|r| r.end_byte.saturating_sub(r.start_byte))
        .sum()
}

/// Clamp `byte` into `text` and round down to the nearest UTF-8 character
/// boundary. Byte values derived from `#offset!` directives (i32 deltas in
/// byte space) are not guaranteed to land on boundaries; feeding a mid-char
/// byte to tree-sitter's `set_included_ranges` or to a string slice would
/// panic. Rounding down keeps the offset inside the intended content.
fn align_down(text: &str, byte: usize) -> usize {
    let mut aligned = byte.min(text.len());
    while aligned > 0 && !text.is_char_boundary(aligned) {
        aligned -= 1;
    }
    aligned
}

/// Materialize every child injection region of `parent_tree` with its
/// effective absolute ranges, sorted by start byte (document order across
/// siblings — the deterministic order captures-protocol's positional delta
/// requires). Shared by the cursor-path stack ([`injection_stack_at`]) and the
/// document-wide walkers below; unlike the cursor path there is no byte
/// containment or smallest-wins selection — every region qualifies, optionally
/// pruned to those intersecting `byte_filter`.
fn effective_child_regions<'t>(
    coordinator: &LanguageCoordinator,
    parent_language: &str,
    parent_tree: &'t tree_sitter::Tree,
    parent_ranges: &[tree_sitter::Range],
    host_text: &str,
    byte_filter: Option<&std::ops::Range<usize>>,
) -> Vec<(
    crate::language::injection::InjectionRegionInfo<'t>,
    Vec<tree_sitter::Range>,
)> {
    let Some(injection_query) = coordinator.injection_query(parent_language) else {
        return Vec::new();
    };
    let root = parent_tree.root_node();
    let Some(injections) = collect_all_injections(&root, host_text, Some(&injection_query)) else {
        return Vec::new();
    };

    let mut regions = Vec::new();
    for region in injections {
        let own_ranges = build_effective_ranges(&region, host_text, &injection_query);
        if own_ranges.is_empty() {
            continue;
        }
        // Inherit parent exclusions (blockquote prefixes etc.), as the
        // cursor-path stack does.
        let absolute_ranges = intersect_included_ranges(parent_ranges, &own_ranges);
        if absolute_ranges.is_empty() {
            continue;
        }
        if let Some(filter) = byte_filter
            && !ranges_intersect(&absolute_ranges, filter)
        {
            continue;
        }
        regions.push((region, absolute_ranges));
    }
    regions.sort_by_key(|(_, ranges)| ranges.first().map_or(0, |r| r.start_byte));
    regions
}

/// Half-open intersection of a disjoint range list with `filter`. A zero-width
/// filter degenerates to point containment so a cursor-sized range still
/// selects the layer under it.
fn ranges_intersect(ranges: &[tree_sitter::Range], filter: &std::ops::Range<usize>) -> bool {
    if filter.start == filter.end {
        let p = filter.start;
        return ranges.iter().any(|r| r.start_byte <= p && p < r.end_byte);
    }
    ranges
        .iter()
        .any(|r| r.start_byte < filter.end && filter.start < r.end_byte)
}

/// Visit every injection layer of the document in **document-order DFS**: the
/// host first, then each injection region by ascending start byte, recursing
/// into nested injections before moving to the next sibling
/// (captures-protocol §"The `injection` parameter").
///
/// `visit` receives the layer's resolved language, its tree (parsed against
/// the full host text via `set_included_ranges`, so byte coordinates are in
/// host space), and its depth — the same depth index `injection_stack_at`
/// assigns, so nodes minted with it resolve through the per-layer Scope rule.
///
/// Regions whose grammar is not loaded (or fails to parse) are skipped
/// silently — discovery and auto-install are the caller's job, via
/// [`collect_injection_languages_in_document`]. `byte_filter` prunes regions
/// (and their entire subtrees) that don't intersect the given host-byte range.
pub(in crate::lsp::lsp_impl::kakehashi) fn walk_document_layers(
    coordinator: &LanguageCoordinator,
    host_language: &str,
    host_text: &str,
    host_tree: &tree_sitter::Tree,
    byte_filter: Option<&std::ops::Range<usize>>,
    visit: &mut dyn FnMut(&str, &tree_sitter::Tree, usize),
) {
    visit(host_language, host_tree, 0);
    walk_child_layers(
        coordinator,
        host_language,
        host_tree,
        &[whole_document_range(host_text)],
        host_text,
        1,
        byte_filter,
        visit,
    );
}

#[allow(clippy::too_many_arguments)]
fn walk_child_layers(
    coordinator: &LanguageCoordinator,
    parent_language: &str,
    parent_tree: &tree_sitter::Tree,
    parent_ranges: &[tree_sitter::Range],
    host_text: &str,
    depth: usize,
    byte_filter: Option<&std::ops::Range<usize>>,
    visit: &mut dyn FnMut(&str, &tree_sitter::Tree, usize),
) {
    if depth > MAX_INJECTION_DEPTH {
        return;
    }
    for (region, absolute_ranges) in effective_child_regions(
        coordinator,
        parent_language,
        parent_tree,
        parent_ranges,
        host_text,
        byte_filter,
    ) {
        let content = &host_text[region.content_node.start_byte()..region.content_node.end_byte()];
        let Some((resolved_lang, _)) =
            coordinator.resolve_injection_language(&region.language, content)
        else {
            continue;
        };
        let Some(language) = coordinator
            .language_registry_for_parallel()
            .get(&resolved_lang)
        else {
            continue;
        };
        let Some(tree) = parse_with_absolute_ranges(&language, host_text, &absolute_ranges) else {
            continue;
        };
        visit(&resolved_lang, &tree, depth);
        walk_child_layers(
            coordinator,
            &resolved_lang,
            &tree,
            &absolute_ranges,
            host_text,
            depth + 1,
            byte_filter,
            visit,
        );
    }
}

/// Collect the injection languages appearing **anywhere** in the document, at
/// all currently-parseable depths — the document-wide analog of
/// [`collect_injection_languages_at`], with the same fixpoint contract: a
/// language is *recorded* once its `@injection.language` resolves, but the
/// walk only *descends* into layers whose parser is already loaded, so callers
/// that install the returned set and call again discover one tier deeper per
/// round.
pub(in crate::lsp::lsp_impl::kakehashi) fn collect_injection_languages_in_document(
    coordinator: &LanguageCoordinator,
    host_language: &str,
    host_text: &str,
    host_tree: &tree_sitter::Tree,
) -> std::collections::HashSet<String> {
    let mut languages = std::collections::HashSet::new();
    discover_child_languages(
        coordinator,
        host_language,
        host_tree,
        &[whole_document_range(host_text)],
        host_text,
        1,
        &mut languages,
    );
    languages
}

fn discover_child_languages(
    coordinator: &LanguageCoordinator,
    parent_language: &str,
    parent_tree: &tree_sitter::Tree,
    parent_ranges: &[tree_sitter::Range],
    host_text: &str,
    depth: usize,
    languages: &mut std::collections::HashSet<String>,
) {
    if depth > MAX_INJECTION_DEPTH {
        return;
    }
    for (region, absolute_ranges) in effective_child_regions(
        coordinator,
        parent_language,
        parent_tree,
        parent_ranges,
        host_text,
        None,
    ) {
        let content = &host_text[region.content_node.start_byte()..region.content_node.end_byte()];
        let Some((resolved_lang, _)) =
            coordinator.resolve_injection_language(&region.language, content)
        else {
            continue;
        };
        languages.insert(resolved_lang.clone());

        // Descend only when the parser is loaded (fixpoint contract).
        let Some(language) = coordinator
            .language_registry_for_parallel()
            .get(&resolved_lang)
        else {
            continue;
        };
        let Some(tree) = parse_with_absolute_ranges(&language, host_text, &absolute_ranges) else {
            continue;
        };
        discover_child_languages(
            coordinator,
            &resolved_lang,
            &tree,
            &absolute_ranges,
            host_text,
            depth + 1,
            languages,
        );
    }
}

/// Convert an absolute byte offset to a `tree_sitter::Point`. Used when an
/// offset directive shifts the injection boundary away from a known node
/// position, so we can't reuse the content node's start/end points.
fn byte_to_point(text: &str, byte: usize) -> tree_sitter::Point {
    // Align first — slicing `&text[..clamped]` on a mid-character byte would
    // panic and crash the LSP server.
    let clamped = align_down(text, byte);
    let prefix = &text[..clamped];
    let row = prefix.bytes().filter(|b| *b == b'\n').count();
    let last_nl = prefix.rfind('\n');
    let column = match last_nl {
        Some(idx) => clamped - idx - 1,
        None => clamped,
    };
    tree_sitter::Point { row, column }
}
