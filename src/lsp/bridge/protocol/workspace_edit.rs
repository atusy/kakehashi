//! WorkspaceEdit coordinate transformation from virtual to host documents.
//!
//! Shared by the response paths that carry a `WorkspaceEdit` produced against
//! a virtual document: rename, codeAction, and workspace/applyEdit (#568).
//! Per LSP spec a WorkspaceEdit may carry edits via `changes` (URI→TextEdit
//! map) or `documentChanges`; both are handled.
//!
//! The guards below ([`workspace_edit_within_region`],
//! [`workspace_edit_preserves_line_prefixes`]) are enforced by codeAction,
//! rename, and applyEdit.

use std::collections::HashMap;

use tower_lsp_server::ls_types::{
    AnnotatedTextEdit, DocumentChangeOperation, DocumentChanges, OneOf, Position, ResourceOp,
    TextDocumentEdit, TextEdit, Uri, WorkspaceEdit,
};

use super::translation::{RegionOffset, translate_virtual_range_to_host};
use super::virtual_uri::VirtualDocumentUri;

/// Transform a WorkspaceEdit in place from virtual to host document coordinates.
///
/// For text edits: real-file URIs pass through, the request's own virtual URI
/// is re-keyed to the host URI (ranges translated, stale virtual-doc versions
/// dropped), and other (cross-region) virtual URIs are filtered out.
///
/// Returns `false` when the edit cannot be represented in host coordinates:
/// a file operation (create/rename/delete) references a virtual URI. The
/// spec applies `documentChanges` in order, so dropping just the op (e.g. a
/// virtual→real rename) would misdirect later edits at an unrelated existing
/// file — the caller must discard the whole edit instead.
pub(crate) fn transform_workspace_edit_to_host(
    edit: &mut WorkspaceEdit,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
) -> bool {
    if let Some(DocumentChanges::Operations(ops)) = &edit.document_changes {
        let has_virtual_file_op = ops.iter().any(|op| match op {
            DocumentChangeOperation::Op(op) => !resource_op_targets_real_files_only(op),
            DocumentChangeOperation::Edit(_) => false,
        });
        if has_virtual_file_op {
            return false;
        }
    }

    // Transform changes map: { [uri: string]: TextEdit[] }
    if let Some(changes) = &mut edit.changes {
        transform_changes_map(changes, request_virtual_uri, host_uri, offset);
    }

    // Transform documentChanges array
    if let Some(doc_changes) = &mut edit.document_changes {
        transform_document_changes(doc_changes, request_virtual_uri, host_uri, offset);
    }

    true
}

/// Whether a `WorkspaceEdit` contains at least one actual change.
///
/// Empty `changes` maps, empty edit vectors, and empty `documentChanges` are
/// no-ops and must not win preferred fan-in over another server or layer that
/// can return real edits. Resource operations count as changes.
pub(crate) fn workspace_edit_has_effect(edit: &WorkspaceEdit) -> bool {
    let changes_has_edit = edit
        .changes
        .as_ref()
        .is_some_and(|changes| changes.values().any(|edits| !edits.is_empty()));
    let doc_changes_has_edit = match &edit.document_changes {
        None => false,
        Some(DocumentChanges::Edits(edits)) => edits.iter().any(|e| !e.edits.is_empty()),
        Some(DocumentChanges::Operations(ops)) => ops.iter().any(|op| match op {
            DocumentChangeOperation::Edit(e) => !e.edits.is_empty(),
            DocumentChangeOperation::Op(_) => true,
        }),
    };
    changes_has_edit || doc_changes_has_edit
}

/// Transform the `changes` map in a WorkspaceEdit.
///
/// Re-keys virtual URIs to host URI and transforms TextEdit ranges.
/// Cross-region virtual URIs are removed entirely.
fn transform_changes_map(
    changes: &mut HashMap<Uri, Vec<TextEdit>>,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
) {
    // Filter and translate in one allocation-free pass, extracting the
    // request's own edits for re-keying afterwards (keys can't change in place).
    let mut rekeyed_edits: Option<Vec<TextEdit>> = None;
    changes.retain(|key, edits| {
        let uri_str = key.as_str();

        // Case 1: Real file URI → keep as-is
        if !VirtualDocumentUri::is_virtual_uri(uri_str) {
            return true;
        }

        // Case 2: Same virtual URI → transform ranges, re-key to host URI
        if uri_str == request_virtual_uri {
            for edit in edits.iter_mut() {
                translate_virtual_range_to_host(&mut edit.range, offset);
            }
            rekeyed_edits = Some(std::mem::take(edits));
            return false;
        }

        // Case 3: Different virtual URI (cross-region) → filter out
        false
    });

    if let Some(edits) = rekeyed_edits {
        // get_mut before insert: clone the host key only on the miss path.
        if let Some(existing) = changes.get_mut(host_uri) {
            existing.extend(edits);
        } else {
            changes.insert(host_uri.clone(), edits);
        }
    }
}

/// Transform the `documentChanges` array in a WorkspaceEdit.
///
/// Handles both `Edits(Vec<TextDocumentEdit>)` and
/// `Operations(Vec<DocumentChangeOperation>)` variants.
/// File operations (CreateFile, RenameFile, DeleteFile) are preserved as-is;
/// the caller has already rejected edits whose file ops reference virtual
/// URIs.
fn transform_document_changes(
    doc_changes: &mut DocumentChanges,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
) {
    match doc_changes {
        DocumentChanges::Edits(edits) => {
            edits.retain_mut(|edit| {
                transform_text_document_edit(edit, request_virtual_uri, host_uri, offset)
            });
        }
        DocumentChanges::Operations(ops) => {
            ops.retain_mut(|op| match op {
                DocumentChangeOperation::Edit(edit) => {
                    transform_text_document_edit(edit, request_virtual_uri, host_uri, offset)
                }
                DocumentChangeOperation::Op(_) => true, // Pre-validated real-file ops
            });
        }
    }
}

