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
    MAX_INJECTION_DEPTH, byte_to_point, byte_to_point_anchored, collect_all_injections,
    compute_included_ranges, compute_included_ranges_clipped, effective_offset_for_pattern,
    intersect_included_ranges,
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
    /// More than one same-depth injection region contained the cursor when
    /// this layer was selected. A depth-only id cannot identify the minting
    /// sibling, so id-based accessors must reject this layer.
    ambiguous: bool,
}

/// Outcome of resolving a depth-keyed node reference.
pub(super) enum NodeResolution<R> {
    Found(R),
    NotFound,
    /// Multiple same-depth siblings contain the anchor, so choosing one would
    /// risk returning a node from a tree that did not mint the id (#350).
    Ambiguous,
}

/// Whether `pattern_index` carries an `#offset!` directive. Used to decide if
/// the raw-content-node fast bounds check is safe: an offset can extend the
/// effective range past the raw node, so the shortcut only holds without one.
fn pattern_has_offset(injection_query: &tree_sitter::Query, pattern_index: usize) -> bool {
    effective_offset_for_pattern(injection_query, pattern_index).is_some()
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
        ambiguous: false,
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
        let parent_ambiguous = parent_layer.ambiguous;
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
        // Once an ancestor was ambiguous, every descendant is ambiguous too:
        // rebuilding the same child path inside the chosen smallest parent
        // still cannot prove that parent was the sibling which minted the id.
        let ambiguous = parent_ambiguous || candidates.len() > 1;
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
            ambiguous,
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
/// stack shallower than `layer`, resolution returns `NotFound` — a safe
/// "re-acquire" signal. But `layer` is only a depth, not a tree
/// identity: an edit that restructures the nesting while keeping
/// `stack.len() > layer` can leave a *different* tree at that depth. Resolution
/// then succeeds only if that tree happens to hold a node at the identical
/// `(start, end, kind)`, and otherwise returns `NotFound`. The resolver returns
/// `Ambiguous` when multiple same-depth regions contain the anchor, closing the
/// known overlapping-sibling path (#350), but a depth index still cannot
/// detect every cross-edit replacement of one non-overlapping region by
/// another. See the
/// layer-discriminator options in lazy-node-identity-tracking for the
/// region-ULID alternative that would close this gap.
///
/// `f` is invoked at most once, with the matching `Node`, and its result is
/// wrapped in `NodeResolution::Found`. This preserves `Found(None)` for an
/// operation such as asking a tree root for its parent, distinct from both
/// `NotFound` and `Ambiguous`.
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
) -> NodeResolution<R> {
    with_resolved_node_ranges(
        coordinator,
        host_language,
        host_text,
        host_tree,
        start,
        end,
        kind,
        layer,
        |node, _ranges| f(node),
    )
}

