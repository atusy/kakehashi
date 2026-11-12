//! WorkspaceEdit coordinate transformation from virtual to host documents.
//!
//! Shared by the response paths that carry a `WorkspaceEdit` produced against
//! a virtual document: rename, codeAction, and workspace/applyEdit (#568).
//! Per LSP spec a WorkspaceEdit may carry edits via `changes` (URI→TextEdit
//! map) or `documentChanges`; both are handled.
//!
//! codeAction, rename, and applyEdit all go through
//! [`transform_workspace_edit_into_region`], which also holds every edit of
//! the host document to the injection region: inside it
//! ([`workspace_edit_within_region`]) and keeping its lines' host prefixes
//! ([`workspace_edit_preserves_line_prefixes`],
//! [`place_text_edits_in_region`]).

use std::collections::HashMap;

use tower_lsp_server::ls_types::{
    AnnotatedTextEdit, DocumentChangeOperation, DocumentChanges, OneOf, Position, ResourceOp,
    TextDocumentEdit, TextEdit, Uri, WorkspaceEdit,
};

use super::translation::{RegionOffset, translate_virtual_range_to_host};
use super::virtual_uri::VirtualDocumentUri;
use crate::language::injection::LinePrefixes;

/// Transform a WorkspaceEdit in place from virtual to host document
/// coordinates, holding every edit of the host document to the injection
/// region.
///
/// For text edits: real-file URIs pass through, the request's own virtual URI
/// is re-keyed to the host URI (ranges translated, stale virtual-doc versions
/// dropped), and other (cross-region) virtual URIs are filtered out.
///
/// The request's own edits are placed in the region
/// ([`place_text_edits_in_region`]): one that adds or joins lines behind host
/// line prefixes is given those prefixes. That reads the edits against the
/// document as it stands, so it is done only for an edit that holds them in
/// one array; spread over several `TextDocumentEdit`s, which apply one after
/// another, they are only checked. So is an edit the server addressed to the
/// host document itself ([`workspace_edit_within_region`],
/// [`workspace_edit_preserves_line_prefixes`]): it is in host coordinates
/// already and relayed verbatim — the server wrote its text against the
/// host, and nothing tells whether that text carries the prefixes. Edits of
/// other real files keep their own extents.
///
/// On `Err` the edit may be half transformed: the caller must discard it
/// whole, never apply what is left. [`RegionEditRefusal::VirtualFileOperation`]
/// is a file operation (create/rename/delete) on a virtual URI: the spec
/// applies `documentChanges` in order, so dropping just the op (e.g. a
/// virtual→real rename) would misdirect later edits at an unrelated existing
/// file.
pub(crate) fn transform_workspace_edit_into_region(
    edit: &mut WorkspaceEdit,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
    region_end: Position,
) -> Result<(), RegionEditRefusal> {
    if let Some(DocumentChanges::Operations(ops)) = &edit.document_changes {
        let has_virtual_file_op = ops.iter().any(|op| match op {
            DocumentChangeOperation::Op(op) => !resource_op_targets_real_files_only(op),
            DocumentChangeOperation::Edit(_) => false,
        });
        if has_virtual_file_op {
            return Err(RegionEditRefusal::VirtualFileOperation);
        }
    }

    // Before the request's own edits are re-keyed to it, the host URI holds
    // only what the server addressed to the host document itself.
    if !workspace_edit_within_region(edit, host_uri, offset, region_end) {
        return Err(RegionEditRefusal::OutsideRegion);
    }
    if !workspace_edit_preserves_line_prefixes(edit, host_uri, offset, region_end) {
        return Err(RegionEditRefusal::BreaksLinePrefixes);
    }

    let one_array = arrays_of(edit, request_virtual_uri) == 1;
    let mut refusal = None;
    let mut place = |edits: &mut dyn Iterator<Item = &mut TextEdit>| {
        let mut edits: Vec<&mut TextEdit> = edits.collect();
        for text_edit in edits.iter_mut() {
            translate_virtual_range_to_host(&mut text_edit.range, offset);
        }
        if refusal.is_some() {
            return;
        }
        refusal = if one_array {
            place_text_edits_in_region(edits, offset, region_end).err()
        } else if !edits
            .iter()
            .all(|e| text_edit_within_region(e, offset, region_end))
        {
            Some(RegionEditRefusal::OutsideRegion)
        } else if !edits
            .iter()
            .all(|e| text_edit_preserves_line_prefixes(e, offset, region_end))
        {
            Some(RegionEditRefusal::BreaksLinePrefixes)
        } else {
            None
        };
    };

    // Transform changes map: { [uri: string]: TextEdit[] }
    if let Some(changes) = &mut edit.changes {
        transform_changes_map(changes, request_virtual_uri, host_uri, &mut place);
    }

    // Transform documentChanges array
    if let Some(doc_changes) = &mut edit.document_changes {
        transform_document_changes(doc_changes, request_virtual_uri, host_uri, &mut place);
    }

    refusal.map_or(Ok(()), Err)
}