/// Whether a file operation references only real (non-virtual) URIs.
fn resource_op_targets_real_files_only(op: &ResourceOp) -> bool {
    match op {
        ResourceOp::Create(create) => !VirtualDocumentUri::is_virtual_uri(create.uri.as_str()),
        ResourceOp::Rename(rename) => {
            !VirtualDocumentUri::is_virtual_uri(rename.old_uri.as_str())
                && !VirtualDocumentUri::is_virtual_uri(rename.new_uri.as_str())
        }
        ResourceOp::Delete(delete) => !VirtualDocumentUri::is_virtual_uri(delete.uri.as_str()),
    }
}

/// Transform a single TextDocumentEdit's URI and edit ranges.
///
/// Returns `true` if the edit should be kept, `false` if it should be filtered out.
fn transform_text_document_edit(
    edit: &mut TextDocumentEdit,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
) -> bool {
    let uri_str = edit.text_document.uri.as_str();

    // Case 1: Real file URI → keep as-is
    if !VirtualDocumentUri::is_virtual_uri(uri_str) {
        return true;
    }

    // Case 2: Same virtual URI → transform
    if uri_str == request_virtual_uri {
        edit.text_document.uri = host_uri.clone();
        // The version counted the virtual document; against the host URI it
        // would make clients reject the edit as stale.
        edit.text_document.version = None;
        for one_of in &mut edit.edits {
            let text_edit = match one_of {
                OneOf::Left(text_edit) => text_edit,
                OneOf::Right(annotated_edit) => &mut annotated_edit.text_edit,
            };
            translate_virtual_range_to_host(&mut text_edit.range, offset);
        }
        return true;
    }

    // Case 3: Cross-region → filter out
    false
}

/// Null every `TextDocumentEdit.version` in an editor-ward `WorkspaceEdit`.
///
/// A bridged downstream's document versions live in the BRIDGE's version
/// space (each connection's didOpen starts its own counter at 1), not the
/// editor's. Relaying a versioned `documentChanges` edit verbatim makes
/// version-checking editors (e.g. Neovim's `apply_text_document_edit`) skip
/// the edit as stale. `version: null` means "apply without a version check"
/// per spec — the same treatment the virtual→host transform gives the
/// request's own document. Call at every editor-ward relay boundary
/// (applyEdit forward, host-layer rename/codeAction results).
///
/// Deliberately erase-without-validate: the version was never a working
/// cross-boundary protection (the spaces differ, so pre-strip it rejected
/// EVERYTHING), and the majority shape (`changes` map) carries no version at
/// all. What remains is COORDINATE validity on virtual paths (region
/// freshness + region bounds) — not general content freshness, and nothing
/// on host paths: an action whose edit aged in the editor's menu while the
/// user kept typing applies unconditionally, versioned shape or not. Mapping
/// downstream versions to editor versions (instead of erasing) is follow-up
/// hardening.
pub(crate) fn strip_bridge_local_versions(edit: &mut WorkspaceEdit) {
    let Some(doc_changes) = &mut edit.document_changes else {
        return;
    };
    let strip = |e: &mut TextDocumentEdit| e.text_document.version = None;
    match doc_changes {
        DocumentChanges::Edits(edits) => edits.iter_mut().for_each(strip),
        DocumentChanges::Operations(ops) => ops
            .iter_mut()
            .filter_map(|op| match op {
                DocumentChangeOperation::Edit(e) => Some(e),
                DocumentChangeOperation::Op(_) => None,
            })
            .for_each(strip),
    }
}

/// The exclusive host-document end of an injection region: the position just
/// past its `virtual_content`, mapped back to host coordinates — the
/// position-precise (line AND column) upper bound for
/// [`workspace_edit_within_region`].
pub(crate) fn region_host_end(virtual_content: &str, offset: &RegionOffset) -> Position {
    let mut end = crate::text::PositionMapper::new(virtual_content)
        .byte_to_position(virtual_content.len())
        .unwrap_or(Position {
            line: 0,
            character: 0,
        });
    super::translation::translate_virtual_position_to_host(&mut end, offset);
    end
}

/// Whether every text edit targeting `host_uri` in a HOST-coordinate
/// `WorkspaceEdit` is CONTAINED in the injection region — bounded above by
/// `region_end` and below, PER LINE, by the region's line offset (`offset`).
///
/// Call this AFTER `transform_workspace_edit_to_host`. Two escape vectors:
/// - a position PAST `region_end`: a stale/malformed downstream edit whose
///   virtual range runs beyond the (possibly shrunk) region content translates
///   to a plausible host range AFTER the fence — into unrelated host text.
/// - a position BELOW the region's per-line floor: virtual→host translation is
///   additive (`translate_virtual_position_to_host` adds the start line and the
///   line's column offset), so a *translated* position on virtual line `k`
///   lands at host column `>= column_for_line(k)` — the floor, with equality at
///   virtual column 0. But an edit already keyed to `host_uri` passes through
///   the transform verbatim (real-file URIs are kept), so a host-URI edit could
///   reach host text above the region OR into a line's prefix (e.g. a
///   blockquote `> ` before the injected content on any line). The per-line
///   floor rejects those and NEVER a legitimate translated edit.
///
/// Note: a single contiguous multi-line range inherently spans intermediate
/// lines' prefixes — that's equally true of a legitimate translated multi-line
/// edit, so only the range ENDPOINTS are floored, not every intermediate line.
///
/// Callers must REJECT, never clamp: applyEdit answers `applied: false`,
/// codeAction/resolve disables the action. Only `host_uri`'s edits are
/// region-bounded; edits to OTHER real files keep their own extents (a
/// downstream editing a genuinely different real file is a feature, not
/// corruption), and host-URI file operations (create/rename/delete) are out of
/// scope here — the transform already rejects *virtual*-URI file ops.
pub(crate) fn workspace_edit_within_region(
    edit: &WorkspaceEdit,
    host_uri: &Uri,
    offset: &RegionOffset,
    region_end: Position,
) -> bool {
    host_uri_text_edits_all(edit, host_uri, |e| {
        text_edit_within_region(e, offset, region_end)
    })
}

