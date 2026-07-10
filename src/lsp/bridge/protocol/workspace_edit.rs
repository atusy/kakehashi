//! WorkspaceEdit coordinate transformation from virtual to host documents.
//!
//! Shared by response paths that carry a `WorkspaceEdit` produced against a
//! virtual document — rename today; codeAction and workspace/applyEdit will
//! reuse it (#568). Per LSP spec a WorkspaceEdit may carry edits via `changes`
//! (URI→TextEdit map) or `documentChanges`; both are handled.

use std::collections::HashMap;

use tower_lsp_server::ls_types::{
    AnnotatedTextEdit, DocumentChangeOperation, DocumentChanges, OneOf, Position, Range,
    ResourceOp, TextDocumentEdit, TextEdit, Uri, WorkspaceEdit,
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
    // A host position is in-region iff it's at/after the region's start line, at
    // /after that line's column floor, and at/before the region end.
    let in_region = |p: Position| {
        p.line >= offset.line() && {
            let virtual_line = p.line - offset.line();
            p.character >= offset.column_for_line(virtual_line) && !position_after(p, region_end)
        }
    };
    let within = |range: Range| in_region(range.start) && in_region(range.end);
    if let Some(changes) = &edit.changes
        && let Some(edits) = changes.get(host_uri)
        && !edits.iter().all(|e| within(e.range))
    {
        return false;
    }
    let doc_edits_within = |edits: &[OneOf<TextEdit, AnnotatedTextEdit>]| {
        edits.iter().all(|one_of| {
            let range = match one_of {
                OneOf::Left(text_edit) => text_edit.range,
                OneOf::Right(annotated) => annotated.text_edit.range,
            };
            within(range)
        })
    };
    match &edit.document_changes {
        None => {}
        Some(DocumentChanges::Edits(edits)) => {
            for e in edits {
                if e.text_document.uri == *host_uri && !doc_edits_within(&e.edits) {
                    return false;
                }
            }
        }
        Some(DocumentChanges::Operations(ops)) => {
            for op in ops {
                if let DocumentChangeOperation::Edit(e) = op
                    && e.text_document.uri == *host_uri
                    && !doc_edits_within(&e.edits)
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
}