/// How many arrays of text edits `edit` holds for `uri`: its entry in the
/// `changes` map, and each `TextDocumentEdit` naming it.
fn arrays_of(edit: &WorkspaceEdit, uri: &str) -> usize {
    let in_changes = edit
        .changes
        .as_ref()
        .is_some_and(|changes| changes.keys().any(|key| key.as_str() == uri));
    let names_it = |e: &TextDocumentEdit| e.text_document.uri.as_str() == uri;
    let in_document_changes = match &edit.document_changes {
        None => 0,
        Some(DocumentChanges::Edits(edits)) => edits.iter().filter(|e| names_it(e)).count(),
        Some(DocumentChanges::Operations(ops)) => ops
            .iter()
            .filter(|op| matches!(op, DocumentChangeOperation::Edit(e) if names_it(e)))
            .count(),
    };
    usize::from(in_changes) + in_document_changes
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

/// Brings one array of the request's own edits to the host document:
/// translates the ranges and holds the edits to the region.
type PlaceEdits<'a> = dyn FnMut(&mut dyn Iterator<Item = &mut TextEdit>) + 'a;

/// Transform the `changes` map in a WorkspaceEdit.
///
/// Re-keys virtual URIs to host URI and transforms TextEdit ranges.
/// Cross-region virtual URIs are removed entirely.
fn transform_changes_map(
    changes: &mut HashMap<Uri, Vec<TextEdit>>,
    request_virtual_uri: &str,
    host_uri: &Uri,
    place: &mut PlaceEdits<'_>,
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
            place(&mut edits.iter_mut());
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
    place: &mut PlaceEdits<'_>,
) {
    match doc_changes {
        DocumentChanges::Edits(edits) => {
            edits.retain_mut(|edit| {
                transform_text_document_edit(edit, request_virtual_uri, host_uri, place)
            });
        }
        DocumentChanges::Operations(ops) => {
            ops.retain_mut(|op| match op {
                DocumentChangeOperation::Edit(edit) => {
                    transform_text_document_edit(edit, request_virtual_uri, host_uri, place)
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
    place: &mut PlaceEdits<'_>,
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
        place(&mut edit.edits.iter_mut().map(|one_of| match one_of {
            OneOf::Left(text_edit) => text_edit,
            OneOf::Right(annotated_edit) => &mut annotated_edit.text_edit,
        }));
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
/// Deliberately erase-without-validate HERE: the version was never a working
/// cross-boundary protection (the spaces differ, so pre-strip it rejected
/// EVERYTHING), and the majority shape (`changes` map) carries no version at
/// all. On the applyEdit path, versioned VIRTUAL-document edits are validated
/// against the connection's tracked version BEFORE this strip runs (see
/// `ApplyEditTranslator::validate_virtual_document_versions`), so the erase
/// only ever launders a version the bridge has confirmed current. What
/// remains unvalidated is real-file versions (see the applyEdit translation
/// module docs for that decision) and the host-layer rename/codeAction relay
/// paths, where the guards are COORDINATE validity (region freshness + region
/// bounds), not content freshness: an action whose edit aged in the editor's
/// menu while the user kept typing applies unconditionally, versioned shape
/// or not. Mapping downstream versions to editor versions (instead of
/// erasing) is follow-up hardening.
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

/// An injection region's end-of-content in host coordinates: the position
/// one past the last character of its `virtual_content` — itself a valid LSP
/// position (the virtual document's EOF). Consumers compare against it
/// INCLUSIVELY (`<=`): the position-precise (line AND column) upper bound for
/// [`workspace_edit_within_region`] edit containment and for
/// position-request dispatch (`host_position_within_region_bounds`).
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
/// Call this on host-coordinate edits ([`transform_workspace_edit_into_region`]
/// does, for the edits a server addressed to the host document itself). Two
/// escape vectors:
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
    // A REVERSED range (start after end) is malformed; some clients would
    // normalize it by swapping, turning it into an edit the checks below (and
    // the prefix/boundary rules, which assume start <= end) never inspected —
    // reject it outright.
    if position_after(e.range.start, e.range.end) {
        return false;
    }
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
/// preserves its structure (per-line prefixes and the newline separating
/// content from the closing fence). Range translation guarantees the
/// start bound, but not the end: a stale/oversized downstream range still
/// translates to a host range that overruns the closing fence (or the rest of
/// an inline region's host line), so containment must be checked explicitly.
///
/// This only checks. It is for edits the editor does not apply as written —
/// a completion item's own insertion, whose later lines the editor indents —
/// where an edit that breaks a prefix cannot be rewritten into one that
/// keeps it. Edits applied as written go through
/// [`place_text_edits_in_region`] instead.
pub(crate) fn text_edit_safe_in_region(
    e: &TextEdit,
    offset: &RegionOffset,
    region_end: Position,
) -> bool {
    text_edit_within_region(e, offset, region_end)
        && text_edit_preserves_line_prefixes(e, offset, region_end)
}

/// Why an edit produced against a virtual document — a `WorkspaceEdit`, or a
/// bare array of text edits — cannot be handed to the editor as an edit of
/// the host document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegionEditRefusal {
    /// A file operation (create/rename/delete) references a virtual URI.
    VirtualFileOperation,
    /// An edit of the host document reaches outside the injection region
    /// ([`text_edit_within_region`]).
    OutsideRegion,
    /// An edit of the host document would break the host's structure around
    /// the region — its lines' prefixes or the line break ahead of the
    /// closing fence — and cannot be given what it lacks
    /// ([`place_text_edits_in_region`]).
    BreaksLinePrefixes,
}

/// [`text_edit_safe_in_region`] for a set of edits whose text the editor
/// applies verbatim — a formatting answer, an `additionalTextEdits` array,
/// the edits a `WorkspaceEdit` holds for the document: an edit that is inside
/// the region but would break its lines' host prefixes is given those
/// prefixes instead of being refused, when the offset knows what they say
/// ([`RegionOffset::line_prefixes`]).
///
/// `edits` are already translated to host coordinates, and are all read
/// against the document as it stands, as LSP reads the edits of one array.
/// On `Ok` they are safe to apply as they now are — new texts, and where
/// [`keep_line_prefixes`] says so a range's bound, may have been rewritten.
/// On `Err` the set must be dropped whole, never applied in part; edits may
/// have been rewritten by then.
///
/// Each edit gives the lines it adds their prefixes without counting on what
/// another edit does to those lines, with two exceptions: an edit that
/// completes a blank line's prefix, and one that takes a line's prefix out,
/// both count on that prefix being in the host as it is now. Such an edit
/// must be the only one of the set on its line.
pub(crate) fn place_text_edits_in_region<'a>(
    edits: impl IntoIterator<Item = &'a mut TextEdit>,
    offset: &RegionOffset,
    region_end: Position,
) -> Result<(), RegionEditRefusal> {
    // The lines whose prefix an edit counts on, and how many edits are on
    // each line any edit starts or ends on — both stay empty until an edit
    // needs its prefixes kept.
    let mut needs_its_line = Vec::new();
    let mut ranges = Vec::new();
    for e in edits {
        if !text_edit_within_region(e, offset, region_end) {
            return Err(RegionEditRefusal::OutsideRegion);
        }
        let touched = (e.range.start.line, e.range.end.line);
        if !text_edit_preserves_line_prefixes(e, offset, region_end) {
            let kept = offset
                .line_prefixes()
                .and_then(|prefixes| keep_line_prefixes(e, offset, prefixes, region_end))
                .ok_or(RegionEditRefusal::BreaksLinePrefixes)?;
            if kept == Kept::CountingOnItsLine {
                needs_its_line.push(touched.0);
            }
        }
        ranges.push(touched);
    }
    if needs_its_line.is_empty() {
        return Ok(());
    }
    let mut edits_on_line: HashMap<u32, usize> = HashMap::new();
    for (start_line, end_line) in ranges {
        *edits_on_line.entry(start_line).or_default() += 1;
        if end_line != start_line {
            *edits_on_line.entry(end_line).or_default() += 1;
        }
    }
    if needs_its_line
        .iter()
        .any(|line| edits_on_line.get(line).copied().unwrap_or(0) > 1)
    {
        return Err(RegionEditRefusal::BreaksLinePrefixes);
    }
    Ok(())
}

/// What [`keep_line_prefixes`] made of an edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kept {
    /// The edit stands on its own.
    Alone,
    /// The edit counts on the prefix of the line it starts on being in the
    /// host as it is now: no other edit of its set may be on that line.
    CountingOnItsLine,
}

/// Rewrite a host-translated edit that adds, joins or removes lines of a
/// line-prefixed region so that every line it leaves behind has its prefix.
/// `None` when it cannot be done; the edit is then untouched.
///
/// The range translation already put both ends behind their lines' prefixes,
/// so the host range holds the prefixes of every line after the first one it
/// spans, and they go with it. What the new text must bring back:
///
/// - **a line it adds** takes the region's prefix — the prefix of every line
///   of text. An added line left empty takes that prefix without its
///   trailing whitespace, as the host writes its own blank lines (`>` in a
///   blockquote, nothing in a list item).
/// - **the line the range ends on** takes the region's prefix like an added
///   line — unless the new text ends with a line break right at that line's
///   start. The line is then left exactly as it was: the range is drawn back
///   to the line's first column, ahead of the prefix it has.
/// - **the line the range starts on** keeps its prefix in the host. If that
///   is a blank line's shorter prefix and the edit puts text there, the edit
///   must start at the line's start, and brings the rest of the region's
///   prefix with it.
///
/// The line after the region's final line break — the closing fence's — is
/// not the region's to write on: the host has its own text there, behind a
/// prefix of its own. An edit reaching it must leave a line break as the last
/// thing before it. Lines it adds right there take the region's prefix from
/// their first character on, the position being ahead of any prefix. An edit
/// that removes whole lines up to it takes the first line's prefix out as
/// well: its range is drawn out to that line's first column.
fn keep_line_prefixes(
    e: &mut TextEdit,
    offset: &RegionOffset,
    prefixes: &LinePrefixes,
    region_end: Position,
) -> Option<Kept> {
    let uniform = prefixes.uniform();
    let blank = uniform.trim_end();
    let lead = |text: &str| if text.is_empty() { blank } else { uniform };

    let (start, end) = (e.range.start, e.range.end);
    let start_line = start.line.checked_sub(offset.line())?;
    let end_line = end.line.checked_sub(offset.line())?;
    // The region's bound keeps both ends at or before `region_end`, so a
    // position on that line is that position.
    let after_region = |position: Position| region_end.character == 0 && position == region_end;
    let at_line_start =
        |position: Position, line: u32| position.character == offset.column_for_line(line);

    let lines = split_lines(&e.new_text);
    let (last_text, _) = lines[lines.len() - 1];
    let mut new_text = String::with_capacity(e.new_text.len() + lines.len() * uniform.len());

    if after_region(start) {
        // Lines added between the region's last line and the host's own.
        if lines.len() == 1 || !last_text.is_empty() {
            return None;
        }
        for (text, line_break) in &lines[..lines.len() - 1] {
            new_text.push_str(lead(text));
            new_text.push_str(text);
            new_text.push_str(line_break);
        }
        e.new_text = new_text;
        return Some(Kept::Alone);
    }

    let mut kept = Kept::Alone;
    let (first_text, first_break) = lines[0];
    let own = prefixes.of_line(start_line);
    // A blank line's own prefix, with text about to stand behind it? When the
    // edit joins lines, the text may be what the range's last line keeps: a
    // blank line's is none, any other line's is not known here and counts.
    let joins_text = lines.len() == 1
        && end_line != start_line
        && !after_region(end)
        && prefixes.is_uniform_at(end_line);
    if !prefixes.is_uniform_at(start_line) && (!first_text.is_empty() || joins_text) {
        let missing = uniform.strip_prefix(own)?;
        if !at_line_start(start, start_line) {
            return None;
        }
        new_text.push_str(missing);
        kept = Kept::CountingOnItsLine;
    }

    if lines.len() == 1 {
        if after_region(end) {
            // Nothing but whole lines may go from here to the region's end,
            // and their first prefix goes with them — if it is a prefix: a
            // region opening after a delimiter on its line has the host's
            // own text there.
            if !first_text.is_empty()
                || !at_line_start(start, start_line)
                || !uniform.starts_with(own)
            {
                return None;
            }
            e.range.start.character = 0;
            return Some(Kept::CountingOnItsLine);
        }
        new_text.push_str(first_text);
        e.new_text = new_text;
        return Some(kept);
    }

    new_text.push_str(first_text);
    new_text.push_str(first_break);
    for (text, line_break) in &lines[1..lines.len() - 1] {
        new_text.push_str(lead(text));
        new_text.push_str(text);
        new_text.push_str(line_break);
    }
    if after_region(end) {
        if !last_text.is_empty() {
            return None;
        }
    } else if last_text.is_empty() && at_line_start(end, end_line) {
        if end_line == start_line {
            // The range is empty, at this line's start: the line's prefix
            // stands ahead of it and now opens the first added line. What
            // was behind it needs the same prefix again.
            new_text.push_str(own);
        } else {
            e.range.end.character = 0;
        }
    } else {
        new_text.push_str(uniform);
        new_text.push_str(last_text);
    }
    e.new_text = new_text;
    Some(kept)
}

/// `text` as its lines, each with the line break that ends it: LF, CRLF, or
/// a lone CR (an LSP line ends at any of them). Never empty: the last entry
/// is what follows the final line break, with no break of its own.
fn split_lines(text: &str) -> Vec<(&str, &str)> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut line_start = 0;
    let mut at = 0;
    while at < bytes.len() {
        let break_len = match bytes[at] {
            b'\n' => 1,
            b'\r' if bytes.get(at + 1) == Some(&b'\n') => 2,
            b'\r' => 1,
            _ => {
                at += 1;
                continue;
            }
        };
        lines.push((&text[line_start..at], &text[at..at + break_len]));
        at += break_len;
        line_start = at;
    }
    lines.push((&text[line_start..], ""));
    lines
}

/// Whether every text edit targeting `host_uri` keeps the region's per-line
/// host prefixes (e.g. a blockquote's `> ` on every line) intact, and — in
/// every region shape — preserves the newline separating content from the
/// closing fence (the fence-boundary EOL rule).
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
/// Plain fenced blocks (all-zero column offsets) are exempt from the prefix
/// rules — every edit is prefix-safe there, which keeps the common case
/// (whole-block refactors like organize-imports) working — but the
/// fence-boundary EOL rule still applies to edits reaching a character-0
/// region end. Known limitation: a CUSTOM injection query (`#offset!`) could
/// capture a column-0 same-line range with a host suffix after it; the
/// offsets cannot represent that shape (it is indistinguishable from a
/// one-line fenced block, where rejecting newlines would break
/// insertFinalNewline), and the built-in queries never produce it.
///
/// In a prefixed per-line region, the closing-fence
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
/// strip or omit the region's per-line host prefixes, or merge content into
/// the closing fence (the fence-boundary EOL rule) — callers must reject
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
        let ends_with_newline = e.new_text.ends_with('\n') || e.new_text.ends_with('\r');
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

    /// The transform with a region that bounds nothing: `false` only for a
    /// file operation on a virtual URI.
    fn transform_workspace_edit_to_host(
        edit: &mut WorkspaceEdit,
        request_virtual_uri: &str,
        host_uri: &Uri,
        offset: &RegionOffset,
    ) -> bool {
        let unbounded = Position {
            line: u32::MAX,
            character: u32::MAX,
        };
        transform_workspace_edit_into_region(edit, request_virtual_uri, host_uri, offset, unbounded)
            != Err(RegionEditRefusal::VirtualFileOperation)
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

    #[test]
    fn reversed_ranges_are_rejected_in_every_region_shape() {
        // A reversed range (start after end) is malformed, and a client that
        // normalizes it by swapping would apply an edit none of the rules
        // inspected (the boundary rule keys on range.end; the multi-line scan
        // assumes start <= end). Containment must reject it outright.
        let p = |line: u32, character: u32| Position { line, character };
        let edit = |start: Position, end: Position, new_text: &str| TextEdit {
            range: tower_lsp_server::ls_types::Range { start, end },
            new_text: new_text.to_string(),
        };
        let region_end = Position {
            line: 5,
            character: 0,
        };

        // Plain fence: reversed {start: region_end, end: earlier} would
        // normalize to a replacement ending at the fence without a newline.
        let plain = RegionOffset::with_per_line_offsets(3, vec![0, 0, 0]);
        assert!(!text_edit_safe_in_region(
            &edit(p(5, 0), p(4, 0), "x"),
            &plain,
            region_end
        ));

        // Prefixed region: reversed multi-line span would normalize to a
        // prefix-stripping replacement.
        let prefixed = RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]);
        assert!(!text_edit_safe_in_region(
            &edit(p(4, 4), p(3, 2), "x"),
            &prefixed,
            region_end
        ));
    }

    #[test]
    fn boundary_deletion_accepts_a_lone_cr_terminator() {
        // LSP recognizes lone '\r' as an EOL; a replacement ending with it
        // still separates content from the closing fence.
        let offset = RegionOffset::with_per_line_offsets(3, vec![0, 0, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let e = TextEdit {
            range: tower_lsp_server::ls_types::Range {
                start: Position {
                    line: 3,
                    character: 0,
                },
                end: region_end,
            },
            new_text: "replaced\r".to_string(),
        };
        assert!(text_edit_safe_in_region(&e, &offset, region_end));
    }

    // ---------------------------------------------------------------------
    // place_text_edits_in_region
    // ---------------------------------------------------------------------

    /// A line-prefixed region as the host holds it: an opening line, the
    /// region's lines behind their prefixes, and — when the region ends with
    /// a line break — the host's own closing line behind the region's prefix.
    struct PrefixedRegion {
        host: String,
        offset: RegionOffset,
        region_end: Position,
    }

    /// `rows` are the region's lines as (host prefix, content). `closed`
    /// says whether the last one ends with a line break, after which the
    /// host's closing fence follows.
    fn prefixed_region(uniform: &str, rows: &[(&str, &str)], closed: bool) -> PrefixedRegion {
        let mut host = format!("{uniform}```lua\n");
        let mut virtual_content = String::new();
        let mut columns = Vec::new();
        let mut blank_lines = Vec::new();
        for (line, (prefix, content)) in rows.iter().enumerate() {
            let last = line + 1 == rows.len();
            let line_break = if last && !closed { "" } else { "\n" };
            host.push_str(&format!("{prefix}{content}{line_break}"));
            virtual_content.push_str(&format!("{content}{line_break}"));
            columns.push(prefix.encode_utf16().count() as u32);
            if *prefix != uniform {
                blank_lines.push((line as u32, *prefix));
            }
        }
        if closed {
            host.push_str(&format!("{uniform}```\n"));
            columns.push(0);
        }
        let offset = RegionOffset::with_per_line_offsets(1, columns).with_line_prefixes(Some(
            std::sync::Arc::new(LinePrefixes::for_test(uniform, &blank_lines)),
        ));
        let region_end = region_host_end(&virtual_content, &offset);
        PrefixedRegion {
            host,
            offset,
            region_end,
        }
    }

    /// A blockquoted fence holding `a` and `b`.
    fn quoted_ab() -> PrefixedRegion {
        prefixed_region("> ", &[("> ", "a"), ("> ", "b")], true)
    }

    fn text_edit(start: (u32, u32), end: (u32, u32), new_text: &str) -> TextEdit {
        TextEdit {
            range: tower_lsp_server::ls_types::Range {
                start: Position {
                    line: start.0,
                    character: start.1,
                },
                end: Position {
                    line: end.0,
                    character: end.1,
                },
            },
            new_text: new_text.to_string(),
        }
    }

    /// `text` with `edits` applied, all read against `text` as it stands
    /// (ASCII only: a UTF-16 column is a byte).
    fn apply(text: &str, edits: &[TextEdit]) -> String {
        let at = |position: Position| {
            let mut offset = 0;
            for (line, content) in text.split_inclusive('\n').enumerate() {
                if line as u32 == position.line {
                    return offset + position.character as usize;
                }
                offset += content.len();
            }
            offset + position.character as usize
        };
        let mut spans: Vec<_> = edits
            .iter()
            .enumerate()
            .map(|(index, edit)| (at(edit.range.start), at(edit.range.end), index))
            .collect();
        // Insertions at one position apply in the order they were given.
        spans.sort_by_key(|(start, end, index)| (*start, *end, *index));
        let mut out = String::new();
        let mut copied = 0;
        for (start, end, index) in spans {
            assert!(start >= copied && end >= start, "overlapping edits");
            out.push_str(&text[copied..start]);
            out.push_str(&edits[index].new_text);
            copied = end;
        }
        out.push_str(&text[copied..]);
        out
    }

    /// The host after the given edits of the VIRTUAL document, translated
    /// and placed as a response transform does; `None` when one is refused.
    fn host_after(region: &PrefixedRegion, virtual_edits: &[TextEdit]) -> Option<String> {
        let mut edits = virtual_edits.to_vec();
        for edit in &mut edits {
            translate_virtual_range_to_host(&mut edit.range, &region.offset);
        }
        place_text_edits_in_region(&mut edits, &region.offset, region.region_end).ok()?;
        Some(apply(&region.host, &edits))
    }

    #[test]
    fn placing_gives_each_added_line_the_region_prefix() {
        let region = quoted_ab();

        // A line above the first one, one between the two, one after them.
        assert_eq!(
            host_after(&region, &[text_edit((0, 0), (0, 0), "x\n")]).as_deref(),
            Some("> ```lua\n> x\n> a\n> b\n> ```\n")
        );
        assert_eq!(
            host_after(&region, &[text_edit((0, 1), (0, 1), "\nx")]).as_deref(),
            Some("> ```lua\n> a\n> x\n> b\n> ```\n")
        );
        assert_eq!(
            host_after(&region, &[text_edit((2, 0), (2, 0), "x\ny\n")]).as_deref(),
            Some("> ```lua\n> a\n> b\n> x\n> y\n> ```\n")
        );
        // A line split in two.
        assert_eq!(
            host_after(
                &prefixed_region("  ", &[("  ", "ab")], true),
                &[text_edit((0, 1), (0, 1), "\n")]
            )
            .as_deref(),
            Some("  ```lua\n  a\n  b\n  ```\n")
        );
    }

    #[test]
    fn placing_keeps_the_prefixes_of_the_lines_a_replacement_spans() {
        let region = quoted_ab();

        // The whole document, as a formatter replaces it.
        assert_eq!(
            host_after(&region, &[text_edit((0, 0), (2, 0), "A\nB\nC\n")]).as_deref(),
            Some("> ```lua\n> A\n> B\n> C\n> ```\n")
        );
        // From the middle of one line into the next.
        assert_eq!(
            host_after(&region, &[text_edit((0, 1), (1, 1), "1\n2\n3")]).as_deref(),
            Some("> ```lua\n> a1\n> 2\n> 3\n> ```\n")
        );
        // Two lines joined: the second one's prefix goes with the line break.
        assert_eq!(
            host_after(&region, &[text_edit((0, 1), (1, 0), " ")]).as_deref(),
            Some("> ```lua\n> a b\n> ```\n")
        );
    }

    #[test]
    fn placing_writes_an_added_blank_line_as_the_host_writes_its_own() {
        // `>` in a blockquote, nothing in a list item.
        assert_eq!(
            host_after(&quoted_ab(), &[text_edit((0, 1), (0, 1), "\n\nx")]).as_deref(),
            Some("> ```lua\n> a\n>\n> x\n> b\n> ```\n")
        );
        assert_eq!(
            host_after(
                &prefixed_region("  ", &[("  ", "a"), ("  ", "b")], true),
                &[text_edit((0, 1), (0, 1), "\n\nx")]
            )
            .as_deref(),
            Some("  ```lua\n  a\n\n  x\n  b\n  ```\n")
        );
        // The line break a formatter adds at the end of the document.
        assert_eq!(
            host_after(&quoted_ab(), &[text_edit((2, 0), (2, 0), "\n")]).as_deref(),
            Some("> ```lua\n> a\n> b\n>\n> ```\n")
        );
    }

    #[test]
    fn placing_leaves_the_line_after_the_new_text_as_it_was() {
        // The new text ends with a line break right where the blank line
        // starts: that line keeps the bare `>` it had.
        let region = prefixed_region("> ", &[("> ", "a"), (">", ""), ("> ", "b")], true);

        assert_eq!(
            host_after(&region, &[text_edit((0, 0), (1, 0), "A\nx\n")]).as_deref(),
            Some("> ```lua\n> A\n> x\n>\n> b\n> ```\n")
        );
    }

    #[test]
    fn placing_completes_a_blank_lines_prefix_when_text_lands_on_it() {
        let region = prefixed_region("> ", &[("> ", "a"), (">", ""), ("> ", "b")], true);

        // Text on the blank line itself, and on the line after it.
        assert_eq!(
            host_after(&region, &[text_edit((1, 0), (1, 0), "x\ny")]).as_deref(),
            Some("> ```lua\n> a\n> x\n> y\n> b\n> ```\n")
        );
        // The blank line joined with the line of text below it.
        assert_eq!(
            host_after(&region, &[text_edit((1, 0), (2, 0), "")]).as_deref(),
            Some("> ```lua\n> a\n> b\n> ```\n")
        );
        // Still blank afterwards: nothing to complete.
        let two_blanks = prefixed_region("> ", &[("> ", "a"), (">", ""), (">", "")], true);
        assert_eq!(
            host_after(&two_blanks, &[text_edit((1, 0), (2, 0), "")]).as_deref(),
            Some("> ```lua\n> a\n>\n> ```\n")
        );
    }

    #[test]
    fn placing_never_writes_onto_the_line_after_the_region() {
        let region = quoted_ab();

        // Text there would stand ahead of the closing fence's own prefix...
        assert_eq!(host_after(&region, &[text_edit((2, 0), (2, 0), "x")]), None);
        assert_eq!(
            host_after(&region, &[text_edit((2, 0), (2, 0), "x\ny")]),
            None
        );
        // ... and a replacement that drops the final line break would pull
        // the fence up onto the last line.
        assert_eq!(
            host_after(&region, &[text_edit((0, 0), (2, 0), "A\nB")]),
            None
        );
        assert_eq!(host_after(&region, &[text_edit((1, 1), (2, 0), "")]), None);
    }

    #[test]
    fn placing_removes_whole_last_lines_with_their_first_prefix() {
        // Otherwise that prefix would be left standing ahead of the fence's.
        assert_eq!(
            host_after(&quoted_ab(), &[text_edit((1, 0), (2, 0), "")]).as_deref(),
            Some("> ```lua\n> a\n> ```\n")
        );
        let blank_tail = prefixed_region("> ", &[("> ", "a"), (">", ""), (">", "")], true);
        assert_eq!(
            host_after(&blank_tail, &[text_edit((1, 0), (3, 0), "")]).as_deref(),
            Some("> ```lua\n> a\n> ```\n")
        );
    }

    #[test]
    fn placing_reaches_the_end_of_a_region_the_host_closes_nothing_after() {
        // No final line break: the last line is the region's own.
        let region = prefixed_region("> ", &[("> ", "a"), ("> ", "b")], false);

        assert_eq!(
            host_after(&region, &[text_edit((1, 1), (1, 1), "\nc")]).as_deref(),
            Some("> ```lua\n> a\n> b\n> c")
        );
    }

    #[test]
    fn placing_keeps_the_line_breaks_the_new_text_uses() {
        assert_eq!(
            host_after(&quoted_ab(), &[text_edit((0, 0), (1, 0), "x\r\ny\rz\n")]).as_deref(),
            Some("> ```lua\n> x\r\n> y\r> z\n> b\n> ```\n")
        );
    }

    #[test]
    fn placing_reads_each_edit_of_a_set_against_the_document_as_it_was() {
        // Two lines inserted at one position, and a third edit starting on
        // the line the first two end at.
        let region = quoted_ab();

        assert_eq!(
            host_after(
                &region,
                &[
                    text_edit((1, 0), (1, 0), "x\n"),
                    text_edit((1, 0), (1, 0), "y\n"),
                    text_edit((1, 1), (2, 0), "!\n"),
                ]
            )
            .as_deref(),
            Some("> ```lua\n> a\n> x\n> y\n> b!\n> ```\n")
        );
    }

    #[test]
    fn placing_refuses_what_it_cannot_prefix() {
        let region = quoted_ab();
        let adds_a_line = text_edit((0, 0), (0, 0), "x\n");

        // The offset does not know what the prefixes say (it crossed the
        // wire, or the region has no single prefix).
        let widths_only = PrefixedRegion {
            offset: RegionOffset::with_per_line_offsets(1, vec![2, 2, 0]),
            host: region.host.clone(),
            region_end: region.region_end,
        };
        assert_eq!(
            host_after(&widths_only, std::slice::from_ref(&adds_a_line)),
            None
        );

        // A region opening after a delimiter on its line: what stands ahead
        // of that line's content is the host's text, not a prefix to remove
        // or to complete.
        let after_delimiter =
            prefixed_region("  ", &[("x = ''", ""), ("  ", "a"), ("  ", "b")], true);
        assert_eq!(
            host_after(&after_delimiter, &[text_edit((0, 0), (3, 0), "")]),
            None
        );
        assert_eq!(
            host_after(&after_delimiter, &[text_edit((0, 0), (1, 0), "c")]),
            None
        );
        // Lines below it are placed as anywhere else.
        assert_eq!(
            host_after(&after_delimiter, &[text_edit((1, 1), (1, 1), "\nc")]).as_deref(),
            Some("  ```lua\nx = ''\n  a\n  c\n  b\n  ```\n")
        );

        // An edit outside the region stays refused.
        assert_eq!(
            host_after(&region, &[text_edit((0, 0), (3, 0), "x\n")]),
            None
        );
    }

    #[test]
    fn placing_says_why_it_refuses() {
        let region = quoted_ab();
        let place = |start, end, new_text| {
            let mut edit = text_edit(start, end, new_text);
            translate_virtual_range_to_host(&mut edit.range, &region.offset);
            place_text_edits_in_region([&mut edit], &region.offset, region.region_end)
        };

        assert_eq!(
            place((0, 0), (3, 0), "x\n"),
            Err(RegionEditRefusal::OutsideRegion)
        );
        assert_eq!(
            place((2, 0), (2, 0), "x"),
            Err(RegionEditRefusal::BreaksLinePrefixes)
        );
    }

    #[test]
    fn placing_refuses_a_second_edit_on_a_line_whose_prefix_one_counts_on() {
        // Each of two texts landing on one blank line would complete its
        // prefix: the second completion would end up in the content.
        let blank = prefixed_region("> ", &[("> ", "a"), (">", ""), ("> ", "b")], true);
        assert_eq!(
            host_after(
                &blank,
                &[
                    text_edit((1, 0), (1, 0), "x\ny"),
                    text_edit((1, 0), (1, 0), "z\nw")
                ]
            ),
            None
        );

        // Removing the last line with its prefix counts on the line break
        // ahead of it; an edit that joins that line to the one above takes
        // the break away.
        assert_eq!(
            host_after(
                &quoted_ab(),
                &[
                    text_edit((0, 1), (1, 0), "c"),
                    text_edit((1, 0), (2, 0), "")
                ]
            ),
            None
        );
    }

    #[test]
    fn placing_leaves_an_edit_that_breaks_no_prefix_untouched() {
        let region = quoted_ab();
        let mut edit = text_edit((1, 2), (1, 3), "x");
        let before = edit.clone();

        assert_eq!(
            place_text_edits_in_region([&mut edit], &region.offset, region.region_end),
            Ok(())
        );
        assert_eq!(edit, before);

        // A fence without prefixes takes line breaks as they come.
        let plain = RegionOffset::with_per_line_offsets(1, vec![0, 0, 0]);
        let mut edit = text_edit((1, 0), (2, 0), "x\ny\n");
        let before = edit.clone();
        let region_end = Position {
            line: 3,
            character: 0,
        };
        assert_eq!(
            place_text_edits_in_region([&mut edit], &plain, region_end),
            Ok(())
        );
        assert_eq!(edit, before);
    }

    // ---------------------------------------------------------------------
    // transform_workspace_edit_into_region
    // ---------------------------------------------------------------------

    /// The new texts `edit` ends up holding for the host document, in order.
    fn host_new_texts(edit: &WorkspaceEdit, host_uri: &Uri) -> Vec<String> {
        let mut texts = Vec::new();
        if let Some(edits) = edit
            .changes
            .as_ref()
            .and_then(|changes| changes.get(host_uri))
        {
            texts.extend(edits.iter().map(|e| e.new_text.clone()));
        }
        if let Some(DocumentChanges::Edits(document_edits)) = &edit.document_changes {
            for document_edit in document_edits {
                assert_eq!(&document_edit.text_document.uri, host_uri);
                texts.extend(document_edit.edits.iter().map(|one_of| match one_of {
                    OneOf::Left(text_edit) => text_edit.new_text.clone(),
                    OneOf::Right(annotated) => annotated.text_edit.new_text.clone(),
                }));
            }
        }
        texts
    }

    /// A line added above the document's first one, in virtual coordinates.
    fn adds_a_line() -> serde_json::Value {
        json!({ "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                "newText": "x\n" })
    }

    #[test]
    fn transform_into_region_gives_the_requests_own_edits_the_line_prefixes() {
        let region = quoted_ab();
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        let shapes = [
            json!({ "changes": { virtual_uri.clone(): [adds_a_line()] } }),
            json!({ "documentChanges": [
                { "textDocument": { "uri": virtual_uri, "version": 1 }, "edits": [adds_a_line()] }
            ] }),
            json!({ "documentChanges": [
                { "textDocument": { "uri": virtual_uri, "version": 1 }, "edits": [{
                    "range": adds_a_line()["range"], "newText": "x\n", "annotationId": "a"
                }] }
            ] }),
        ];

        for shape in shapes {
            let mut edit = parse_workspace_edit(shape.clone());
            assert_eq!(
                transform_workspace_edit_into_region(
                    &mut edit,
                    &virtual_uri,
                    &host_uri,
                    &region.offset,
                    region.region_end,
                ),
                Ok(()),
                "{shape}"
            );
            assert_eq!(host_new_texts(&edit, &host_uri), ["x\n> "], "{shape}");
        }
    }

    #[test]
    fn transform_into_region_only_checks_edits_spread_over_several_arrays() {
        // `TextDocumentEdit`s apply one after another: the second one's
        // positions are in a document the first has changed, where this
        // offset's lines no longer tell which prefix is where.
        let region = quoted_ab();
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        let two_arrays = |second: serde_json::Value| {
            let mut edit = parse_workspace_edit(json!({ "documentChanges": [
                { "textDocument": { "uri": virtual_uri, "version": 1 }, "edits": [
                    { "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                      "newText": "A" }
                ] },
                { "textDocument": { "uri": virtual_uri, "version": 2 }, "edits": [second] }
            ] }));
            transform_workspace_edit_into_region(
                &mut edit,
                &virtual_uri,
                &host_uri,
                &region.offset,
                region.region_end,
            )
            .map(|()| host_new_texts(&edit, &host_uri))
        };

        assert_eq!(
            two_arrays(adds_a_line()),
            Err(RegionEditRefusal::BreaksLinePrefixes)
        );
        assert_eq!(
            two_arrays(json!({
                "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}},
                "newText": "B"
            })),
            Ok(vec!["A".to_string(), "B".to_string()])
        );
    }

    #[test]
    fn transform_into_region_only_checks_edits_addressed_to_the_host_document() {
        // Written against the host, in host coordinates: whether its text
        // already carries the prefixes is not ours to tell, so it is relayed
        // as it came or not at all.
        let region = quoted_ab();
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        let host_edit = |new_text: &str| {
            let mut edit = parse_workspace_edit(json!({ "changes": { host_uri.as_str(): [
                { "range": {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 3}},
                  "newText": new_text }
            ] } }));
            transform_workspace_edit_into_region(
                &mut edit,
                &virtual_uri,
                &host_uri,
                &region.offset,
                region.region_end,
            )
            .map(|()| host_new_texts(&edit, &host_uri))
        };

        assert_eq!(host_edit("A"), Ok(vec!["A".to_string()]));
        assert_eq!(
            host_edit("A\n> x"),
            Err(RegionEditRefusal::BreaksLinePrefixes)
        );
    }

    #[test]
    fn transform_into_region_says_why_it_refuses() {
        let region = quoted_ab();
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        let refusal = |value: serde_json::Value| {
            transform_workspace_edit_into_region(
                &mut parse_workspace_edit(value),
                &virtual_uri,
                &host_uri,
                &region.offset,
                region.region_end,
            )
        };

        assert_eq!(
            refusal(json!({ "documentChanges": [
                { "kind": "create", "uri": virtual_uri }
            ] })),
            Err(RegionEditRefusal::VirtualFileOperation)
        );
        assert_eq!(
            refusal(json!({ "changes": { virtual_uri.clone(): [
                { "range": {"start": {"line": 0, "character": 0}, "end": {"line": 7, "character": 0}},
                  "newText": "x" }
            ] } })),
            Err(RegionEditRefusal::OutsideRegion)
        );
        assert_eq!(
            refusal(json!({ "changes": { virtual_uri.clone(): [
                { "range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 0}},
                  "newText": "x" }
            ] } })),
            Err(RegionEditRefusal::BreaksLinePrefixes)
        );
    }

    mod round_trip {
        //! The property the placement exists for, checked against the real
        //! markdown grammar: applying the placed edits to the host and
        //! reading the fence's content back gives what applying the same
        //! edits to the virtual document gives — with the document around
        //! the fence untouched.

        use super::*;
        use crate::language::injection::InjectionResolver;
        use crate::language::{LanguageCoordinator, NodeTracker};
        use proptest::prelude::*;

        const QUERY: &str = r#"
            (fenced_code_block
              (info_string (language) @injection.language)
              (code_fence_content) @injection.content)
        "#;

        /// The first fence of `host`, as discovery resolves it.
        fn resolve(host: &str) -> Option<crate::language::injection::ResolvedInjection> {
            let language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&language).expect("markdown grammar");
            let tree = parser.parse(host, None).expect("parse markdown");
            let query = tree_sitter::Query::new(&language, QUERY).expect("valid query");
            let mut resolved = InjectionResolver::resolve_all(
                &LanguageCoordinator::new(),
                &NodeTracker::new(),
                &url::Url::parse("file:///round_trip.md").expect("valid url"),
                &tree,
                host,
                &query,
                0,
            );
            (resolved.len() == 1).then(|| resolved.remove(0))
        }

        /// How a fence is nested: what stands above it, the prefix of its
        /// lines of text, and the forms its blank lines take.
        #[derive(Debug, Clone)]
        struct Container {
            above: &'static str,
            prefix: &'static str,
            blank: &'static [&'static str],
        }

        const CONTAINERS: &[Container] = &[
            Container {
                above: "",
                prefix: "> ",
                blank: &[">", "> "],
            },
            Container {
                above: "- item\n\n",
                prefix: "  ",
                blank: &["", "  "],
            },
            Container {
                above: "1.  item\n\n",
                prefix: "    ",
                blank: &["", "    "],
            },
            Container {
                above: "- outer\n  - inner\n\n",
                prefix: "    ",
                blank: &["", "    "],
            },
            Container {
                above: "> - item\n>\n",
                prefix: ">   ",
                blank: &[">", ">   "],
            },
        ];

        /// A fence's lines as written: `Ok` a line of text, `Err` a blank
        /// line in the container's n-th form.
        type Line = Result<&'static str, usize>;

        /// A nested fence holding at least one line of text.
        fn document() -> impl Strategy<Value = (Container, Vec<Line>)> {
            let line = prop_oneof![
                3 => prop_oneof![
                    Just("a"),
                    Just("b = 1"),
                    Just("  indented"),
                    Just("\tx"),
                    Just("if y then")
                ]
                .prop_map(Ok),
                1 => (0usize..2).prop_map(Err),
            ];
            (
                proptest::sample::select(CONTAINERS),
                proptest::collection::vec(line, 1..6),
            )
                .prop_filter("a line of text to read the prefix from", |(_, lines)| {
                    lines.iter().any(Result::is_ok)
                })
        }

        fn host_of(container: &Container, lines: &[Line]) -> String {
            let mut host = format!("# T\n\n{}{}```lua\n", container.above, container.prefix);
            for line in lines {
                match line {
                    Ok(content) => host.push_str(&format!("{}{content}\n", container.prefix)),
                    Err(form) => {
                        host.push_str(container.blank[form % container.blank.len()]);
                        host.push('\n');
                    }
                }
            }
            host.push_str(&format!("{}```\n\nafter\n", container.prefix));
            host
        }

        /// Up to three edits of a document of the given lines, none
        /// overlapping another: each is (start, end, new text) with the ends
        /// drawn as indexes into the document's positions.
        fn edits() -> impl Strategy<Value = Vec<(prop::sample::Index, prop::sample::Index, String)>>
        {
            let piece = prop_oneof![
                Just("x"),
                Just("  y"),
                Just("\n"),
                Just("\n"),
                Just(" "),
                Just("z = 2")
            ];
            let new_text =
                proptest::collection::vec(piece, 0..5).prop_map(|pieces| pieces.concat());
            proptest::collection::vec(
                (
                    any::<prop::sample::Index>(),
                    any::<prop::sample::Index>(),
                    new_text,
                ),
                1..4,
            )
        }

        /// Every position of `text`, in order.
        fn positions(text: &str) -> Vec<Position> {
            let mut all = Vec::new();
            for (line, content) in text.split('\n').enumerate() {
                for character in 0..=content.len() {
                    all.push(Position {
                        line: line as u32,
                        character: character as u32,
                    });
                }
            }
            all
        }

        proptest! {
            #![proptest_config(ProptestConfig {
                cases: 3000,
                failure_persistence: None,
                ..ProptestConfig::default()
            })]

            #[test]
            fn placed_edits_read_back_as_the_edited_virtual_document(
                (container, lines) in document(),
                drawn in edits(),
            ) {
                let host = host_of(&container, &lines);
                let resolved = resolve(&host).expect("the fence resolves");
                let virtual_content = resolved.virtual_content.clone();
                let offset = RegionOffset::with_per_line_offsets(
                    resolved.region.line_range.start,
                    resolved.line_column_offsets.clone(),
                )
                .with_line_prefixes(resolved.line_prefixes.clone());
                prop_assert!(
                    offset.line_prefixes().is_some(),
                    "a nested fence has one prefix: {host:?}"
                );
                let region_end = region_host_end(&virtual_content, &offset);

                // Non-overlapping edits, in document order.
                let all = positions(&virtual_content);
                let mut bounds: Vec<usize> = drawn
                    .iter()
                    .flat_map(|(start, end, _)| [start.index(all.len()), end.index(all.len())])
                    .collect();
                bounds.sort_unstable();
                let virtual_edits: Vec<TextEdit> = drawn
                    .iter()
                    .enumerate()
                    .map(|(index, (_, _, new_text))| TextEdit {
                        range: tower_lsp_server::ls_types::Range {
                            start: all[bounds[2 * index]],
                            end: all[bounds[2 * index + 1]],
                        },
                        new_text: new_text.clone(),
                    })
                    .collect();

                let mut host_edits = virtual_edits.clone();
                for edit in &mut host_edits {
                    translate_virtual_range_to_host(&mut edit.range, &offset);
                }
                let translated = host_edits.clone();
                // A refusal is always allowed: the set is then dropped.
                prop_assume!(
                    place_text_edits_in_region(&mut host_edits, &offset, region_end).is_ok()
                );
                // Out of this property's reach: an edit the prefix guard
                // lets through as it came although it writes on a blank
                // line, whose prefix is shorter than the region's (#1172,
                // #1175). Nothing was placed there.
                let prefixes = offset.line_prefixes().expect("checked above");
                let on_a_blank_line = |edit: &TextEdit| {
                    [edit.range.start.line, edit.range.end.line]
                        .iter()
                        .any(|line| !prefixes.is_uniform_at(line - offset.line()))
                };
                prop_assume!(!translated.iter().zip(&host_edits).any(|(before, after)| {
                    before == after
                        && on_a_blank_line(before)
                        && !(before.new_text.is_empty() && before.range.start == before.range.end)
                }));

                let expected = apply(&virtual_content, &virtual_edits);
                let edited_host = apply(&host, &host_edits);
                // A fence left without a line has no content to resolve.
                let read_back = resolve(&edited_host)
                    .map(|resolved| resolved.virtual_content)
                    .unwrap_or_default();
                prop_assert_eq!(
                    read_back,
                    expected,
                    "host {:?} became {:?} by {:?}",
                    host,
                    edited_host,
                    host_edits
                );
                let above = format!("# T\n\n{}{}```lua\n", container.above, container.prefix);
                let below = format!("{}```\n\nafter\n", container.prefix);
                prop_assert!(
                    edited_host.starts_with(&above) && edited_host.ends_with(&below),
                    "the document around the fence changed: {edited_host:?}"
                );
            }
        }
    }
}