/// Whether a single already-host-translated text edit stays inside the region
/// (see [`workspace_edit_within_region`]). Response transforms use this to
/// reject a downstream range that — via a stale/malformed virtual range —
/// lands past the region's content end (the closing fence, or the rest of an
/// inline region's host line), which range translation alone cannot prevent.
pub(crate) fn text_edit_within_region(
    e: &TextEdit,
    offset: &RegionOffset,
    region_end: Position,
) -> bool {
    // A host position is in-region iff it's at/after the region's start line, at
    // /after that line's column floor, and at/before the region end.
    let in_region = |p: Position| {
        p.line >= offset.line() && {
            let virtual_line = p.line - offset.line();
            p.character >= offset.column_for_line(virtual_line) && !position_after(p, region_end)
        }
    };
    in_region(e.range.start) && in_region(e.range.end)
}

/// Combined per-edit safety check for response transforms: an
/// already-host-translated edit is safe iff it stays inside the region AND
/// keeps the region's per-line prefixes. Range translation guarantees the
/// start bound, but not the end: a stale/oversized downstream range still
/// translates to a host range that overruns the closing fence (or the rest of
/// an inline region's host line), so containment must be checked explicitly.
pub(crate) fn text_edit_safe_in_region(
    e: &TextEdit,
    offset: &RegionOffset,
    region_end: Position,
) -> bool {
    text_edit_within_region(e, offset, region_end)
        && text_edit_preserves_line_prefixes(e, offset, region_end)
}

/// Whether every text edit targeting `host_uri` keeps the region's per-line
/// host prefixes (e.g. a blockquote's `> ` on every line) intact.
///
/// The virtual→host transform translates RANGES but emits `newText` verbatim
/// — nothing re-inserts host line prefixes into replacement text. Two edit
/// shapes would therefore strip or omit prefixes and structurally corrupt the
/// host document (a broken blockquote un-fences the injected block):
/// - a MULTI-LINE range: the host range legitimately contains the interior/end
///   lines' prefixes, so replacing it deletes them. Unsafe iff any line after
///   the start line carries a non-zero prefix.
/// - a `newText` containing a NEWLINE: the inserted lines carry no prefix.
///   Unsafe iff the insertion point's neighborhood is prefixed (the start line
///   or the one after it has a non-zero prefix — a single-line inline/
///   blockquote region has no line after, so the start line's own offset is
///   the only signal there).
///
/// Plain fenced blocks (all-zero column offsets) are unaffected: every edit is
/// prefix-safe there, which keeps the common case (whole-block refactors like
/// organize-imports) working. In a prefixed per-line region, the closing-fence
/// BOUNDARY row (recorded as a trailing zero entry, or falling past the array)
/// counts as prefixed, and edits touching it are rejected outright — the row's
/// real prefix is unrecorded, so even a no-newline insertion at its column 0
/// would land before the `> `. Callers must REJECT unsafe edits, never clamp —
/// same contract as [`workspace_edit_within_region`]. Positions ABOVE the
/// region are this predicate's don't-care (the region bound rejects them).
pub(crate) fn workspace_edit_preserves_line_prefixes(
    edit: &WorkspaceEdit,
    host_uri: &Uri,
    offset: &RegionOffset,
    region_end: Position,
) -> bool {
    // In the per-line (blockquote) shape, content ending with a newline puts
    // the region end at column 0 of the NEXT host row — the closing fence,
    // whose recorded offset is 0 (`compute_line_column_offsets` leaves a
    // trailing zero for the row where the included ranges end) even though the
    // host line carries the `> ` prefix. That row is the prefixed BOUNDARY:
    // derived from the region end, not the array shape, because a trailing
    // zero entry can equally be a REAL unprefixed content row when an included
    // gap ends mid-row (region end at a non-zero character — no boundary row
    // is reachable then). Single-element arrays are the non-blockquote
    // fallback whose closing fence carries no prefix, so no boundary
    // semantics.
    //
    // The boundary row can also be the EOF row of an UNCLOSED blockquote fence
    // (no closing line exists); the offsets cannot distinguish that from the
    // closed-fence row, so the guard stays fail-closed there — rejecting a
    // boundary-touching edit in a transient malformed construct is acceptable,
    // corrupting a well-formed one is not.
    // No outer all-zero fast path: the per-edit predicate keeps its own for
    // the prefix rules, but its fence-boundary EOL rule must run even in
    // plain fenced regions (a boundary edit there can merge content into the
    // closing fence — "y```").
    host_uri_text_edits_all(edit, host_uri, |e| {
        text_edit_preserves_line_prefixes(e, offset, region_end)
    })
}