/// Like [`with_resolved_node`], but also hands the closure the **included
/// ranges the minting layer's tree was parsed against** (host coordinates).
///
/// The coordinate-input accessors (`firstChildForByte`,
/// `descendant*For{Byte,Point}Range`) need them: an injected layer parsed with
/// non-contiguous `included_ranges` (e.g. blockquoted code, where `> `
/// prefixes are excluded) has *gap* bytes inside a node's contiguous span that
/// are not real injected content. A coordinate argument landing in a gap must
/// collapse to `null` — consistent with the entry point, which never pushes an
/// injection layer for a gap byte (#341). For the host layer the ranges are
/// the whole document, so gap checks are vacuous there.
#[allow(clippy::too_many_arguments)]
pub(super) fn with_resolved_node_ranges<R>(
    coordinator: &LanguageCoordinator,
    host_language: &str,
    host_text: &str,
    host_tree: &tree_sitter::Tree,
    start: usize,
    end: usize,
    kind: &'static str,
    layer: usize,
    mut f: impl FnMut(tree_sitter::Node<'_>, &[tree_sitter::Range]) -> R,
) -> NodeResolution<R> {
    // Reject obviously-invalid ranges up front — same guard `find_node_at`
    // applies internally, but checking here also avoids the expensive
    // `injection_stack_at` walk (which clones and re-parses layers) for a
    // stale tracker entry whose range no longer fits the document.
    if start > end || end > host_text.len() {
        return NodeResolution::NotFound;
    }

    // Host layer: resolve against the host tree without the stack walk.
    if layer == 0 {
        let Some(node) = find_node_at(host_tree, start, end, kind) else {
            return NodeResolution::NotFound;
        };
        // This is the shared prelude for EVERY id-based accessor, so the
        // whole-document range must be O(1): reuse the root's end position
        // instead of whole_document_range, whose byte_to_point would rescan
        // the document on each call. The end point only matters if a consumer
        // ever reads it — the gap checks are byte-based — and the root's end
        // position is the document end for a whole-document parse anyway.
        let ranges = [tree_sitter::Range {
            start_byte: 0,
            end_byte: host_text.len(),
            start_point: tree_sitter::Point { row: 0, column: 0 },
            end_point: host_tree.root_node().end_position(),
        }];
        return NodeResolution::Found(f(node, &ranges));
    }

    // Deeper layer: rebuild the stack at `start` and search the minting layer
    // only. `stack.get(layer)` is None when the nesting is now shallower.
    let stack = injection_stack_at(coordinator, host_language, host_text, host_tree, start);
    let Some(layer_entry) = stack.get(layer) else {
        return if stack.last().is_some_and(|entry| entry.ambiguous) {
            NodeResolution::Ambiguous
        } else {
            NodeResolution::NotFound
        };
    };
    if layer_entry.ambiguous {
        return NodeResolution::Ambiguous;
    }
    let Some(node) = find_node_at(&layer_entry.tree, start, end, kind) else {
        return NodeResolution::NotFound;
    };
    NodeResolution::Found(f(node, &layer_entry.ranges))
}

/// Resolve **two** tracked nodes in the **same** minting layer and run `f` on
/// the pair — the two-id contract behind `childWithDescendant` (issue #335).
///
/// Both `(start, end, kind)` triples must name nodes in one tree: the layer's
/// tree is materialised once and both lookups run against it, so the pair can
/// never straddle two layers. The stack walk is anchored at the *descendant's*
/// start byte — the method's contract requires the descendant to lie inside
/// `node`, so when the pair is genuinely related the smallest-region path at
/// that byte reaches the layer that minted both. An unrelated pair (descendant
/// outside `node`, or minted from different disjoint regions) returns
/// `NotFound`. Overlapping same-depth ancestry returns `Ambiguous` instead;
/// both outcomes surface as the protocol's null re-acquire signal.
#[allow(clippy::too_many_arguments)]
pub(super) fn with_resolved_node_pair<R>(
    coordinator: &LanguageCoordinator,
    host_language: &str,
    host_text: &str,
    host_tree: &tree_sitter::Tree,
    node: (usize, usize, &'static str),
    descendant: (usize, usize, &'static str),
    layer: usize,
    mut f: impl FnMut(tree_sitter::Node<'_>, tree_sitter::Node<'_>) -> R,
) -> NodeResolution<R> {
    let (node_start, node_end, node_kind) = node;
    let (desc_start, desc_end, desc_kind) = descendant;
    // Same defensive range guards as the single-node path: a stale tracker
    // entry must collapse to null before any tree work.
    if node_start > node_end
        || node_end > host_text.len()
        || desc_start > desc_end
        || desc_end > host_text.len()
    {
        return NodeResolution::NotFound;
    }

    // Host layer: both resolve against the host tree, no stack walk.
    if layer == 0 {
        let Some(resolved_node) = find_node_at(host_tree, node_start, node_end, node_kind) else {
            return NodeResolution::NotFound;
        };
        let Some(resolved_desc) = find_node_at(host_tree, desc_start, desc_end, desc_kind) else {
            return NodeResolution::NotFound;
        };
        return NodeResolution::Found(f(resolved_node, resolved_desc));
    }

    let stack = injection_stack_at(coordinator, host_language, host_text, host_tree, desc_start);
    let Some(layer_entry) = stack.get(layer) else {
        return if stack.last().is_some_and(|entry| entry.ambiguous) {
            NodeResolution::Ambiguous
        } else {
            NodeResolution::NotFound
        };
    };
    if layer_entry.ambiguous {
        return NodeResolution::Ambiguous;
    }
    let Some(resolved_node) = find_node_at(&layer_entry.tree, node_start, node_end, node_kind)
    else {
        return NodeResolution::NotFound;
    };
    let Some(resolved_desc) = find_node_at(&layer_entry.tree, desc_start, desc_end, desc_kind)
    else {
        return NodeResolution::NotFound;
    };
    NodeResolution::Found(f(resolved_node, resolved_desc))
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
    // Bounded like every other compute-pool parse: this runs inside node/*
    // work-units, where an unbounded native parse would pin a pool thread.
    let deadline = std::time::Instant::now() + crate::language::injection::NATIVE_PARSE_BUDGET;
    crate::language::injection::parse_with_deadline(&mut parser, text, None, deadline)
}

/// Compute the absolute byte-range list that the injection parser would see
/// for `region` given the host's `@injection.content` node, any `#offset!`
/// directive on the pattern, and child-exclusion gap ranges (clipped to the
/// post-offset window when both are active, #186).
///
/// Returns an empty `Vec` when the offset-adjusted range is degenerate
/// (`start >= end`, including after char-boundary alignment).
fn build_effective_ranges(
    region: &crate::language::injection::InjectionRegionInfo<'_>,
    host_text: &str,
    injection_query: &tree_sitter::Query,
) -> Vec<tree_sitter::Range> {
    // 1. Apply #offset! to the raw content_node span.
    let offset = effective_offset_for_pattern(injection_query, region.pattern_index);
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

    // 2. Compute the child-exclusion gap list (relative coords inside the
    // effective window) and shift to absolute, so blockquote prefixes excluded
    // by the gap list and bytes trimmed by the offset directive are both
    // honoured (#186).
    let content_start = region.content_node.start_byte();
    let content_start_pos = region.content_node.start_position();
    let absolute_ranges: Vec<tree_sitter::Range> = if offset.is_some() {
        // calculate_effective_range already clamped, snapped inward to UTF-8
        // boundaries, and normalized start <= end, so eff_start/eff_end are
        // safe to hand to tree-sitter (a mid-char byte in set_included_ranges
        // is UB / panic territory); the degenerate case returned above.
        match compute_included_ranges_clipped(
            &region.content_node,
            region.include_children,
            host_text,
            eff_start..eff_end,
        ) {
            Some(gaps) => {
                let window_start_pos =
                    byte_to_point_anchored(host_text, eff_start, content_start, content_start_pos);
                gaps.into_iter()
                    .map(|r| absolutize_range(r, eff_start, window_start_pos))
                    .collect()
            }
            None => {
                let start_point =
                    byte_to_point_anchored(host_text, eff_start, content_start, content_start_pos);
                let end_point = byte_to_point_anchored(host_text, eff_end, eff_start, start_point);
                vec![tree_sitter::Range {
                    start_byte: eff_start,
                    end_byte: eff_end,
                    start_point,
                    end_point,
                }]
            }
        }
    } else {
        match compute_included_ranges(&region.content_node, region.include_children) {
            Some(gaps) => gaps
                .into_iter()
                .map(|r| absolutize_range(r, content_start, content_start_pos))
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

/// Shift a window-relative gap range (as produced by
/// `compute_included_ranges` / `compute_included_ranges_clipped`) into
/// absolute host coordinates anchored at `base_byte` / `base_pos`. Columns
/// are only shifted on the window's first row — later rows already carry
/// absolute columns.
fn absolutize_range(
    r: tree_sitter::Range,
    base_byte: usize,
    base_pos: tree_sitter::Point,
) -> tree_sitter::Range {
    let absolutize_point = |p: tree_sitter::Point| tree_sitter::Point {
        row: base_pos.row + p.row,
        column: if p.row == 0 {
            base_pos.column + p.column
        } else {
            p.column
        },
    };
    tree_sitter::Range {
        start_byte: base_byte + r.start_byte,
        end_byte: base_byte + r.end_byte,
        start_point: absolutize_point(r.start_point),
        end_point: absolutize_point(r.end_point),
    }
}

/// Half-open containment over a list of disjoint ranges, with the node-reference-protocol decision's
/// end-of-document exception (`byte == host_len` includes nodes whose
/// `end_byte == host_len`).
///
/// `pub(super)` so the coordinate accessors can apply the same rule the entry
/// point uses to their byte / start-position arguments (#341).
pub(super) fn ranges_contain_byte(
    ranges: &[tree_sitter::Range],
    byte: usize,
    host_len: usize,
) -> bool {
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
            && !ranges_intersect(&absolute_ranges, filter, host_text.len())
        {
            continue;
        }
        regions.push((region, absolute_ranges));
    }
    // `sort_by_key` is stable, so ties would already keep the deterministic
    // query-match order — the extra raw-span/pattern components just make the
    // order independent of insertion order outright (the captures positional
    // delta requires a deterministic document order).
    regions.sort_by_key(|(region, ranges)| {
        (
            ranges.first().map_or(0, |r| r.start_byte),
            region.content_node.start_byte(),
            region.pattern_index,
        )
    });
    regions
}

/// Half-open intersection of a disjoint range list with `filter`. A zero-width
/// filter degenerates to point containment so a cursor-sized range still
/// selects the layer under it — including the protocol's end-of-document
/// exception (a point at `host_len` selects layers ending exactly there),
/// mirroring [`ranges_contain_byte`].
fn ranges_intersect(
    ranges: &[tree_sitter::Range],
    filter: &std::ops::Range<usize>,
    host_len: usize,
) -> bool {
    if filter.start == filter.end {
        return ranges_contain_byte(ranges, filter.start, host_len);
    }
    ranges
        .iter()
        .any(|r| r.start_byte < filter.end && filter.start < r.end_byte)
}

/// Run the standard layer walk once and collect every injected layer's
/// parsed tree in document-order DFS (parse-snapshot ADR §3, the layer-tree
/// half of never-discover-twice): the FIRST walking captures request on a
/// snapshot calls this lazily and the result rides the `ParseSnapshot`, so
/// subsequent per-keystroke walks iterate pre-parsed layers instead of
/// re-running the walk. Byte-identical to the inline walk by construction —
/// it IS the inline walk, with a collecting visitor.
pub(crate) fn collect_document_layer_trees(
    coordinator: &LanguageCoordinator,
    host_language: &str,
    host_text: &str,
    host_tree: &tree_sitter::Tree,
) -> Vec<crate::document::SnapshotLayerTree> {
    let mut layers = Vec::new();
    walk_document_layers(
        coordinator,
        host_language,
        host_text,
        host_tree,
        None,
        None,
        &mut |language, tree, depth| {
            // The host layer (depth 0) already lives on the snapshot as
            // `ParseSnapshot::tree`; store only the injected layers.
            if depth == 0 {
                return;
            }
            let ranges = tree.included_ranges();
            let span = match (ranges.first(), ranges.last()) {
                (Some(first), Some(last)) => first.start_byte..last.end_byte,
                _ => 0..host_text.len(),
            };
            layers.push(crate::document::SnapshotLayerTree {
                language: language.to_string(),
                tree: tree.clone(),
                depth,
                span,
            });
        },
    );
    layers
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
///
/// Overlapping same-depth regions remain unaddressable by the depth index:
/// the walker visits both, while the cursor-path stack keeps the smallest.
/// [`with_resolved_node`] therefore rejects such an ambiguous layer instead
/// of risking wrong-tree resolution (#350). A durable region identity is
/// still required to make those ids navigable; disjoint same-depth regions
/// (the norm) are unaffected.
pub(in crate::lsp::lsp_impl::kakehashi) fn walk_document_layers(
    coordinator: &LanguageCoordinator,
    host_language: &str,
    host_text: &str,
    host_tree: &tree_sitter::Tree,
    byte_filter: Option<&std::ops::Range<usize>>,
    cancel: Option<&crate::cancel::CancelToken>,
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
        cancel,
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
    cancel: Option<&crate::cancel::CancelToken>,
    visit: &mut dyn FnMut(&str, &tree_sitter::Tree, usize),
) {
    // Allows injected depths 1..=MAX_INJECTION_DEPTH — deliberately matching
    // `injection_stack_at` (`for _depth in 0..MAX` pushes up to MAX injected
    // layers), because minted node ids must resolve through that stack's
    // depth indexing. The semantic-tokens collector caps one layer shallower
    // (`depth >= MAX` with a different base); resolution does not depend on
    // it, so the cursor-path stack is the convention that matters here.
    if depth > MAX_INJECTION_DEPTH {
        return;
    }
    // Cancellation checkpoint before the per-depth injection query and the
    // per-region resolve+parse below — the walk's expensive units.
    if crate::cancel::is_cancelled(cancel) {
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
        // Per-region checkpoint: each iteration below runs a language
        // resolve + a full injected-region reparse, so on an
        // injection-heavy document a cancel landing mid-loop must not pay
        // for the remaining siblings before the per-depth check above sees
        // it on the next recursion.
        if crate::cancel::is_cancelled(cancel) {
            return;
        }
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
            cancel,
            visit,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stored layer trees must be exactly what the inline walk would
    /// visit over the same (text, tree): same (language, depth) sequence and
    /// same included ranges per layer — the equivalence the captures fast
    /// path relies on.
    #[test]
    fn collect_document_layer_trees_matches_the_inline_walk() {
        let coordinator = crate::language::LanguageCoordinator::new();
        let settings = crate::config::WorkspaceSettings {
            search_paths: vec![
                std::env::var("TREE_SITTER_GRAMMARS")
                    .unwrap_or_else(|_| "deps/tree-sitter".to_string()),
            ],
            ..Default::default()
        };
        let _ = coordinator.load_settings(&settings);
        for lang in ["markdown", "markdown_inline", "lua"] {
            if !coordinator.ensure_language_loaded(lang).success {
                eprintln!("Skipping: {lang} parser not available");
                return;
            }
        }

        let text = "# T\n\nsome *inline* text\n\n```lua\nlocal x = 1\n```\n";
        let mut pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = pool.acquire("markdown") else {
            return;
        };
        let tree = parser.parse(text, None).expect("parse markdown");
        pool.release("markdown".to_string(), parser);

        type LayerShape = (String, usize, Vec<(usize, usize)>);
        let mut inline: Vec<LayerShape> = Vec::new();
        walk_document_layers(
            &coordinator,
            "markdown",
            text,
            &tree,
            None,
            None,
            &mut |lang, layer_tree, depth| {
                if depth == 0 {
                    return;
                }
                inline.push((
                    lang.to_string(),
                    depth,
                    layer_tree
                        .included_ranges()
                        .iter()
                        .map(|r| (r.start_byte, r.end_byte))
                        .collect(),
                ));
            },
        );
        assert!(
            !inline.is_empty(),
            "sanity: the document has injected layers"
        );

        let stored = collect_document_layer_trees(&coordinator, "markdown", text, &tree);
        let stored_shape: Vec<LayerShape> = stored
            .iter()
            .map(|l| {
                (
                    l.language.clone(),
                    l.depth,
                    l.tree
                        .included_ranges()
                        .iter()
                        .map(|r| (r.start_byte, r.end_byte))
                        .collect(),
                )
            })
            .collect();
        assert_eq!(stored_shape, inline, "stored layers == inline walk");
    }

    #[test]
    fn build_effective_ranges_combines_offset_with_child_exclusion() {
        // #186: #offset! WITHOUT injection.include-children. The row offset
        // trims the first content line; blockquote `> ` prefixes
        // (block_continuation children) must still be excluded within the
        // remaining window.
        //
        // Byte map:
        //   "> ```lua\n"       0..9
        //   "> local a = 1\n"  9..23   (trimmed by the offset)
        //   "> local b = 2\n" 23..37   (prefix at 23..25)
        //   "> ```\n"         37..43
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&md_language).expect("set md language");
        let text = "> ```lua\n> local a = 1\n> local b = 2\n> ```\n";
        let tree = parser.parse(text, None).expect("parse markdown");

        let query = tree_sitter::Query::new(
            &md_language,
            r#"
            ((fenced_code_block
               (info_string (language) @injection.language)
               (code_fence_content) @injection.content)
              (#offset! @injection.content 1 0 0 0))
            "#,
        )
        .expect("valid offset injection query");

        let regions = collect_all_injections(&tree.root_node(), text, Some(&query))
            .expect("should find injections");
        assert_eq!(regions.len(), 1);
        assert!(
            !regions[0].include_children,
            "query must not set include-children for this scenario"
        );

        let ranges = build_effective_ranges(&regions[0], text, &query);

        let bytes: Vec<(usize, usize)> =
            ranges.iter().map(|r| (r.start_byte, r.end_byte)).collect();
        assert_eq!(
            bytes,
            vec![(25, 37)],
            "only the post-offset code (sans `> ` prefix) should remain"
        );
        assert_eq!(
            ranges[0].start_point,
            tree_sitter::Point { row: 2, column: 2 }
        );
        assert_eq!(
            ranges[0].end_point,
            tree_sitter::Point { row: 3, column: 0 }
        );
    }

    #[test]
    fn overlapping_same_depth_regions_do_not_resolve_an_ambiguous_node() {
        let query_root = tempfile::tempdir().expect("create query root");
        let query_dir = query_root.path().join("queries/markdown");
        std::fs::create_dir_all(&query_dir).expect("create markdown query dir");
        std::fs::write(
            query_dir.join("injections.scm"),
            r#"
            ((section) @injection.content
              (#set! injection.language "markdown")
              (#set! injection.include-children))
            ((paragraph) @injection.content
              (#set! injection.language "markdown")
              (#set! injection.include-children))
            "#,
        )
        .expect("write overlapping injection query");

        let grammar_root = std::env::var("TREE_SITTER_GRAMMARS")
            .unwrap_or_else(|_| "deps/test/kakehashi".to_string());
        let coordinator = LanguageCoordinator::new();
        let settings = crate::config::WorkspaceSettings {
            search_paths: vec![
                query_root.path().to_string_lossy().into_owned(),
                grammar_root,
            ],
            ..Default::default()
        };
        let _ = coordinator.load_settings(&settings);
        assert!(
            coordinator.ensure_language_loaded("markdown").success,
            "markdown parser fixture is required"
        );

        let text = "# heading\n\nambiguous paragraph\n";
        let mut pool = coordinator.create_document_parser_pool();
        let mut parser = pool.acquire("markdown").expect("acquire markdown parser");
        let tree = parser.parse(text, None).expect("parse markdown");
        pool.release("markdown".to_string(), parser);

        let mut paragraph = tree
            .root_node()
            .descendant_for_byte_range(text.find("ambiguous").unwrap(), text.len() - 1)
            .expect("paragraph node");
        while paragraph.kind() != "paragraph" {
            paragraph = paragraph.parent().expect("paragraph ancestor");
        }
        assert_eq!(paragraph.kind(), "paragraph");

        let stack = injection_stack_at(
            &coordinator,
            "markdown",
            text,
            &tree,
            paragraph.start_byte(),
        );
        assert!(
            stack.len() > 2,
            "recursive markdown fixture has a child layer"
        );
        assert!(stack[1].ambiguous, "the overlapping siblings are detected");
        assert!(
            stack[2].ambiguous,
            "an ambiguous ancestor taints its descendant layer"
        );

        let resolved = with_resolved_node(
            &coordinator,
            "markdown",
            text,
            &tree,
            paragraph.start_byte(),
            paragraph.end_byte(),
            paragraph.kind(),
            1,
            |node| (node.start_byte(), node.end_byte(), node.kind()),
        );

        assert!(
            matches!(resolved, NodeResolution::Ambiguous),
            "a depth-only id cannot prove which overlapping sibling minted it"
        );

        let pair = with_resolved_node_pair(
            &coordinator,
            "markdown",
            text,
            &tree,
            (
                paragraph.start_byte(),
                paragraph.end_byte(),
                paragraph.kind(),
            ),
            (
                paragraph.start_byte(),
                paragraph.end_byte(),
                paragraph.kind(),
            ),
            1,
            |_, _| (),
        );
        assert!(
            matches!(pair, NodeResolution::Ambiguous),
            "pair resolution must reject the same ambiguous layer"
        );

        let truncated = with_resolved_node(
            &coordinator,
            "markdown",
            text,
            &tree,
            paragraph.start_byte(),
            paragraph.end_byte(),
            paragraph.kind(),
            stack.len() + 1,
            |_| (),
        );
        assert!(
            matches!(truncated, NodeResolution::Ambiguous),
            "a truncated stack below an ambiguous ancestor is still ambiguous"
        );
    }
}