/// Per-edit form of [`workspace_edit_preserves_line_prefixes`], for response
/// shapes that carry bare `TextEdit`s rather than a `WorkspaceEdit`
/// (formatting-family responses, completion/inlayHint/colorPresentation item
/// edits). Same contract: `false` means applying the edit verbatim would
/// strip or omit the region's per-line host prefixes — callers must reject
/// or drop, never clamp.
pub(crate) fn text_edit_preserves_line_prefixes(
    e: &TextEdit,
    offset: &RegionOffset,
    region_end: Position,
) -> bool {
    // Fence-boundary EOL preservation — checked BEFORE the all-zero fast
    // path because it protects plain fenced regions too. When the region's
    // content end is the START of the closing-fence row (character 0 —
    // content ends with a newline), an edit reaching that boundary erases or
    // omits the newline separating content from fence: inserting "y" there
    // yields "y```" on the fence line. After the edit, the text before the
    // fence still ends with a newline iff the newText ends with one, or —
    // for a pure deletion — the range starts at a line start (whole trailing
    // lines removed; the preceding newline survives). Reject everything
    // else. The canonical insertFinalNewline ("\n" at the boundary) passes.
    if region_end.character == 0 && e.range.end == region_end {
        let ends_with_newline = e.new_text.ends_with('\n');
        let whole_line_deletion = e.new_text.is_empty() && e.range.start.character == 0;
        if !ends_with_newline && !whole_line_deletion {
            return false;
        }
    }

    let columns = offset.columns();
    if columns.iter().all(|&column| column == 0) {
        return true;
    }
    let content_prefixed = columns.len() > 1;
    let boundary_at = |host_line: u32| {
        content_prefixed && region_end.character == 0 && host_line >= region_end.line
    };
    let prefix_at = |host_line: u32| {
        boundary_at(host_line)
            || host_line
                .checked_sub(offset.line())
                .is_some_and(|v| offset.column_for_line(v) > 0)
    };
    {
        let (start, end) = (e.range.start, e.range.end);
        // The boundary row's real prefix is unrecorded (0), so the per-line
        // floor can't protect it: even a same-line, no-newline insertion at
        // (fence, 0) lands before the `> `. Reject anything touching it.
        let touches_boundary = boundary_at(start.line) || boundary_at(end.line);
        // end.line at character 0 still counts as spanned — deliberately
        // conservative (REJECT, never clamp): deleting up to (end, 0) removes
        // the previous line's newline and merges the prefixed line upward.
        //
        // The scan is CLAMPED to the explicit per-line array: every line past
        // it classifies via `boundary_at`, which is monotone in the line
        // number, so the range's largest line (`end.line`) decides for all of
        // them — the scan is O(columns.len()), never O(end.line), even for a
        // malformed edit claiming a huge range.
        let last_explicit = offset
            .line()
            .saturating_add((columns.len() as u32).saturating_sub(1));
        let spans_prefixed_line = end.line > start.line
            && ((start.line.saturating_add(1)..=end.line.min(last_explicit)).any(prefix_at)
                || boundary_at(end.line));
        // Single-element MULTI-LINE regions are exempt from the newline check:
        // later lines are designed unprefixed and line 0's region content runs
        // to end-of-line, so an inserted newline splits nothing. For a
        // SINGLE-LINE region (region end on the start line — e.g. inline code)
        // the host line continues past the region and a newline would split
        // it, so line 0's offset still rejects there.
        let newline_splits_nothing = columns.len() == 1 && region_end.line > offset.line();
        let inserts_unprefixed_line = !newline_splits_nothing
            && (e.new_text.contains('\n') || e.new_text.contains('\r'))
            && (prefix_at(start.line) || prefix_at(start.line.saturating_add(1)));
        !touches_boundary && !spans_prefixed_line && !inserts_unprefixed_line
    }
}

/// Whether every text edit targeting `host_uri` (across both the `changes` map
/// and `documentChanges`, unwrapping annotated edits) satisfies `pred`.
/// Edits to other URIs are ignored; file operations carry no text edit.
fn host_uri_text_edits_all(
    edit: &WorkspaceEdit,
    host_uri: &Uri,
    pred: impl Fn(&TextEdit) -> bool,
) -> bool {
    if let Some(changes) = &edit.changes
        && let Some(edits) = changes.get(host_uri)
        && !edits.iter().all(&pred)
    {
        return false;
    }
    let doc_edits_all = |edits: &[OneOf<TextEdit, AnnotatedTextEdit>]| {
        edits.iter().all(|one_of| {
            let text_edit = match one_of {
                OneOf::Left(text_edit) => text_edit,
                OneOf::Right(annotated) => &annotated.text_edit,
            };
            pred(text_edit)
        })
    };
    match &edit.document_changes {
        None => {}
        Some(DocumentChanges::Edits(edits)) => {
            for e in edits {
                if e.text_document.uri == *host_uri && !doc_edits_all(&e.edits) {
                    return false;
                }
            }
        }
        Some(DocumentChanges::Operations(ops)) => {
            for op in ops {
                if let DocumentChangeOperation::Edit(e) = op
                    && e.text_document.uri == *host_uri
                    && !doc_edits_all(&e.edits)
                {
                    return false;
                }
            }
        }
    }
    true
}

/// `a` is strictly after `b` in `(line, character)` order.
fn position_after(a: Position, b: Position) -> bool {
    (a.line, a.character) > (b.line, b.character)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_host_uri() -> Uri {
        crate::lsp::lsp_impl::url_to_uri(&url::Url::parse("file:///test.md").unwrap()).unwrap()
    }

    fn make_virtual_uri_string() -> String {
        VirtualDocumentUri::new(&make_host_uri(), "lua", "region-0").to_uri_string()
    }

    fn parse_workspace_edit(value: serde_json::Value) -> WorkspaceEdit {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn document_changes_operations_preserves_file_ops_and_transforms_edits() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        let mut edit = parse_workspace_edit(json!({
            "documentChanges": [
                { "kind": "create", "uri": "file:///new.lua" },
                {
                    "textDocument": { "uri": virtual_uri, "version": 3 },
                    "edits": [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 5 }
                        },
                        "newText": "newName"
                    }]
                }
            ]
        }));

        transform_workspace_edit_to_host(
            &mut edit,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
        );

        match edit.document_changes.unwrap() {
            DocumentChanges::Operations(ops) => {
                assert_eq!(ops.len(), 2, "file op and edit must both survive");
                assert!(
                    matches!(&ops[0], DocumentChangeOperation::Op(_)),
                    "file operation must pass through untouched"
                );
                match &ops[1] {
                    DocumentChangeOperation::Edit(edit) => {
                        assert_eq!(edit.text_document.uri, host_uri);
                        assert_eq!(edit.text_document.version, None);
                        match &edit.edits[0] {
                            OneOf::Left(text_edit) => {
                                assert_eq!(
                                    text_edit.range.start.line, 10,
                                    "range must be translated by the region offset (0 + 10)"
                                );
                            }
                            OneOf::Right(_) => panic!("Expected Left(TextEdit)"),
                        }
                    }
                    DocumentChangeOperation::Op(_) => panic!("Expected Edit operation"),
                }
            }
            DocumentChanges::Edits(_) => panic!("Expected Operations variant"),
        }
    }

    #[test]
    fn document_changes_rejects_whole_edit_on_virtual_uri_file_ops() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        // A file op against a virtual URI would leak the synthetic resource
        // to the client, and documentChanges apply IN ORDER — dropping just
        // the op (e.g. a virtual→real rename) would misdirect later edits at
        // an unrelated existing file. The whole edit must be rejected.
        let mut edit = parse_workspace_edit(json!({
            "documentChanges": [
                { "kind": "rename", "oldUri": virtual_uri, "newUri": "file:///renamed.lua" },
                {
                    "textDocument": { "uri": "file:///renamed.lua", "version": null },
                    "edits": [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 5 }
                        },
                        "newText": "dependsOnRename"
                    }]
                }
            ]
        }));

        let representable = transform_workspace_edit_to_host(
            &mut edit,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
        );

        assert!(
            !representable,
            "a virtual-URI file op must reject the whole WorkspaceEdit"
        );
    }

    #[rstest::rstest]
    #[case::create(json!({ "kind": "create", "uri": "kakehashi://virtual" }))]
    #[case::rename_new(
        json!({ "kind": "rename", "oldUri": "file:///a.lua", "newUri": "kakehashi://virtual" })
    )]
    #[case::delete(json!({ "kind": "delete", "uri": "kakehashi://virtual" }))]
    fn document_changes_rejects_each_virtual_file_op_kind(#[case] mut op: serde_json::Value) {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        // Substitute a well-formed virtual URI for the placeholder
        for key in ["uri", "oldUri", "newUri"] {
            if op.get(key).is_some_and(|v| v == "kakehashi://virtual") {
                op[key] = json!(virtual_uri.clone());
            }
        }
        let mut edit = parse_workspace_edit(json!({ "documentChanges": [op] }));

        let representable = transform_workspace_edit_to_host(
            &mut edit,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
        );

        assert!(!representable);
    }

    #[test]
    fn document_changes_translates_annotated_edits() {
        use tower_lsp_server::ls_types::{
            AnnotatedTextEdit, OptionalVersionedTextDocumentIdentifier, Position, Range,
        };

        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        // Constructed in code: serde's untagged OneOf parses annotated edits as
        // plain TextEdits (unknown fields are ignored), so responses
        // deserialized from JSON never reach Right and silently lose their
        // annotationId (an upstream ls-types limitation, tracked in #568
        // checklist item 7). This pins the arm for typed construction.
        let mut edit = WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: virtual_uri.parse().unwrap(),
                    version: Some(1),
                },
                edits: vec![OneOf::Right(AnnotatedTextEdit {
                    text_edit: TextEdit {
                        range: Range {
                            start: Position {
                                line: 2,
                                character: 0,
                            },
                            end: Position {
                                line: 2,
                                character: 5,
                            },
                        },
                        new_text: "newName".to_string(),
                    },
                    annotation_id: "refactor".to_string(),
                })],
            }])),
            ..Default::default()
        };

        transform_workspace_edit_to_host(
            &mut edit,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
        );

        match edit.document_changes.unwrap() {
            DocumentChanges::Edits(edits) => match &edits[0].edits[0] {
                OneOf::Right(annotated) => {
                    assert_eq!(annotated.text_edit.range.start.line, 12); // 2 + 10
                    assert_eq!(annotated.annotation_id, "refactor");
                }
                OneOf::Left(_) => panic!("Expected Right(AnnotatedTextEdit)"),
            },
            DocumentChanges::Operations(_) => panic!("Expected Edits variant"),
        }
    }

    #[test]
    fn document_changes_real_file_edit_preserved_untouched() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        let real_uri = "file:///usr/local/lib/types.lua";
        let mut edit = parse_workspace_edit(json!({
            "documentChanges": [{
                "textDocument": { "uri": real_uri, "version": 5 },
                "edits": [{
                    "range": {
                        "start": { "line": 50, "character": 0 },
                        "end": { "line": 50, "character": 5 }
                    },
                    "newText": "newName"
                }]
            }]
        }));

        transform_workspace_edit_to_host(
            &mut edit,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
        );

        match edit.document_changes.unwrap() {
            DocumentChanges::Edits(edits) => {
                assert_eq!(edits[0].text_document.uri.as_str(), real_uri);
                assert_eq!(
                    edits[0].text_document.version,
                    Some(5),
                    "real-file version must survive untouched"
                );
                match &edits[0].edits[0] {
                    OneOf::Left(text_edit) => {
                        assert_eq!(text_edit.range.start.line, 50, "real-file range untouched");
                    }
                    OneOf::Right(_) => panic!("Expected Left(TextEdit)"),
                }
            }
            DocumentChanges::Operations(_) => panic!("Expected Edits variant"),
        }
    }

    #[test]
    fn preserves_line_prefixes_rejects_multi_line_edit_spanning_prefixed_lines() {
        let host_uri = make_host_uri();
        // A 3-line BLOCKQUOTE region starting at host line 3: every line's
        // injected content sits behind a `> ` prefix (width 2).
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2, 2]);
        // A downstream multi-line replacement (host lines 3-5). The host range
        // legitimately contains lines 4 and 5's `> ` prefixes, but the verbatim
        // newText carries no prefixes — applying it would strip them and break
        // the blockquote.
        let edit = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 3, "character": 2}, "end": {"line": 5, "character": 6}},
                  "newText": "import a\nimport b" }
            ] }
        }));

        assert!(!workspace_edit_preserves_line_prefixes(
            &edit,
            &host_uri,
            &offset,
            Position {
                line: 5,
                character: 8,
            },
        ));
    }

    #[test]
    fn preserves_line_prefixes_rejects_newline_insertion_in_prefixed_region() {
        let host_uri = make_host_uri();
        // Single-line blockquote region: `> code` at host line 3, prefix width 2.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2]);
        // Inserting a newline splits the host line; the continuation line has
        // no `> ` prefix, so the blockquote breaks.
        let edit = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 4}},
                  "newText": "a\nb" }
            ] }
        }));

        // Single-LINE region: the region end sits on the start line.
        assert!(!workspace_edit_preserves_line_prefixes(
            &edit,
            &host_uri,
            &offset,
            Position {
                line: 3,
                character: 6,
            },
        ));
    }

    #[rstest::rstest]
    // The PRODUCTION blockquote shape: included ranges end at column 0 of the
    // closing-fence line, so `compute_line_column_offsets` leaves a trailing
    // EXPLICIT zero entry for that row — content on host lines 3-4, fence on
    // host line 5 recorded as offset 0 despite its real `> ` prefix.
    #[case::trailing_zero_entry(vec![2, 2, 0], 4, 5)]
    // Defensive shape: no trailing artifact (content ends mid-line); the
    // fence line falls PAST the array and must count as prefixed too.
    #[case::past_the_array(vec![2, 2, 2], 5, 6)]
    fn preserves_line_prefixes_rejects_edit_reaching_the_boundary_of_prefixed_region(
        #[case] columns: Vec<u32>,
        #[case] last_content_line: u32,
        #[case] fence_line: u32,
    ) {
        let host_uri = make_host_uri();
        let offset = RegionOffset::with_per_line_offsets(3, columns);
        // "Delete the last line": ends at (fence_line, 0), which the region
        // end admits. Applying removes the previous line's newline and merges
        // the PREFIXED fence line up into `> `, yielding `> > ```` —
        // structural corruption at the boundary row.
        let edit = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": last_content_line, "character": 2},
                            "end": {"line": fence_line, "character": 0}},
                  "newText": "" }
            ] }
        }));

        assert!(!workspace_edit_preserves_line_prefixes(
            &edit,
            &host_uri,
            &offset,
            Position {
                line: fence_line,
                character: 0,
            },
        ));
    }

    #[test]
    fn preserves_line_prefixes_rejects_plain_insertion_on_the_boundary_row() {
        let host_uri = make_host_uri();
        // Production blockquote shape: trailing zero entry = the closing-fence
        // row. A same-line, no-newline insertion at (5, 0) — which the region
        // end admits and the per-line floor (recorded 0) lets through — would
        // land BEFORE the fence line's `> `, producing `x> ````. No span, no
        // newline: only boundary awareness catches it.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]);
        let edit = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 5, "character": 0}, "end": {"line": 5, "character": 0}},
                  "newText": "x" }
            ] }
        }));

        assert!(!workspace_edit_preserves_line_prefixes(
            &edit,
            &host_uri,
            &offset,
            Position {
                line: 5,
                character: 0,
            },
        ));
    }

    #[test]
    fn preserves_line_prefixes_allows_newline_insertion_in_multi_line_single_element_region() {
        let host_uri = make_host_uri();
        // Production single-element shape over a MULTI-LINE region: later
        // lines are designed unprefixed, and line 0's region content runs to
        // end-of-line (the region continues on the next host line), so a
        // newline-bearing replacement starting on line 0 splits nothing and
        // its continuation lines correctly land at column 0.
        let offset = RegionOffset::new(3, 4);
        let region_end = Position {
            line: 5,
            character: 2,
        };
        let edit = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 6}},
                  "newText": "x\ny" }
            ] }
        }));

        assert!(workspace_edit_preserves_line_prefixes(
            &edit, &host_uri, &offset, region_end
        ));
    }

    #[test]
    fn preserves_line_prefixes_allows_edit_on_trailing_zero_content_row() {
        let host_uri = make_host_uri();
        // A trailing zero entry is NOT always the closing-fence boundary:
        // custom injection queries can produce an included gap that spans into
        // a real second row and ends MID-row, yielding [start_column, 0] where
        // row 1 is genuine unprefixed content. The region end then sits at a
        // non-zero character on that row — no boundary row is reachable, and
        // edits touching it must stay allowed.
        let offset = RegionOffset::with_per_line_offsets(3, vec![4, 0]);
        let region_end = Position {
            line: 4,
            character: 3,
        };
        let edit = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 4, "character": 0}, "end": {"line": 4, "character": 2}},
                  "newText": "x" }
            ] }
        }));

        assert!(workspace_edit_preserves_line_prefixes(
            &edit, &host_uri, &offset, region_end
        ));
    }

    #[test]
    fn preserves_line_prefixes_rejects_lone_cr_newline_insertion() {
        let host_uri = make_host_uri();
        let offset = RegionOffset::with_per_line_offsets(3, vec![2]);
        // LSP admits bare `\r` as an EOL: it inserts an unprefixed line just
        // like `\n` and must be caught by the same guard.
        let edit = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 4}},
                  "newText": "a\rb" }
            ] }
        }));

        // Single-LINE region, like the \n sibling above.
        assert!(!workspace_edit_preserves_line_prefixes(
            &edit,
            &host_uri,
            &offset,
            Position {
                line: 3,
                character: 6,
            },
        ));
    }

    #[test]
    fn preserves_line_prefixes_allows_edits_in_unprefixed_fence() {
        let host_uri = make_host_uri();
        // Plain fenced block: content starts at column 0 on every line.
        let offset = RegionOffset::new(3, 0);
        // Whole-block multi-line replacement with newlines — the
        // organize-imports shape. Nothing to corrupt; must stay allowed.
        let edit = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 3, "character": 0}, "end": {"line": 6, "character": 0}},
                  "newText": "import a\nimport b\n" }
            ] }
        }));

        assert!(workspace_edit_preserves_line_prefixes(
            &edit,
            &host_uri,
            &offset,
            Position {
                line: 6,
                character: 0,
            },
        ));
    }

    #[test]
    fn preserves_line_prefixes_allows_single_line_edit_in_prefixed_region() {
        let host_uri = make_host_uri();
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2]);
        // A rename-shaped single-line replacement behind the prefix: the
        // prefix is outside the range and no line structure changes.
        let edit = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 4, "character": 2}, "end": {"line": 4, "character": 7}},
                  "newText": "renamed" }
            ] }
        }));

        assert!(workspace_edit_preserves_line_prefixes(
            &edit,
            &host_uri,
            &offset,
            Position {
                line: 4,
                character: 9,
            },
        ));
    }

    #[test]
    fn preserves_line_prefixes_allows_multi_line_edit_over_unprefixed_tail() {
        let host_uri = make_host_uri();
        // The PRODUCTION non-blockquote shape (`vec![start_column]`): per the
        // RegionOffset contract, lines past the first are DESIGNED unprefixed
        // — a single-element array never means "blockquote", which always
        // carries one entry per content line plus the trailing boundary row.
        // A multi-line edit starting on line 0 never deletes a prefix: line
        // 0's offset sits before the range start, and the tail carries none.
        let offset = RegionOffset::new(3, 4);
        let edit = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 3, "character": 4}, "end": {"line": 5, "character": 2}},
                  "newText": "x" }
            ] }
        }));

        assert!(workspace_edit_preserves_line_prefixes(
            &edit,
            &host_uri,
            &offset,
            Position {
                line: 5,
                character: 2,
            },
        ));
    }

    #[test]
    fn preserves_line_prefixes_rejects_insertion_when_only_the_next_line_is_prefixed() {
        let host_uri = make_host_uri();
        // Start line unprefixed, the FOLLOWING line prefixed (lazy-continuation
        // first line): an inserted newline puts the new unprefixed line into a
        // prefixed neighborhood. Pins the second disjunct of the insertion
        // check — prefix_at(start.line) alone would let this through.
        let offset = RegionOffset::with_per_line_offsets(3, vec![0, 2]);
        let edit = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 3, "character": 1}, "end": {"line": 3, "character": 1}},
                  "newText": "a\nb" }
            ] }
        }));

        assert!(!workspace_edit_preserves_line_prefixes(
            &edit,
            &host_uri,
            &offset,
            Position {
                line: 4,
                character: 9,
            },
        ));
    }

    #[test]
    fn preserves_line_prefixes_rejects_span_when_only_the_end_line_is_prefixed() {
        let host_uri = make_host_uri();
        // Interior line unprefixed, END line prefixed: the range's tail eats
        // the end line's prefix. Pins the `..=end.line` inclusivity — an
        // exclusive range would miss it. Deletion-only (empty newText) shows
        // the span check is independent of the replacement text.
        let offset = RegionOffset::with_per_line_offsets(3, vec![0, 0, 2]);
        let edit = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 3, "character": 0}, "end": {"line": 5, "character": 1}},
                  "newText": "" }
            ] }
        }));

        assert!(!workspace_edit_preserves_line_prefixes(
            &edit,
            &host_uri,
            &offset,
            Position {
                line: 5,
                character: 9,
            },
        ));
    }

    #[test]
    fn preserves_line_prefixes_checks_annotated_document_changes_operations() {
        use tower_lsp_server::ls_types::{
            AnnotatedTextEdit, OptionalVersionedTextDocumentIdentifier, Range, TextDocumentEdit,
        };

        let host_uri = make_host_uri();
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2]);
        // Typed construction: serde's untagged OneOf never parses Right from
        // JSON (annotationId is silently dropped), so exercise the annotated
        // arm and the Operations arm of the shared walk in code. A newline-
        // bearing annotated edit in a prefixed region must be rejected.
        let edit = WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri: host_uri.clone(),
                        version: None,
                    },
                    edits: vec![OneOf::Right(AnnotatedTextEdit {
                        text_edit: TextEdit {
                            range: Range {
                                start: Position {
                                    line: 3,
                                    character: 2,
                                },
                                end: Position {
                                    line: 3,
                                    character: 2,
                                },
                            },
                            new_text: "a\nb".to_string(),
                        },
                        annotation_id: "refactor".to_string(),
                    })],
                }),
            ])),
            ..Default::default()
        };

        assert!(!workspace_edit_preserves_line_prefixes(
            &edit,
            &host_uri,
            &offset,
            Position {
                line: 4,
                character: 9,
            },
        ));
    }

    #[test]
    fn within_region_accepts_exact_boundary_positions() {
        let host_uri = make_host_uri();
        // Region: blockquote, host lines 3-4 behind a `> ` floor of 2,
        // region end at (4, 11). Both boundaries are INCLUSIVE: an edit
        // ending exactly at region_end (whole-region replacement — the
        // organize-imports shape) and one starting exactly at the column
        // floor must be accepted; `position_after` is strict and rejecting
        // equality would silently disable every full-region action.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2]);
        let region_end = Position {
            line: 4,
            character: 11,
        };
        let check = |edit: &WorkspaceEdit| {
            workspace_edit_within_region(edit, &host_uri, &offset, region_end)
        };

        let ends_at_region_end = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 3, "character": 2}, "end": {"line": 4, "character": 11}}, "newText": "x" }
            ] }
        }));
        assert!(check(&ends_at_region_end), "end == region_end must pass");

        let starts_at_floor = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 4, "character": 2}, "end": {"line": 4, "character": 5}}, "newText": "x" }
            ] }
        }));
        assert!(check(&starts_at_floor), "start == column floor must pass");
    }

    #[test]
    fn transform_translates_utf16_prefix_widths() {
        // Per-line offsets are UTF-16 code units by contract: the transform
        // adds the STORED width verbatim, so this test pins only that
        // addition (offset 2 → +2 columns). The byte-vs-UTF-16 distinction is
        // enforced where offsets are MINTED — see
        // compute_line_column_offsets_converts_non_ascii_prefix_to_utf16 in
        // language/injection/content.rs, whose fullwidth prefix would mint 4
        // if the width were byte-derived.
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2]);
        let mut edit = parse_workspace_edit(json!({
            "changes": { virtual_uri.clone(): [
                { "range": {"start": {"line": 1, "character": 4}, "end": {"line": 1, "character": 7}}, "newText": "x" }
            ] }
        }));

        transform_workspace_edit_to_host(&mut edit, &virtual_uri, &host_uri, &offset);

        let edits = edit.changes.unwrap().remove(&host_uri).unwrap();
        assert_eq!(edits[0].range.start.line, 4);
        assert_eq!(
            edits[0].range.start.character, 6,
            "virtual col 4 + UTF-16 prefix width 2 (NOT the 4-byte width)"
        );
        assert_eq!(edits[0].range.end.character, 9);
    }

    #[test]
    fn within_region_bounds_the_host_uri_edits_only() {
        let host_uri = make_host_uri();
        // A 2-line BLOCKQUOTE region: starts at host line 3, and every line has a
        // `> ` prefix (column floor 2). Region end is host (4, 11).
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2]);
        let region_end = Position {
            line: 4,
            character: 11,
        };
        let check = |edit: &WorkspaceEdit| {
            workspace_edit_within_region(edit, &host_uri, &offset, region_end)
        };

        // In-region: line 3 at/after the floor (col 2), end within region.
        let in_bounds = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 3, "character": 2}, "end": {"line": 3, "character": 5}}, "newText": "x" }
            ] }
        }));
        assert!(check(&in_bounds));

        // range.end PAST region_end line — escape after the fence. Rejected.
        let past_end_line = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 3, "character": 2}, "end": {"line": 8, "character": 0}}, "newText": "x" }
            ] }
        }));
        assert!(!check(&past_end_line));

        // Past the end COLUMN on the end line. Rejected.
        let past_end_col = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 3, "character": 2}, "end": {"line": 4, "character": 20}}, "newText": "x" }
            ] }
        }));
        assert!(!check(&past_end_col));

        // start ABOVE the region (line 0). Rejected.
        let before_start = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 0, "character": 0}, "end": {"line": 3, "character": 5}}, "newText": "x" }
            ] }
        }));
        assert!(!check(&before_start));

        // PER-LINE FLOOR: a pass-through host edit into the SECOND line's `> `
        // prefix (line 4, column 0 < that line's floor 2). A single line-0-derived
        // region_start would let this through (line 4 > start line 3); the
        // per-line floor rejects it. This is the blockquote-prefix escape.
        let into_line_prefix = parse_workspace_edit(json!({
            "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 4, "character": 0}, "end": {"line": 4, "character": 5}}, "newText": "x" }
            ] }
        }));
        assert!(!check(&into_line_prefix));

        // A REAL file (different URI) is not region-bounded — fine.
        let real_file = parse_workspace_edit(json!({
            "changes": { "file:///other.lua": [
                { "range": {"start": {"line": 99, "character": 0}, "end": {"line": 99, "character": 0}}, "newText": "x" }
            ] }
        }));
        assert!(check(&real_file));

        // documentChanges (Edits) host-URI edits are bounded the same way.
        let doc_changes_past_end = parse_workspace_edit(json!({
            "documentChanges": [{
                "textDocument": { "uri": host_uri.as_str(), "version": null },
                "edits": [
                    { "range": {"start": {"line": 3, "character": 2}, "end": {"line": 9, "character": 0}}, "newText": "x" }
                ]
            }]
        }));
        assert!(!check(&doc_changes_past_end));
        let doc_changes_into_prefix = parse_workspace_edit(json!({
            "documentChanges": [{
                "textDocument": { "uri": host_uri.as_str(), "version": null },
                "edits": [
                    { "range": {"start": {"line": 4, "character": 1}, "end": {"line": 4, "character": 3}}, "newText": "x" }
                ]
            }]
        }));
        assert!(!check(&doc_changes_into_prefix));
    }

    #[test]
    fn boundary_edits_must_preserve_the_fence_separating_newline() {
        // PLAIN fenced region (all-zero offsets): content host lines 3-4,
        // closing fence at line 5, region end (5, 0) — the boundary is the
        // fence row's start. Containment is inclusive there and the all-zero
        // fast path skips the prefix rules, so the EOL-preservation rule is
        // the only thing keeping "y" from landing as "y```".
        let offset = RegionOffset::with_per_line_offsets(3, vec![0, 0, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let edit = |start: Position, end: Position, new_text: &str| TextEdit {
            range: tower_lsp_server::ls_types::Range { start, end },
            new_text: new_text.to_string(),
        };
        let p = |line: u32, character: u32| Position { line, character };
        let check = |e: &TextEdit| text_edit_safe_in_region(e, &offset, region_end);

        // Insert without a trailing newline at the boundary → "y```". Reject.
        assert!(!check(&edit(p(5, 0), p(5, 0), "y")));
        // Canonical insertFinalNewline. Accept.
        assert!(check(&edit(p(5, 0), p(5, 0), "\n")));
        // Whole-block replacement ending at the boundary with a trailing
        // newline (organize-imports shape). Accept.
        assert!(check(&edit(p(3, 0), p(5, 0), "import a\nimport b\n")));
        // Same replacement WITHOUT the trailing newline → fence merges into
        // the last content line. Reject.
        assert!(!check(&edit(p(3, 0), p(5, 0), "import a\nimport b")));
        // Whole trailing-line deletion: preceding newline survives. Accept.
        assert!(check(&edit(p(4, 0), p(5, 0), "")));
        // Mid-line deletion ending at the boundary eats the separating
        // newline → "x```". Reject.
        assert!(!check(&edit(p(4, 2), p(5, 0), "")));
    }

    #[test]
    fn workspace_edit_boundary_rule_applies_to_plain_fenced_regions() {
        // Regression: the wrapper used to fast-path all-zero regions before
        // the per-edit predicate ran, so codeAction/rename/applyEdit edits
        // could still merge content into the closing fence ("y```"). The
        // fence-boundary EOL rule must fire through the WorkspaceEdit form.
        let host_uri = make_host_uri();
        let offset = RegionOffset::with_per_line_offsets(3, vec![0, 0, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let check = |new_text: &str| {
            let edit = parse_workspace_edit(json!({
                "changes": { host_uri.as_str(): [
                    { "range": {"start": {"line": 5, "character": 0},
                                "end": {"line": 5, "character": 0}},
                      "newText": new_text }
                ] }
            }));
            workspace_edit_preserves_line_prefixes(&edit, &host_uri, &offset, region_end)
        };

        assert!(!check("y"), "non-newline boundary insert must reject");
        assert!(check("\n"), "insertFinalNewline must pass");
    }
}
