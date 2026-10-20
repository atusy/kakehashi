//! Completion request handling for bridge connections.
//!
//! Handles the coordinate transformation between host and virtual documents,
//! and drops items whose primary insertion (text edit, or the insertText/
//! label fallback at the request position) is unsafe for the injection region —
//! escape it, break per-line `> ` prefixes, or merge content into the closing
//! fence — if applied verbatim (see `transform_completion_item`).
//! On a prepared document the items' edits map back through its map,
//! re-indented; a one-line snippet maps as its expansion and goes out with
//! the indent its expansion regained before it (a literal one as plain
//! text), and an
//! item without an edit is made an explicit one where its text at the caret
//! regains an indent, or dropped where it would map off the caret.
//! Messages are queued via the channel-based writer task (ls-bridge-message-ordering) for FIFO
//! ordering with other messages.

use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::settings::BridgeServerConfig;
use crate::lsp::bridge::envelope::{ENVELOPE_KEY, should_envelope, wrap_envelope};
use tower_lsp_server::ls_types::{CompletionItem, CompletionList, Position};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, build_position_based_request,
    host_position_in_removed_indent, response_has_jsonrpc_error, text_edit_safe_in_region,
    translate_virtual_main_and_additional_edits_to_host, translate_virtual_range_to_host,
};
use super::keep_additional_edits_if_safe;
use super::snippet::Expansion;
use tower_lsp_server::ls_types::TextDocumentPositionParams;

impl LanguageServerPool {
    /// Send a completion request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle) for the
    /// full lifecycle, providing completion-specific request building and response
    /// transformation.
    ///
    /// Content synchronization (didChange) is handled by the notification pipeline
    /// in `forward_didchange_to_bridges`, which runs during `did_change()` processing
    /// before any subsequent request is handled.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_completion_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        host_position: Position,
        region_end: Position,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<CompletionList>> {
        // Inside indentation or blank lines the prepare peer removed, the
        // server sees the caret elsewhere (the line's content, the
        // document's start, the content's end): its items would replace text
        // away from the caret the client asked at, which LSP requires an
        // edit's range to contain, and one without an edit would be inserted
        // there outside any mapping.
        if host_position_in_removed_indent(host_position, &offset) {
            return Ok(None);
        }
        let host_incarnation = self.current_host_incarnation(host_uri);
        let handle = self
            .get_or_create_virtual_connection(
                server_name,
                server_config,
                host_uri,
                injection_language,
                region_id,
            )
            .await?;
        if !handle.has_capability("textDocument/completion") {
            return Ok(None);
        }
        // Read resolve support when the RESPONSE arrives, as the host layer
        // does: a dynamic `resolveProvider` registration can land between
        // send and reply, and the envelope decision should see it.
        let origin = std::sync::Arc::clone(&handle);
        // Read only for a prepared document, the only one it bears on.
        let insert_mode = if offset.prepared().is_some() {
            ClientInsertMode::from_capabilities(self.client_capabilities_ref())
        } else {
            ClientInsertMode::default()
        };
        self.execute_position_bridge_request_with_handle(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            host_position,
            region_end,
            "textDocument/completion",
            |virtual_uri, request_id| {
                build_completion_request(virtual_uri, host_position, &offset, request_id)
            },
            |response, ctx| {
                transform_completion_response_to_host(
                    response,
                    ctx.offset,
                    region_end,
                    Some(host_position),
                    origin.has_capability("completionItem/resolve"),
                    &EnvelopeContext {
                        server_name,
                        injection_language,
                        incarnation: host_incarnation,
                        host_uri: host_uri.as_str(),
                        region_id,
                        offset: ctx.offset,
                        region_end: Some(region_end),
                        host_layer: false,
                    },
                    insert_mode,
                )
            },
        )
        .await
    }
}

/// Build a JSON-RPC completion request for a downstream language server.
fn build_completion_request(
    virtual_uri: &VirtualDocumentUri,
    host_position: tower_lsp_server::ls_types::Position,
    offset: &RegionOffset,
    request_id: RequestId,
) -> JsonRpcRequest<TextDocumentPositionParams> {
    build_position_based_request(
        virtual_uri,
        host_position,
        offset,
        request_id,
        "textDocument/completion",
    )
}

/// Parse a JSON-RPC completion response and transform coordinates to host document space.
///
/// Normalizes all responses to `CompletionList` (an array result is wrapped as
/// `CompletionList { isIncomplete: false, items }`). Returns `None` for null
/// results, missing results, and deserialization failures. Each item that
/// [`should_envelope`] selects — the origin advertises
/// `completionItem/resolve`, or the payload squats on the reserved key — has
/// its `data` wrapped in a routing envelope; the rest pass through bare.
fn transform_completion_response_to_host(
    mut response: serde_json::Value,
    offset: &RegionOffset,
    region_end: Position,
    request_host_position: Option<Position>,
    server_resolves: bool,
    envelope_ctx: &EnvelopeContext<'_>,
    insert_mode: ClientInsertMode,
) -> Option<CompletionList> {
    if response_has_jsonrpc_error(&response, "textDocument/completion") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }

    // Determine format and deserialize into a unified CompletionList
    let mut list = if result.is_array() {
        // Legacy format: array of CompletionItem. Normalize to CompletionList.
        let Ok(items) = serde_json::from_value::<Vec<CompletionItem>>(result) else {
            return None;
        };
        CompletionList {
            is_incomplete: false,
            items,
        }
    } else {
        // Preferred format: CompletionList object
        let Ok(list) = serde_json::from_value::<CompletionList>(result) else {
            return None;
        };
        list
    };

    // Transform all items in the list (dropping any whose primary edit is
    // unsafe for the injection region), then envelope for resolve routing
    // under the same policy as the host layer.
    let before = list.items.len();
    list.items.retain_mut(|item| {
        if !transform_completion_item(item, offset, region_end, request_host_position, insert_mode)
        {
            return false;
        }
        if should_envelope(item.data.as_ref(), server_resolves) {
            envelope_item_data(item, envelope_ctx);
        }
        true
    });
    let dropped = before - list.items.len();
    if dropped > 0 {
        log::warn!(
            target: "kakehashi::bridge",
            "completion: dropped {dropped} item(s) whose primary insertion/edit is unsafe for the injection region (escapes it, breaks line prefixes, splits the host line, or merges content into the closing fence) or does not map back through its prepared document"
        );
    }

    Some(list)
}

/// Transform textEdit range in a single completion item to host coordinates.
///
/// Handles both TextEdit format and InsertReplaceEdit format. Also transforms
/// additionalTextEdits if present. On a prepared document the edits go
/// through its map (`translate_virtual_main_and_additional_edits_to_host`),
/// their text re-indented, and the item may be given `insertTextMode: asIs`
/// (see `ClientInsertMode`). A snippet there is checked as its expansion
/// (see [`Expansion`]) and sent with the indentation its expansion regained
/// before it, a literal one as plain text; an item without an edit is
/// probed as an edit at the caret: made that explicit edit when its text
/// regains an indent, dropped when it maps off the caret.
///
/// Returns `false` when the item's primary insertion — its text edit, or the
/// insertText/label fallback applied at the request position — is unsafe for the
/// injection region — escapes it, corrupts per-line `> ` prefixes, or merges
/// content into the closing fence — if applied verbatim; the caller must
/// drop the whole item (same fail-closed rule as WorkspaceEdit bridging).
/// On a prepared document also when its edit does not map back (or its
/// insert and replace ranges map its text apart), a mapped range no longer
/// contains the caret, its snippet expands differently on some client (one
/// breaking a line or holding a tab included) or regains other than
/// spaces before it, its text without an edit breaks a line, or its
/// multi-line edit would reach a client that always adjusts indentation.
/// An unsafe `additionalTextEdits` member drops the whole array instead:
/// the primary insertion still applies, though possibly semantically
/// incomplete (e.g. without its auto-import) — availability over fidelity.
pub(super) fn transform_completion_item(
    item: &mut CompletionItem,
    offset: &RegionOffset,
    region_end: Position,
    request_host_position: Option<Position>,
    insert_mode: ClientInsertMode,
) -> bool {
    if offset.prepared().is_some() {
        transform_prepared_completion_item(
            item,
            offset,
            region_end,
            request_host_position,
            insert_mode,
        )
    } else {
        transform_unprepared_completion_item(
            item,
            offset,
            region_end,
            request_host_position.map(|position| position.line),
        )
    }
}

/// [`transform_completion_item`] on a document as is.
fn transform_unprepared_completion_item(
    item: &mut CompletionItem,
    offset: &RegionOffset,
    region_end: Position,
    request_host_line: Option<u32>,
) -> bool {
    // A multi-line insertion is unsafe only where its INSERTION POINT's
    // neighborhood is prefixed — a mixed region (single-element `[n]` offsets:
    // line 0 starts mid-host-line, later lines are whole and unprefixed)
    // safely takes it on a later line. Checking the following line too covers
    // the inserted-lines-land-below shape, mirroring the shared predicate.
    // `None` (the resolve path, which has no position snapshot) falls back to
    // region-wide any-prefix: fail-closed.
    let prefixed_at_insertion = |host_line: Option<u32>| match host_line {
        Some(line) => {
            insertion_point_prefixed(offset, region_end, line)
                || insertion_point_prefixed(offset, region_end, line.saturating_add(1))
        }
        None => offset.columns().iter().any(|&column| column != 0),
    };
    // The literal newline scans below can't see what a SNIPPET expands to at
    // the client: runtime variables (`${CLIPBOARD}`, `$TM_SELECTED_TEXT`) may
    // be multiline. At a prefixed insertion point, fail closed on any snippet
    // variable in the primary insert text (tabstops/placeholders expand from
    // literal text the scans already cover; additionalTextEdits are never
    // snippets).
    let snippet =
        item.insert_text_format == Some(tower_lsp_server::ls_types::InsertTextFormat::SNIPPET);
    let snippet_unsafe = |text: &str, host_line: Option<u32>| {
        snippet && prefixed_at_insertion(host_line) && snippet_contains_variable(text)
    };

    // Transform text_edit if present
    if let Some(ref mut text_edit) = item.text_edit {
        match text_edit {
            tower_lsp_server::ls_types::CompletionTextEdit::Edit(edit) => {
                translate_virtual_range_to_host(&mut edit.range, offset);
                if !text_edit_safe_in_region(edit, offset, region_end)
                    || snippet_unsafe(&edit.new_text, Some(edit.range.start.line))
                {
                    return false;
                }
            }
            tower_lsp_server::ls_types::CompletionTextEdit::InsertAndReplace(edit) => {
                translate_virtual_range_to_host(&mut edit.insert, offset);
                translate_virtual_range_to_host(&mut edit.replace, offset);
                // Check both ranges through one probe edit, moving new_text in
                // and out instead of cloning it per range.
                let mut probe = tower_lsp_server::ls_types::TextEdit {
                    range: edit.insert,
                    new_text: std::mem::take(&mut edit.new_text),
                };
                let insert_ok = text_edit_safe_in_region(&probe, offset, region_end);
                probe.range = edit.replace;
                let replace_ok = text_edit_safe_in_region(&probe, offset, region_end);
                edit.new_text = probe.new_text;
                // Check BOTH client-selectable ranges: with unequal starts,
                // the minimum line could be unprefixed while the other range
                // starts on a prefixed line.
                if !insert_ok
                    || !replace_ok
                    || snippet_unsafe(&edit.new_text, Some(edit.insert.start.line))
                    || snippet_unsafe(&edit.new_text, Some(edit.replace.start.line))
                {
                    return false;
                }
            }
        }
    }

    // No textEdit: the client inserts insertText (or the label) at the
    // completion position instead. In a prefixed region (per-line `> ` or an
    // inline region's host-line tail) a multi-line insertion creates
    // unprefixed/splitting lines — the same class the textEdit guard rejects —
    // and the transform has no range to check, so the newline itself is the
    // reject signal. All-zero (plain fenced) regions take multi-line
    // insertions safely and stay exempt.
    if item.text_edit.is_none() {
        let effective = item.insert_text.as_deref().unwrap_or(&item.label);
        if (effective.contains('\n') || effective.contains('\r'))
            && prefixed_at_insertion(request_host_line)
        {
            return false;
        }
        if snippet_unsafe(effective, request_host_line) {
            return false;
        }
    }

    // Transform additional_text_edits if present. ALL-OR-NOTHING: the array
    // can carry paired halves of one operation (a deletion plus a
    // reinsertion), so stripping only the unsafe member could apply half and
    // lose content — if any member is unsafe, drop the whole array. The item
    // is kept: its primary insertion still applies, though possibly
    // semantically incomplete (e.g. without its auto-import) — availability
    // over fidelity, never corruption.
    if let Some(ref mut additional_edits) = item.additional_text_edits {
        for edit in additional_edits.iter_mut() {
            translate_virtual_range_to_host(&mut edit.range, offset);
        }
        if !additional_edits
            .iter()
            .all(|edit| text_edit_safe_in_region(edit, offset, region_end))
        {
            log::warn!(
                target: "kakehashi::bridge",
                "completion: dropped an item's additionalTextEdits ({}): a member is unsafe for the injection region (escapes it, breaks line prefixes, or merges content into the closing fence)",
                additional_edits.len()
            );
            item.additional_text_edits = None;
        }
    }
    true
}

/// [`transform_completion_item`] on a prepared document.
fn transform_prepared_completion_item(
    item: &mut CompletionItem,
    offset: &RegionOffset,
    region_end: Position,
    request_host_position: Option<Position>,
    insert_mode: ClientInsertMode,
) -> bool {
    use tower_lsp_server::ls_types::{CompletionTextEdit, InsertTextMode, TextEdit};
    // The text the map checks must be the text the client inserts: a
    // snippet's expansion, every tabstop at its default, kept only when
    // every client expands it alike. The expansion stands in for the
    // snippet through every check below, and the snippet goes out with the
    // indentation its expansion regained before it (`expanded`). (An
    // expansion holds no variable, so none is left to expand into lines at
    // the insertion point, as the unprepared path has to check.)
    let mut expanded: Option<Expansion> = None;
    // An item without an edit is inserted as its insertText (or label): the
    // expansion stands in for it, and it is put back as it came.
    let mut insert_text_sent: Option<Option<String>> = None;
    if item.insert_text_format == Some(tower_lsp_server::ls_types::InsertTextFormat::SNIPPET) {
        let inserted = match &item.text_edit {
            Some(CompletionTextEdit::Edit(edit)) => &edit.new_text,
            Some(CompletionTextEdit::InsertAndReplace(edit)) => &edit.new_text,
            None => item.insert_text.as_ref().unwrap_or(&item.label),
        };
        if inserted.contains(['$', '\\']) {
            // One breaking a line or holding a tab is refused too: snippet
            // engines rewrite both whatever the item asks (see
            // `Expansion::of`).
            let Some(expansion) = Expansion::of(inserted) else {
                return false;
            };
            // Nothing but spaces to check, where the user types at a
            // tabstop: on a blank line among dedented ones the map regains
            // no indent for it, and what is typed would land short of the
            // content's.
            if expansion.text().chars().all(|c| c == ' ') {
                return false;
            }
            let text = expansion.text().to_string();
            match &mut item.text_edit {
                Some(CompletionTextEdit::Edit(edit)) => edit.new_text = text,
                Some(CompletionTextEdit::InsertAndReplace(edit)) => edit.new_text = text,
                None => {
                    insert_text_sent = Some(item.insert_text.replace(text));
                }
            }
            expanded = Some(expansion);
        } else {
            // Literal text means the same as plain text, which a client's
            // snippet engine cannot re-indent once more on insertion.
            item.insert_text_format =
                Some(tower_lsp_server::ls_types::InsertTextFormat::PLAIN_TEXT);
        }
    }
    // LSP requires an item's edit ranges to contain the requested position.
    // Out of a prepared document a range can map past it: the server saw the
    // caret where the prepared text has no position of its own (an emptied
    // gap's edge, say), and an edit anchored there maps to the far side of
    // what the peer removed.
    let contains_request = |range: &tower_lsp_server::ls_types::Range| {
        request_host_position
            .is_none_or(|position| range.start <= position && position <= range.end)
    };
    // Transform text_edit if present, and the additional edits with it, as
    // the one set the client applies, through each range it may apply the
    // primary with.
    let additional = item.additional_text_edits.as_mut();
    let mut additional_kept = match &mut item.text_edit {
        None => translate_virtual_main_and_additional_edits_to_host(&mut [], additional, offset)
            .unwrap_or(false),
        Some(CompletionTextEdit::Edit(edit)) => {
            let Some(kept) = translate_virtual_main_and_additional_edits_to_host(
                std::slice::from_mut(edit),
                additional,
                offset,
            ) else {
                return false;
            };
            kept
        }
        Some(CompletionTextEdit::InsertAndReplace(edit)) => {
            let Some(kept) = translate_insert_replace_edit_to_host(edit, additional, offset) else {
                return false;
            };
            kept
        }
    };
    if let Some(ref mut text_edit) = item.text_edit {
        match text_edit {
            CompletionTextEdit::Edit(edit) => {
                if !contains_request(&edit.range)
                    || !text_edit_safe_in_region(edit, offset, region_end)
                {
                    return false;
                }
            }
            CompletionTextEdit::InsertAndReplace(edit) => {
                if !contains_request(&edit.insert) || !contains_request(&edit.replace) {
                    return false;
                }
                // Check both ranges through one probe edit, moving new_text in
                // and out instead of cloning it per range.
                let mut probe = TextEdit {
                    range: edit.insert,
                    new_text: std::mem::take(&mut edit.new_text),
                };
                let insert_ok = text_edit_safe_in_region(&probe, offset, region_end);
                probe.range = edit.replace;
                let replace_ok = text_edit_safe_in_region(&probe, offset, region_end);
                edit.new_text = probe.new_text;
                // Check BOTH client-selectable ranges: with unequal starts,
                // the minimum line could be unprefixed while the other range
                // starts on a prefixed line.
                if !insert_ok || !replace_ok {
                    return false;
                }
            }
        }
    }

    // A multi-line edit already regained the indentation the peer removed:
    // a client adjusting it to the line's indentation as well would indent
    // it twice, inserting other text than was checked.
    if let Some(text_edit) = &item.text_edit {
        let new_text = match text_edit {
            CompletionTextEdit::Edit(edit) => &edit.new_text,
            CompletionTextEdit::InsertAndReplace(edit) => &edit.new_text,
        };
        if new_text.contains(['\n', '\r']) {
            match insert_mode {
                ClientInsertMode::AsIsSettable => {
                    item.insert_text_mode = Some(InsertTextMode::AS_IS);
                }
                ClientInsertMode::AdjustsIndentation => return false,
                // The client's default inserts as is; an item asking it to
                // adjust instead (honored if the client supports that
                // override) is cleared back to the default.
                ClientInsertMode::AsIs => {
                    if item.insert_text_mode == Some(InsertTextMode::ADJUST_INDENTATION) {
                        item.insert_text_mode = None;
                    }
                }
            }
        }
    }

    // No textEdit: the client inserts insertText (or the label) at the
    // completion position instead, verbatim: a multi-line insertion lands
    // without the indentation the prepare peer removed (explicit edits regain
    // it through the edit seam instead), so the newline itself is the reject
    // signal, as in any prefixed region.
    if item.text_edit.is_none() {
        let effective = item.insert_text.as_deref().unwrap_or(&item.label);
        if effective.contains('\n') || effective.contains('\r') {
            return false;
        }
        // On a prepared document the client inserts the text at the caret
        // as is. Where the same text as an edit at the caret regains an
        // indent (a blank line among dedented ones, where no word prefix is
        // the client's to replace), or replaces whitespace there with the
        // content's indent (a blank line the peer emptied of other
        // whitespace), the item goes out as that edit, as an item with one
        // would; one mapping off the caret's line or the caret is refused.
        if let Some(caret) = request_host_position
            && !effective.is_empty()
        {
            let mut at = caret;
            super::super::protocol::translate_host_position_to_virtual(&mut at, offset);
            let mut probe = TextEdit {
                range: tower_lsp_server::ls_types::Range::new(at, at),
                new_text: effective.to_string(),
            };
            // The edit may replace whitespace around the caret on its line
            // (a blank line the peer emptied, whose whitespace the content's
            // indent replaces); it must stay on that line and contain the
            // caret, as LSP requires of an item's edit. The additional edits
            // map with it as the one set the client applies, should it go
            // out as that edit.
            let Some(additional_kept_with_probe) =
                translate_virtual_main_and_additional_edits_to_host(
                    std::slice::from_mut(&mut probe),
                    item.additional_text_edits.as_mut(),
                    offset,
                )
            else {
                return false;
            };
            let range = probe.range;
            if range.start.line != caret.line
                || range.end.line != caret.line
                || caret < range.start
                || range.end < caret
            {
                return false;
            }
            if probe.new_text != effective
                || range != tower_lsp_server::ls_types::Range::new(caret, caret)
            {
                // Written here, it answers to the region's checks as any
                // edit does (a caret on the closing fence's line, say).
                if !text_edit_safe_in_region(&probe, offset, region_end) {
                    return false;
                }
                item.text_edit = Some(CompletionTextEdit::Edit(probe));
                additional_kept = additional_kept_with_probe;
            }
        }
    }

    // An item without a textEdit has no primary to check its additional
    // edits with (the client inserts at the caret, over a range it picks):
    // they are dropped, unless the item goes out as the edit the caret probe
    // made, with which they were checked.
    if let Some(dropped) = keep_additional_edits_if_safe(
        &mut item.additional_text_edits,
        additional_kept,
        offset,
        region_end,
    ) {
        log::warn!(
            target: "kakehashi::bridge",
            "completion: dropped an item's additionalTextEdits ({dropped}): a member is unsafe for the injection region (escapes it, breaks line prefixes, merges content into the closing fence, or reaches host text its prepared document hides)",
        );
    }
    // The snippet goes out where its expansion was checked, expanding to
    // the text checked: the indentation the map wrote before the expansion
    // written before the snippet, or the item refused.
    if let Some(expansion) = expanded {
        let mapped = match &mut item.text_edit {
            Some(CompletionTextEdit::Edit(edit)) => Some(&mut edit.new_text),
            Some(CompletionTextEdit::InsertAndReplace(edit)) => Some(&mut edit.new_text),
            None => None,
        };
        if let Some(mapped) = mapped {
            let Some(snippet) = expansion.carry(mapped) else {
                return false;
            };
            *mapped = snippet;
        }
        if let Some(sent) = insert_text_sent {
            item.insert_text = sent;
        }
    }
    true
}

/// Translate both ranges of a prepared document's insert/replace completion
/// edit in place, with the item's additional edits (see
/// `translate_virtual_main_and_additional_edits_to_host`, whose answer this
/// is). Each range goes through the edit seam with the shared `newText`,
/// which must map to the same text for either choice the client makes.
fn translate_insert_replace_edit_to_host(
    edit: &mut tower_lsp_server::ls_types::InsertReplaceEdit,
    additional: Option<&mut Vec<tower_lsp_server::ls_types::TextEdit>>,
    offset: &RegionOffset,
) -> Option<bool> {
    use tower_lsp_server::ls_types::TextEdit;
    // LSP has both ranges start at the same position. Sent apart, no
    // realignment below would send a range that was checked.
    if edit.insert.start != edit.replace.start {
        return None;
    }
    let new_text = std::mem::take(&mut edit.new_text);
    let mut mains = [
        TextEdit {
            range: edit.insert,
            new_text: new_text.clone(),
        },
        TextEdit {
            range: edit.replace,
            new_text,
        },
    ];
    let kept = translate_virtual_main_and_additional_edits_to_host(&mut mains, additional, offset);
    let [insert, replace] = mains;
    edit.new_text = replace.new_text;
    let kept = kept?;
    if insert.new_text != edit.new_text {
        return None;
    }
    edit.insert = insert.range;
    edit.replace = replace.range;
    // Mapped, the two ranges must still share their start (LSP), as sent.
    // Clearing a dedented line's content maps the replace range over the
    // removed indent, which the (empty) insert range does not cover: start
    // both after it, as the shared text was re-indented for.
    if edit.replace.start != edit.insert.start {
        if edit.replace.start < edit.insert.start && edit.insert.start <= edit.replace.end {
            edit.replace.start = edit.insert.start;
        } else {
            return None;
        }
    }
    Some(kept)
}

/// How the client inserts a completion's multi-line text (LSP
/// `insertTextMode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum ClientInsertMode {
    /// As is, by default, with no way to tell it otherwise declared.
    #[default]
    AsIs,
    /// It honors an item's `insertTextMode: asIs`.
    AsIsSettable,
    /// It adjusts the indentation by default, and an item cannot tell it not
    /// to.
    AdjustsIndentation,
}

impl ClientInsertMode {
    /// From the client's completion capabilities.
    pub(super) fn from_capabilities(
        caps: Option<&tower_lsp_server::ls_types::ClientCapabilities>,
    ) -> Self {
        use tower_lsp_server::ls_types::InsertTextMode;
        let completion = caps
            .and_then(|caps| caps.text_document.as_ref())
            .and_then(|text_document| text_document.completion.as_ref());
        let settable = completion
            .and_then(|completion| completion.completion_item.as_ref())
            .and_then(|item| item.insert_text_mode_support.as_ref())
            .is_some_and(|support| support.value_set.contains(&InsertTextMode::AS_IS));
        if settable {
            Self::AsIsSettable
        } else if completion
            .and_then(|completion| completion.insert_text_mode)
            .is_some_and(|mode| mode == InsertTextMode::ADJUST_INDENTATION)
        {
            Self::AdjustsIndentation
        } else {
            Self::AsIs
        }
    }
}

/// Whether the host line carries region-adjacent host content an unprefixed
/// insertion would break. Lines before the region are fail-closed `true`.
/// Lines past
/// the recorded per-line array are the BOUNDARY in per-line (blockquote)
/// regions — fail-closed `true` (their real prefix is unrecorded) — but in
/// the single-element fallback shape (`[start_column]`: only line 0 starts
/// mid-host-line) later lines are whole and unprefixed: `false`.
fn insertion_point_prefixed(offset: &RegionOffset, region_end: Position, host_line: u32) -> bool {
    let columns = offset.columns();
    // Boundary rows of a per-line-prefixed region — the recorded trailing
    // zeros and everything past the array — carry an unrecorded real prefix
    // (the closing fence's `> `): fail-closed, mirroring the shared guard's
    // boundary rule. All-zero multi-line regions have no boundary semantics.
    let per_line_prefixed = columns.len() > 1 && columns.iter().any(|&column| column != 0);
    if per_line_prefixed && region_end.character == 0 && host_line >= region_end.line {
        return true;
    }
    match host_line.checked_sub(offset.line()) {
        None => true,
        Some(virtual_line) => match columns.get(virtual_line as usize) {
            // Single-element shape on line 0: in a MULTI-LINE region the
            // captured content runs to end-of-line, so an inserted newline
            // splits only injected content (mirror the shared guard's
            // `newline_splits_nothing`); in a SAME-LINE region the host line
            // continues past the region, so a non-zero start column (inline
            // content always sits behind its delimiter) is prefixed. A
            // column-0 same-line region is a one-line fenced block — safe.
            // Known limitation: a CUSTOM injection query (`#offset!`) could
            // capture a column-0 same-line range with a host suffix after it;
            // the offsets can't represent that, and treating column 0 as
            // unsafe breaks insertFinalNewline for one-line fences — the
            // built-in queries never produce the shape.
            Some(&column) => column != 0 && (columns.len() > 1 || region_end.line == offset.line()),
            None => per_line_prefixed,
        },
    }
}

/// Whether snippet-syntax text references a snippet VARIABLE — `$name` or
/// `${name...}` with a leading letter/underscore — whose expansion the client
/// resolves at insert time and may be multiline (`${CLIPBOARD}`,
/// `$TM_SELECTED_TEXT`). Numeric tabstops/placeholders (`$1`, `${1:text}`)
/// expand from literal text the caller's newline scans already cover, so they
/// don't count. An escaped `\\$` is literal per the snippet grammar; a
/// preceding backslash therefore skips the candidate (an over-escape here
/// would only reject, never admit).
fn snippet_contains_variable(text: &str) -> bool {
    let bytes = text.as_bytes();
    let starts_name = |c: u8| c.is_ascii_alphabetic() || c == b'_';
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // A backslash escapes the next character (literal per the snippet
            // grammar) — skip both.
            b'\\' => i += 2,
            b'$' => {
                match bytes.get(i + 1).copied() {
                    Some(b'{') if bytes.get(i + 2).copied().is_some_and(starts_name) => {
                        return true;
                    }
                    Some(c) if starts_name(c) => return true,
                    _ => {}
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    false
}

// =============================================================================
// Envelope types for completionItem/resolve routing
// =============================================================================

/// Envelope stored in `CompletionItem.data` for routing `completionItem/resolve`.
///
/// When Kakehashi fans out completion to multiple downstream servers, the
/// `data` of each item whose origin advertises `completionItem/resolve` (or
/// whose payload squats on the reserved key) is wrapped in this envelope so
/// that a later `completionItem/resolve` can be routed back to the correct
/// server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct KakehashiEnvelope {
    /// Server name identifying which downstream produced the item.
    pub origin: String,
    /// Language used to construct the virtual document URI for resolve.
    /// Empty for host-layer and legacy envelopes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub injection_language: String,
    /// Host open incarnation that produced this item. Missing for legacy data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incarnation: Option<u64>,
    /// Host document URI the completion was requested on. Used to re-resolve the
    /// same `(server, root)` connection for `completionItem/resolve` so the
    /// resolve reaches the very process that produced the item (#382) — without
    /// it, resolve would land on the server's client-root fallback connection
    /// (or, for a `preferSharedInstance` server, its shared instance),
    /// a *different* process in a multi-root monorepo of per-root servers. Stored as a `String`
    /// (not `url::Url`) because this crate does not enable the `url/serde`
    /// feature; the resolve path parses it once. Empty (via `serde(default)`)
    /// for envelopes from before this field existed → falls back to root-less
    /// routing, matching the old behavior.
    #[serde(default)]
    pub host_uri: String,
    /// Stable injection region identity for live resolve-time geometry checks.
    /// Empty for envelopes minted before this field existed, and for every
    /// host-layer envelope (which has no region) — skipped on the wire in
    /// that case, since this rides in every enveloped item of a completion
    /// response.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub region_id: String,
    /// The downstream server's original `data` value (preserved verbatim).
    /// Absent — not `null` — when the server sent none, which is most items
    /// on most responses; `wrap_envelope` moves the value in afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inner: Option<Value>,
    /// Region offset snapshot for coordinate transformation of the resolved response.
    pub offset: EnvelopeOffset,
    /// Region end (host `(line, character)`) snapshot for the resolve-path
    /// safety guard (containment, prefixes, fence boundary) — the resolve has
    /// no region-content access, so it can't recompute this. `None` (via `serde(default)`) for envelopes
    /// minted before this field existed → the resolve path falls back to a
    /// fully fail-closed guard in prefixed regions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region_end: Option<(u32, u32)>,
    /// Host-layer item (`bridge._self`, #958): it is already in host
    /// coordinates, so `completionItem/resolve` routes to the host server
    /// VERBATIM — no virtual URI, region, or offset translation.
    /// `region_id`/`offset`/`region_end` are unused for these. Defaults to
    /// `false` (virt layer) so existing enveloped items deserialize unchanged
    /// — and is skipped on the wire in that case, so the far more numerous
    /// virt items pay nothing for a marker only host items use.
    #[serde(default, skip_serializing_if = "is_false")]
    pub host_layer: bool,
}

/// `skip_serializing_if` predicate for a `bool` that defaults to `false`.
fn is_false(value: &bool) -> bool {
    !*value
}

impl KakehashiEnvelope {
    /// Whether this is a genuine HOST-layer envelope: `host_layer` set AND no
    /// region identity. A host envelope is minted with an empty `region_id`
    /// ([`envelope_host_item`]), so requiring both here means a CONFORMING
    /// client that merely flips `host_layer = true` on a VIRT envelope (which
    /// keeps its `region_id`) still can't route it through the translation-free
    /// host path and skip the region freshness / coordinate validation. This is
    /// not a security boundary: the envelope round-trips through client-supplied
    /// `CompletionItem.data` and is not integrity-protected, so a client could
    /// clear `region_id` too — the check guards against ACCIDENTAL bypass, not a
    /// malicious one (which the translation-free host path also fails soft on).
    pub(crate) fn is_host_layer(&self) -> bool {
        self.host_layer && self.region_id.is_empty()
    }
}

/// Offset snapshot stored in the envelope for coordinate back-translation.
///
/// Serialized into `CompletionItem.data` and roundtripped through the client.
/// The optional `line_column_offsets` field preserves per-line column offsets
/// for blockquoted injections. Old envelopes without this field deserialize
/// with `None` thanks to `#[serde(default)]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct EnvelopeOffset {
    pub line: u32,
    pub column: u32,
    /// Per-line column offsets for blockquoted injections.
    /// `None` for non-blockquote injections (backwards-compatible default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_column_offsets: Option<Vec<u32>>,
    /// The item came from a prepared document, whose map no envelope can
    /// carry: it never resolves through these offsets alone, whatever the
    /// region reads as by then (after its pair lost its peer, the offsets
    /// alone compare equal) — a code action's resolves through the text its
    /// server held as it was listed (see `CodeActionEnvelope`), other items
    /// not at all.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prepared: bool,
}

impl EnvelopeOffset {
    /// Whether the region still has the offset this envelope was minted
    /// with — never for an envelope minted from a prepared document.
    pub(crate) fn describes(&self, live: &RegionOffset) -> bool {
        !self.prepared && RegionOffset::from(self) == *live
    }
}

impl EnvelopeOffset {
    /// This offset as a fresh mint writes it, for an item re-enveloped after
    /// a refused resolve, keeping the mark of an item minted from a prepared
    /// document, which no `RegionOffset` carries without the map: dropped,
    /// the item would describe the region again once its pair lost its peer.
    /// An item minted as is gets exactly the bytes a fresh mint gives.
    pub(crate) fn rewritten(&self) -> Self {
        Self {
            prepared: self.prepared,
            ..Self::from(&RegionOffset::from(self))
        }
    }
}

impl From<&RegionOffset> for EnvelopeOffset {
    fn from(o: &RegionOffset) -> Self {
        Self {
            line: o.line(),
            column: o.column_for_line(0),
            line_column_offsets: Some(o.columns().to_vec()),
            prepared: o.prepared().is_some(),
        }
    }
}

impl From<&EnvelopeOffset> for RegionOffset {
    fn from(o: &EnvelopeOffset) -> Self {
        match &o.line_column_offsets {
            Some(columns) => Self::with_per_line_offsets(o.line, columns.clone()),
            None => Self::new(o.line, o.column),
        }
    }
}

/// Context needed to create envelopes during completion response processing.
pub(crate) struct EnvelopeContext<'a> {
    pub server_name: &'a str,
    pub injection_language: &'a str,
    pub incarnation: Option<u64>,
    /// Host document URI the completion ran on, stored in the envelope so
    /// `completionItem/resolve` can route back to the originating connection.
    pub host_uri: &'a str,
    pub region_id: &'a str,
    pub offset: &'a RegionOffset,
    /// Region end (host coords) snapshot for the resolve-path safety guard;
    /// `None` only when re-enveloping a legacy envelope that never carried it.
    pub region_end: Option<Position>,
    /// Carries the layer through a re-envelope: a host-layer item resolved
    /// once must still route through the host path on the NEXT resolve.
    /// Minting sites use `false` (virt) or [`envelope_host_item`].
    pub host_layer: bool,
}

/// Wrap `item.data` in a Kakehashi envelope for origin tracking.
///
/// The original `data` value is moved into `inner`, and the envelope is
/// serialized as `{"kakehashi": {...}}`.
pub(crate) fn envelope_item_data(item: &mut CompletionItem, ctx: &EnvelopeContext) {
    let inner = item.data.take();
    let envelope = KakehashiEnvelope {
        origin: ctx.server_name.to_string(),
        injection_language: ctx.injection_language.to_string(),
        incarnation: ctx.incarnation,
        host_uri: ctx.host_uri.to_string(),
        region_id: ctx.region_id.to_string(),
        inner: None,
        offset: EnvelopeOffset::from(ctx.offset),
        region_end: ctx.region_end.map(|end| (end.line, end.character)),
        host_layer: ctx.host_layer,
    };
    item.data = Some(wrap_envelope(&envelope, inner));
}

/// Apply the HOST-layer completion policy to a host response (#958): envelope
/// each item so a later `completionItem/resolve` routes back to the server that
/// produced it. The bridge owns this the way it owns `bridge_code_actions` for
/// the codeAction host layer; the handler only supplies `server_resolves`.
///
/// `server_resolves` — whether the answering connection advertises
/// `completionItem/resolve` — gates the minting: without it the envelope would
/// be pure wire weight on every item of every completion, and the resolve would
/// fail soft back to the unresolved item anyway. An item whose OWN `data`
/// already carries the envelope key is enveloped regardless: served verbatim it
/// would be read back AS a Kakehashi envelope at resolve time, letting a
/// downstream server author routing (an origin to spawn, a URI to root it at)
/// that it never earned.
///
/// Both response shapes are covered — host servers answer with a bare array as
/// often as with a `CompletionList`, and a List-only loop would leave array
/// responses unresolvable, which is the very bug this fixes.
pub(crate) fn bridge_host_completion_items(
    response: &mut tower_lsp_server::ls_types::CompletionResponse,
    server_name: &str,
    host_uri: &str,
    incarnation: Option<u64>,
    server_resolves: bool,
) {
    use tower_lsp_server::ls_types::CompletionResponse;
    let items = match response {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => &mut list.items,
    };
    for item in items.iter_mut() {
        if should_envelope(item.data.as_ref(), server_resolves) {
            envelope_host_item(item, server_name, host_uri, incarnation);
        }
    }
}

/// Wrap a HOST-layer item's `data` in a routing envelope so a later
/// `completionItem/resolve` routes back to the host server (#958). The host
/// document is forwarded verbatim, so no region/offset is captured
/// (`host_layer = true` tells the resolve path to skip all coordinate
/// translation and its region-geometry guards).
pub(super) fn envelope_host_item(
    item: &mut CompletionItem,
    server_name: &str,
    host_uri: &str,
    incarnation: Option<u64>,
) {
    let inner = item.data.take();
    let envelope = KakehashiEnvelope {
        origin: server_name.to_string(),
        injection_language: String::new(),
        incarnation,
        host_uri: host_uri.to_string(),
        region_id: String::new(),
        inner: None,
        // Unused on the host path (resolve forwards verbatim); a zero offset.
        offset: EnvelopeOffset {
            line: 0,
            column: 0,
            line_column_offsets: None,
            prepared: false,
        },
        region_end: None,
        host_layer: true,
    };
    item.data = Some(wrap_envelope(&envelope, inner));
}

/// Extract the envelope from a completion item's `data` without modifying the item.
///
/// Returns `None` if `data` is absent or not an envelope.
pub(crate) fn extract_envelope(item: &CompletionItem) -> Option<KakehashiEnvelope> {
    let data = item.data.as_ref()?;
    let wrapper = data.get(ENVELOPE_KEY)?;
    serde_json::from_value(wrapper.clone()).ok()
}

/// Extract the envelope and restore the original `data` value.
///
/// On success, `item.data` is set back to the downstream's original value (`inner`).
/// Returns the extracted envelope. Returns `None` if not an envelope (item unchanged).
pub(crate) fn strip_envelope(item: &mut CompletionItem) -> Option<KakehashiEnvelope> {
    let mut envelope = extract_envelope(item)?;
    item.data = envelope.inner.take();
    Some(envelope)
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    /// A region at host line 10 holding `virtual_text`, dedented by two on
    /// every line starting with two spaces.
    fn dedented_offset(virtual_text: &str) -> RegionOffset {
        RegionOffset::new(10, 0).with_prepared(dedented_map(virtual_text))
    }

    /// The map of `virtual_text` dedented by two on every line starting
    /// with two spaces.
    fn dedented_map(virtual_text: &str) -> std::sync::Arc<crate::lsp::bridge::PreparedMap> {
        use crate::lsp::bridge::protocol::{VirtualLayout, apply_prepare_result};
        let changes: Vec<_> = virtual_text
            .split('\n')
            .enumerate()
            .filter(|(_, line)| line.starts_with("  "))
            .map(|(line, _)| json!({
                "range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": 2}},
                "newText": ""
            }))
            .collect();
        let result =
            serde_json::from_value(json!({"segments": [{"type": "content", "changes": changes}]}))
                .unwrap();
        apply_prepare_result(
            virtual_text,
            &VirtualLayout::single(virtual_text),
            Some(result),
        )
        .unwrap()
        .map
    }

    fn item(value: serde_json::Value) -> CompletionItem {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_prepared_edit_maps_back_reindented_with_its_additional_edits() {
        // `  import os\n  x = o\n`, the server completing `o` on P line 1
        // and adding an import line above.
        let offset = dedented_offset("  import os\n  x = o\n");
        let mut completed = item(json!({
            "label": "os.path",
            "textEdit": {
                "range": {"start": {"line": 1, "character": 4}, "end": {"line": 1, "character": 5}},
                "newText": "os.path"
            },
            "additionalTextEdits": [{
                "range": {"start": {"line": 0, "character": 9}, "end": {"line": 0, "character": 9}},
                "newText": "\nimport os.path"
            }]
        }));
        assert!(transform_completion_item(
            &mut completed,
            &offset,
            Position::new(12, 0),
            Some(Position::new(11, 7)),
            ClientInsertMode::default(),
        ));
        let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(edit)) = &completed.text_edit
        else {
            panic!("{completed:?}");
        };
        assert_eq!(
            edit.range,
            tower_lsp_server::ls_types::Range::new(Position::new(11, 6), Position::new(11, 7))
        );
        assert_eq!(
            completed.additional_text_edits,
            Some(vec![tower_lsp_server::ls_types::TextEdit {
                range: tower_lsp_server::ls_types::Range::new(
                    Position::new(10, 11),
                    Position::new(10, 11)
                ),
                new_text: "\n  import os.path".to_string(),
            }])
        );
    }

    #[test]
    fn a_prepared_item_without_a_text_edit_drops_its_additional_edits() {
        // Inserted at the caret over a range the client picks: there is no
        // primary edit to check the additional edits with. (A region at line
        // 0, where the edits as sent would be in bounds: only the refusal
        // drops them.)
        let offset = RegionOffset::new(0, 0).with_prepared(dedented_map("  a\n  \n  b\n"));
        let mut completed = item(json!({
            "label": "x",
            "additionalTextEdits": [{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                "newText": "y"
            }]
        }));
        assert!(transform_completion_item(
            &mut completed,
            &offset,
            Position::new(3, 0),
            Some(Position::new(1, 2)),
            ClientInsertMode::default(),
        ));
        assert_eq!(completed.additional_text_edits, None);
    }

    #[test]
    fn an_empty_additional_edits_list_is_kept_as_it_came() {
        // Nothing to check, even with no main edit to check it with.
        let offset = dedented_offset("  a\n");
        let mut completed = item(json!({"label": "x", "additionalTextEdits": []}));
        assert!(transform_completion_item(
            &mut completed,
            &offset,
            Position::new(11, 0),
            Some(Position::new(10, 3)),
            ClientInsertMode::default(),
        ));
        assert_eq!(completed.additional_text_edits, Some(vec![]));
    }

    #[test]
    fn a_prepared_item_inserted_at_the_caret_goes_out_as_its_edit_would() {
        // `    abc\n\n    xyz\n` dedented by four: on the blank line an edit
        // would regain the indent the client's verbatim insertion lacks.
        let virtual_text = "    abc\n\n    xyz\n";
        let map = {
            use crate::lsp::bridge::protocol::{VirtualLayout, apply_prepare_result};
            let result = serde_json::from_value(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 4}}, "newText": ""},
                {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 4}}, "newText": ""}
            ]}]}))
            .unwrap();
            apply_prepare_result(
                virtual_text,
                &VirtualLayout::single(virtual_text),
                Some(result),
            )
            .unwrap()
            .map
        };
        let offset = RegionOffset::new(10, 0).with_prepared(map);
        let plain = || item(json!({"label": "foo"}));
        let mut on_blank = plain();
        assert!(transform_completion_item(
            &mut on_blank,
            &offset,
            Position::new(13, 0),
            Some(Position::new(11, 0)),
            ClientInsertMode::default(),
        ));
        assert_eq!(
            on_blank.text_edit,
            Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(
                tower_lsp_server::ls_types::TextEdit {
                    range: tower_lsp_server::ls_types::Range::new(
                        Position::new(11, 0),
                        Position::new(11, 0)
                    ),
                    new_text: "    foo".to_string(),
                }
            ))
        );
        // After text, the insertion is what the edit would be: as it came.
        let mut after_text = plain();
        assert!(transform_completion_item(
            &mut after_text,
            &offset,
            Position::new(13, 0),
            Some(Position::new(10, 7)),
            ClientInsertMode::default(),
        ));
        assert_eq!(after_text.text_edit, None);
    }

    #[test]
    fn a_caret_edit_written_for_an_item_answers_to_the_region_checks() {
        // `  a\n` dedented, the caret at column 0 of the closing fence's line
        // (the region's end): as an edit the item would regain the indent
        // and push text onto the fence.
        let offset = dedented_offset("  a\n");
        let mut completed = item(json!({"label": "foo"}));
        assert!(!transform_completion_item(
            &mut completed,
            &offset,
            Position::new(11, 0),
            Some(Position::new(11, 0)),
            ClientInsertMode::default(),
        ));
    }

    #[test]
    fn a_prepared_item_inserting_lines_at_the_caret_is_dropped() {
        // Without a textEdit the client inserts it as is: its second line
        // would not regain the removed indent.
        let offset = dedented_offset("  a\n");
        let mut completed = item(json!({"label": "x", "insertText": "if x:\n  y"}));
        assert!(!transform_completion_item(
            &mut completed,
            &offset,
            Position::new(11, 0),
            Some(Position::new(10, 3)),
            ClientInsertMode::default(),
        ));
    }

    #[test]
    fn additional_edits_refused_with_the_primary_are_dropped() {
        // The additional edit touches the primary right after its line
        // break: together they are refused; the primary stays as it maps
        // alone.
        let offset = dedented_offset("  a\n  b\n");
        let mut completed = item(json!({
            "label": "x",
            "textEdit": {
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                "newText": "x\n"
            },
            "additionalTextEdits": [{
                "range": {"start": {"line": 0, "character": 1}, "end": {"line": 1, "character": 0}},
                "newText": "y"
            }]
        }));
        assert!(transform_completion_item(
            &mut completed,
            &offset,
            Position::new(12, 0),
            Some(Position::new(10, 2)),
            ClientInsertMode::default(),
        ));
        assert_eq!(completed.additional_text_edits, None);
    }

    #[rstest]
    #[case::as_is_settable(
        ClientInsertMode::AsIsSettable,
        true,
        Some(tower_lsp_server::ls_types::InsertTextMode::AS_IS)
    )]
    #[case::adjusting(ClientInsertMode::AdjustsIndentation, false, None)]
    #[case::as_is(ClientInsertMode::AsIs, true, None)]
    fn a_prepared_multi_line_edit_is_inserted_as_is(
        #[case] insert_mode: ClientInsertMode,
        #[case] kept: bool,
        #[case] mode: Option<tower_lsp_server::ls_types::InsertTextMode>,
    ) {
        // Mapped, its second line regained the removed indent: a client
        // adjusting it to the line's indent again would double it.
        let offset = dedented_offset("  foo\n  baz\n");
        let mut completed = item(json!({
            "label": "if",
            "insertTextMode": 2,
            "textEdit": {
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
                "newText": "if x:\n  pass"
            }
        }));
        assert_eq!(
            transform_completion_item(
                &mut completed,
                &offset,
                Position::new(12, 0),
                Some(Position::new(10, 5)),
                insert_mode,
            ),
            kept
        );
        if kept {
            assert_eq!(completed.insert_text_mode, mode);
        }
    }

    /// The prepared-only rules leave an unprepared item as main sends it:
    /// its edit range need not contain the caret, its multi-line edit keeps
    /// the insert mode it asked for whatever the client does, and its
    /// snippet stays a snippet.
    #[rstest]
    #[case::as_is_settable(ClientInsertMode::AsIsSettable)]
    #[case::adjusting(ClientInsertMode::AdjustsIndentation)]
    #[case::as_is(ClientInsertMode::AsIs)]
    fn prepared_only_rules_leave_an_unprepared_item_alone(#[case] insert_mode: ClientInsertMode) {
        use tower_lsp_server::ls_types::{InsertTextFormat, InsertTextMode};
        let offset = RegionOffset::new(10, 0);
        let mut completed = item(json!({
            "label": "if",
            "insertTextMode": 2,
            "insertTextFormat": 2,
            "textEdit": {
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
                "newText": "if ${1:x}:\n  pass"
            }
        }));
        assert!(transform_completion_item(
            &mut completed,
            &offset,
            Position::new(12, 0),
            // Past the edit's range.
            Some(Position::new(11, 2)),
            insert_mode,
        ));
        assert_eq!(
            completed.insert_text_mode,
            Some(InsertTextMode::ADJUST_INDENTATION)
        );
        assert_eq!(
            completed.insert_text_format,
            Some(InsertTextFormat::SNIPPET)
        );
    }

    #[test]
    fn the_client_insert_mode_follows_its_capabilities() {
        let caps = |value: serde_json::Value| -> tower_lsp_server::ls_types::ClientCapabilities {
            serde_json::from_value(value).unwrap()
        };
        assert_eq!(
            ClientInsertMode::from_capabilities(None),
            ClientInsertMode::AsIs
        );
        assert_eq!(
            ClientInsertMode::from_capabilities(Some(&caps(
                json!({"textDocument": {"completion": {
                    "completionItem": {"insertTextModeSupport": {"valueSet": [1, 2]}}
                }}})
            ))),
            ClientInsertMode::AsIsSettable
        );
        assert_eq!(
            ClientInsertMode::from_capabilities(Some(&caps(
                json!({"textDocument": {"completion": {
                    "insertTextMode": 2
                }}})
            ))),
            ClientInsertMode::AdjustsIndentation
        );
        // Support for adjusting only is no way to ask for as is.
        assert_eq!(
            ClientInsertMode::from_capabilities(Some(&caps(
                json!({"textDocument": {"completion": {
                    "completionItem": {"insertTextModeSupport": {"valueSet": [2]}}
                }}})
            ))),
            ClientInsertMode::AsIs
        );
    }

    /// The text a client inserts for an item on a prepared document, from
    /// its edit or (without one) its insertText or label.
    fn sent_text(completed: &CompletionItem) -> &str {
        use tower_lsp_server::ls_types::CompletionTextEdit;
        match &completed.text_edit {
            Some(CompletionTextEdit::Edit(edit)) => &edit.new_text,
            Some(CompletionTextEdit::InsertAndReplace(edit)) => &edit.new_text,
            None => completed.insert_text.as_ref().unwrap_or(&completed.label),
        }
    }

    #[rstest]
    #[case::tabstop("x$0", true)]
    #[case::placeholder("${1:x}", true)]
    #[case::choice("${1|x,y|}", false)]
    #[case::mirror("${1:x}$1", true)]
    #[case::escape("x\\$", true)]
    #[case::escaped_brace("x\\}", true)]
    #[case::literal("x", true)]
    #[case::variable("$TM_SELECTED_TEXT", false)]
    #[case::variable_with_default("${CLIPBOARD:x}", false)]
    #[case::transform("${1/a/b/}", false)]
    #[case::nested_placeholder("${1:${2:x}}", false)]
    #[case::two_defaults("${1:a}${1:b}", false)]
    #[case::choice_escaping_a_dollar("${1|a\\$|}", false)]
    fn a_prepared_snippet_is_kept_as_every_client_expands_it(
        #[case] snippet: &str,
        #[case] kept: bool,
    ) {
        use tower_lsp_server::ls_types::InsertTextFormat;
        let offset = dedented_offset("  a\n");
        for completed in [
            json!({"label": "x", "insertTextFormat": 2, "insertText": snippet}),
            json!({"label": snippet, "insertTextFormat": 2}),
            json!({"label": "x", "insertTextFormat": 2, "textEdit": {
                "range": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 1}},
                "newText": snippet
            }}),
        ] {
            let mut transformed = item(completed.clone());
            assert_eq!(
                transform_completion_item(
                    &mut transformed,
                    &offset,
                    Position::new(11, 0),
                    Some(Position::new(10, 3)),
                    ClientInsertMode::default(),
                ),
                kept,
                "{completed}"
            );
            if !kept {
                continue;
            }
            // Kept, it goes out as it came: on one line, the map writes
            // nothing into it. Literal text goes out as the plain text it
            // is, which no snippet engine re-indents.
            assert_eq!(sent_text(&transformed), snippet, "{completed}");
            assert_eq!(
                transformed.insert_text,
                completed["insertText"].as_str().map(str::to_string),
                "{completed}"
            );
            assert_eq!(
                transformed.insert_text_format,
                Some(if snippet.contains(['$', '\\']) {
                    InsertTextFormat::SNIPPET
                } else {
                    InsertTextFormat::PLAIN_TEXT
                }),
                "{completed}"
            );
        }
        // As is, a snippet is the client's to expand.
        let mut as_is = item(json!({"label": "x", "insertTextFormat": 2, "insertText": snippet}));
        assert!(transform_completion_item(
            &mut as_is,
            &RegionOffset::new(10, 0),
            Position::new(11, 0),
            Some(Position::new(10, 3)),
            ClientInsertMode::default(),
        ));
        assert_eq!(as_is.insert_text_format, Some(InsertTextFormat::SNIPPET));
        assert_eq!(as_is.insert_text.as_deref(), Some(snippet));
    }

    /// A snippet breaking a line or holding a tab is refused whatever the
    /// client declares: snippet engines indent the lines after the first
    /// by the caret line's and turn tabs into spaces whatever the item asks
    /// (Neovim's vim.snippet.expand; mini.snippets, behind a completion
    /// declaring it inserts as is), so the text checked is not the text
    /// inserted. A literal one is plain text, inserted as is.
    #[rstest]
    #[case::tabstop_only_line("{\n$0\n}")]
    #[case::placeholder_then_tabstop("if ${1:x}:\n\t$0")]
    #[case::placeholder_spanning_lines("${1:b\nc}\nd")]
    #[case::mirrored_line_breaking_default("${1:b\nc}$1")]
    #[case::tab("f(\t$1)")]
    fn a_prepared_snippet_breaking_a_line_or_holding_a_tab_is_refused_for_every_client(
        #[case] snippet: &str,
    ) {
        use tower_lsp_server::ls_types::{InsertTextFormat, InsertTextMode};
        let offset = dedented_offset("  a\n");
        let edit_of = |new_text: &str| {
            item(json!({"label": "x", "insertTextFormat": 2, "textEdit": {
                "range": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 1}},
                "newText": new_text
            }}))
        };
        let transform = |completed: &mut CompletionItem, insert_mode| {
            transform_completion_item(
                completed,
                &offset,
                Position::new(11, 0),
                Some(Position::new(10, 3)),
                insert_mode,
            )
        };
        for insert_mode in [
            ClientInsertMode::AsIsSettable,
            ClientInsertMode::AsIs,
            ClientInsertMode::AdjustsIndentation,
        ] {
            assert!(
                !transform(&mut edit_of(snippet), insert_mode),
                "{insert_mode:?}"
            );
        }
        let mut literal = edit_of("{\n}");
        assert!(transform(&mut literal, ClientInsertMode::AsIsSettable));
        assert_eq!(sent_text(&literal), "{\n  }");
        assert_eq!(
            literal.insert_text_format,
            Some(InsertTextFormat::PLAIN_TEXT)
        );
        assert_eq!(literal.insert_text_mode, Some(InsertTextMode::AS_IS));
    }

    /// On tab-indented content the indent a one-line snippet regains is a
    /// tab, which snippet engines may turn into spaces short of its column:
    /// refused for every client; a literal one goes out as plain text.
    #[test]
    fn a_snippet_regaining_a_tab_indent_is_refused_for_every_client() {
        use crate::lsp::bridge::protocol::{VirtualLayout, apply_prepare_result};
        // `\ta\n\n\tb\n`, dedented by a tab; the caret on the blank line.
        let virtual_text = "\ta\n\n\tb\n";
        let answer = serde_json::from_value(json!({"segments": [{"type": "content", "changes": [
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 1}}, "newText": ""}
        ]}]}))
        .unwrap();
        let map = apply_prepare_result(
            virtual_text,
            &VirtualLayout::single(virtual_text),
            Some(answer),
        )
        .unwrap()
        .map;
        let offset = RegionOffset::new(10, 0).with_prepared(map);
        let transform = |completed: &mut CompletionItem, insert_mode| {
            transform_completion_item(
                completed,
                &offset,
                Position::new(13, 0),
                Some(Position::new(11, 0)),
                insert_mode,
            )
        };
        let snippet = json!({"label": "f", "insertTextFormat": 2, "insertText": "f(${1:x})$0"});
        for insert_mode in [
            ClientInsertMode::AsIsSettable,
            ClientInsertMode::AsIs,
            ClientInsertMode::AdjustsIndentation,
        ] {
            assert!(
                !transform(&mut item(snippet.clone()), insert_mode),
                "{insert_mode:?}"
            );
        }
        let mut literal = item(json!({"label": "f", "insertTextFormat": 2, "insertText": "f()"}));
        assert!(transform(&mut literal, ClientInsertMode::AsIs));
        assert_eq!(sent_text(&literal), "\tf()");
    }

    /// A snippet expanding to nothing but spaces is refused: the user types
    /// at its tabstop, which on a blank line among dedented ones would
    /// regain no indent. One expanding to text regains it before the
    /// tabstop.
    #[rstest]
    #[case::tabstop("$1", None)]
    #[case::braced_tabstop("${1}", None)]
    #[case::final_tabstop("$0", None)]
    #[case::space_default("${1: }", None)]
    #[case::text_after("$1x", Some("  $1x"))]
    #[case::default("${1:x}", Some("  ${1:x}"))]
    fn a_prepared_snippet_expanding_to_spaces_only_is_refused(
        #[case] snippet: &str,
        #[case] sent: Option<&str>,
    ) {
        // A blank line among dedented ones, with a textEdit and without.
        let offset = dedented_offset("  a\n\n  b\n");
        for completed in [
            json!({"label": "x", "insertTextFormat": 2, "textEdit": {
                "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 0}},
                "newText": snippet
            }}),
            json!({"label": "x", "insertTextFormat": 2, "insertText": snippet}),
        ] {
            let mut transformed = item(completed.clone());
            let kept = transform_completion_item(
                &mut transformed,
                &offset,
                Position::new(13, 0),
                Some(Position::new(11, 0)),
                ClientInsertMode::default(),
            );
            assert_eq!(kept.then(|| sent_text(&transformed)), sent, "{completed}");
        }
    }

    /// Without a textEdit, a snippet inserted at a caret regaining an
    /// indent goes out as that edit, the snippet in it.
    #[test]
    fn an_implicit_snippet_made_an_edit_keeps_its_tabstops() {
        use tower_lsp_server::ls_types::{CompletionTextEdit, InsertTextFormat};
        // A blank line among dedented ones: the caret there regains the indent.
        let offset = dedented_offset("  a\n\n  b\n");
        let mut completed =
            item(json!({"label": "f", "insertTextFormat": 2, "insertText": "f(${1:x})$0"}));
        assert!(transform_completion_item(
            &mut completed,
            &offset,
            Position::new(13, 0),
            Some(Position::new(11, 0)),
            ClientInsertMode::default(),
        ));
        let Some(CompletionTextEdit::Edit(edit)) = &completed.text_edit else {
            panic!("made an edit: {completed:?}");
        };
        assert_eq!(edit.new_text, "  f(${1:x})$0");
        assert_eq!(completed.insert_text.as_deref(), Some("f(${1:x})$0"));
        assert_eq!(
            completed.insert_text_format,
            Some(InsertTextFormat::SNIPPET)
        );
    }

    #[test]
    fn a_prepared_item_must_contain_the_requested_caret() {
        use crate::lsp::bridge::protocol::{SegmentKind, VirtualLayout, apply_prepare_result};
        // `foo${x}bar` at host line 10, the interpolation emptied: `foobar`.
        let virtual_text = "foo    bar\n";
        let layout = VirtualLayout::from_pieces(
            virtual_text,
            [
                (SegmentKind::Content, 0..3, String::new()),
                (SegmentKind::Gap, 3..7, "${x}".to_string()),
                (SegmentKind::Content, 7..11, String::new()),
            ],
        );
        let result = serde_json::from_value(json!({"segments": [
            {"type": "content"}, {"type": "gap", "content": ""}, {"type": "content"}
        ]}))
        .unwrap();
        let map = apply_prepare_result(virtual_text, &layout, Some(result))
            .unwrap()
            .map;
        let offset = RegionOffset::new(10, 0).with_prepared(map);
        let region_end = Position::new(11, 0);
        // Both carets reach the server at P (0, 3); its insertion there maps
        // before `${x}`.
        let completed = || {
            item(json!({
                "label": "X",
                "textEdit": {
                    "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}},
                    "newText": "X"
                }
            }))
        };
        assert!(
            !transform_completion_item(
                &mut completed(),
                &offset,
                region_end,
                Some(Position::new(10, 7)),
                ClientInsertMode::default()
            ),
            "the edit lands before the gap, short of the caret after it"
        );
        assert!(transform_completion_item(
            &mut completed(),
            &offset,
            region_end,
            Some(Position::new(10, 3)),
            ClientInsertMode::default()
        ));
    }

    /// `foo${x}bar` at host line 10, the interpolation emptied: `foobar`.
    fn emptied_gap_offset() -> RegionOffset {
        use crate::lsp::bridge::protocol::{SegmentKind, VirtualLayout, apply_prepare_result};
        let virtual_text = "foo    bar\n";
        let layout = VirtualLayout::from_pieces(
            virtual_text,
            [
                (SegmentKind::Content, 0..3, String::new()),
                (SegmentKind::Gap, 3..7, "${x}".to_string()),
                (SegmentKind::Content, 7..11, String::new()),
            ],
        );
        let result = serde_json::from_value(json!({"segments": [
            {"type": "content"}, {"type": "gap", "content": ""}, {"type": "content"}
        ]}))
        .unwrap();
        let map = apply_prepare_result(virtual_text, &layout, Some(result))
            .unwrap()
            .map;
        RegionOffset::new(10, 0).with_prepared(map)
    }

    #[test]
    fn insert_replace_ranges_mapped_to_different_starts_are_dropped() {
        // At the emptied interpolation: the insert range maps before its
        // host text, the replace range over `bar` after it. Realigned, the
        // replace range would swallow `${x}`.
        let mut completed = item(json!({
            "label": "X",
            "textEdit": {
                "insert": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}},
                "replace": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 6}},
                "newText": "X"
            }
        }));
        assert!(!transform_completion_item(
            &mut completed,
            &emptied_gap_offset(),
            Position::new(11, 0),
            Some(Position::new(10, 3)),
            ClientInsertMode::default(),
        ));
    }

    #[test]
    fn insert_replace_ranges_must_contain_the_caret_once_mapped() {
        // Both ranges empty at the emptied interpolation map before its host
        // text, short of a caret after it.
        let mut completed = item(json!({
            "label": "X",
            "textEdit": {
                "insert": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}},
                "replace": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}},
                "newText": "X"
            }
        }));
        assert!(!transform_completion_item(
            &mut completed,
            &emptied_gap_offset(),
            Position::new(11, 0),
            Some(Position::new(10, 7)),
            ClientInsertMode::default(),
        ));
    }

    /// An item without a textEdit is made one only from single-line text:
    /// multi-line text is dropped before the caret probe, so the edit it is
    /// given never spans lines, and no client insert mode re-indents it.
    #[rstest]
    #[case::as_is_settable(ClientInsertMode::AsIsSettable)]
    #[case::adjusting(ClientInsertMode::AdjustsIndentation)]
    #[case::as_is(ClientInsertMode::AsIs)]
    fn an_implicit_item_is_made_an_edit_only_on_one_line(#[case] insert_mode: ClientInsertMode) {
        // A blank line among dedented ones: the caret there regains the indent.
        let offset = dedented_offset("  a\n\n  b\n");
        let transform = |completed: &mut CompletionItem| {
            transform_completion_item(
                completed,
                &offset,
                Position::new(13, 0),
                Some(Position::new(11, 0)),
                insert_mode,
            )
        };
        let mut single = item(json!({"label": "foo"}));
        assert!(transform(&mut single));
        let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(edit)) = &single.text_edit
        else {
            panic!("made an edit: {single:?}");
        };
        assert_eq!(edit.new_text, "  foo");
        assert_eq!(single.insert_text_mode, None);
        let mut multi = item(json!({"label": "x", "insertText": "if a:\n  b"}));
        assert!(!transform(&mut multi));
    }

    /// A caret on a blank line the peer emptied of a space shorter than
    /// the content's indent: text typed there regains the content's indent,
    /// replacing that space. An item without a textEdit goes out as that
    /// edit, its range on the caret's line and containing the caret; one
    /// with a textEdit at the caret maps to the same edit; multi-line text
    /// is dropped as anywhere on a prepared document.
    #[test]
    fn an_item_on_an_emptied_blank_line_replaces_its_whitespace() {
        use crate::lsp::bridge::protocol::{VirtualLayout, apply_prepare_result};
        use tower_lsp_server::ls_types::{CompletionTextEdit, Range, TextEdit};
        let virtual_text = "  a\n \n  b\n";
        let answer = serde_json::from_value(json!({"segments": [{"type": "content", "changes": [
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
        ]}]}))
        .unwrap();
        let map = apply_prepare_result(
            virtual_text,
            &VirtualLayout::single(virtual_text),
            Some(answer),
        )
        .unwrap()
        .map;
        let offset = RegionOffset::new(10, 0).with_prepared(map);
        let caret = Position::new(11, 1);
        let transform = |completed: &mut CompletionItem| {
            transform_completion_item(
                completed,
                &offset,
                Position::new(13, 0),
                Some(caret),
                ClientInsertMode::default(),
            )
        };
        let expected = Some(CompletionTextEdit::Edit(TextEdit {
            range: Range::new(Position::new(11, 0), Position::new(11, 1)),
            new_text: "  z".to_string(),
        }));
        let mut implicit = item(json!({"label": "z"}));
        assert!(transform(&mut implicit));
        assert_eq!(implicit.text_edit, expected);
        let mut explicit = item(json!({"label": "z", "textEdit": {
            "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 0}},
            "newText": "z"
        }}));
        assert!(transform(&mut explicit));
        assert_eq!(explicit.text_edit, expected);
        let mut multi = item(json!({"label": "x", "insertText": "if a:\n  b"}));
        assert!(!transform(&mut multi));
    }

    /// An item without a textEdit that goes out as the caret probe's edit
    /// keeps its additional edits (an auto-import) checked with that edit,
    /// as for an item with one; left inserted at the caret, it drops them.
    #[test]
    fn an_implicit_item_made_an_edit_keeps_its_additional_edits() {
        use tower_lsp_server::ls_types::{CompletionTextEdit, Range, TextEdit};
        // A blank line among dedented ones: the probe regains the indent.
        let offset = dedented_offset("  a\n\n  b\n");
        let mut completed = item(json!({
            "label": "foo",
            "additionalTextEdits": [{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                "newText": "use x;\n"
            }]
        }));
        assert!(transform_completion_item(
            &mut completed,
            &offset,
            Position::new(13, 0),
            Some(Position::new(11, 0)),
            ClientInsertMode::default(),
        ));
        assert_eq!(
            completed.text_edit,
            Some(CompletionTextEdit::Edit(TextEdit {
                range: Range::new(Position::new(11, 0), Position::new(11, 0)),
                new_text: "  foo".to_string(),
            }))
        );
        assert_eq!(
            completed.additional_text_edits,
            // After line 0's indent, regaining it for the line it keeps.
            Some(vec![TextEdit {
                range: Range::new(Position::new(10, 2), Position::new(10, 2)),
                new_text: "use x;\n  ".to_string(),
            }])
        );
    }

    #[test]
    fn an_implicit_item_whose_caret_edit_maps_off_the_caret_is_dropped() {
        // The caret right after the emptied interpolation's host text: the
        // label inserted there as an edit maps before that text, so the
        // client inserting at the caret would insert other text than checked.
        let mut completed = item(json!({"label": "q"}));
        assert!(!transform_completion_item(
            &mut completed,
            &emptied_gap_offset(),
            Position::new(11, 0),
            Some(Position::new(10, 7)),
            ClientInsertMode::default(),
        ));
    }

    /// Unprepared, a snippet goes as the server sent it, its ranges
    /// translated only, whatever it holds and the client declares: no
    /// expansion, no indent, no insert mode set, a variable refused only
    /// where a prefixed line would take its expansion.
    #[rstest]
    #[case::as_is_settable(ClientInsertMode::AsIsSettable)]
    #[case::as_is(ClientInsertMode::AsIs)]
    #[case::adjusting(ClientInsertMode::AdjustsIndentation)]
    fn an_unprepared_snippet_goes_as_the_server_sent_it(#[case] insert_mode: ClientInsertMode) {
        use tower_lsp_server::ls_types::{CompletionTextEdit, InsertTextFormat, Range};
        let offset = RegionOffset::new(10, 2);
        let transform = |completed: &mut CompletionItem| {
            transform_completion_item(
                completed,
                &offset,
                Position::new(13, 0),
                Some(Position::new(11, 0)),
                insert_mode,
            )
        };
        for (snippet, text_edit) in [
            ("do\n\t${1:body}\n$0end", true),
            ("{\n$0\n}", true),
            ("\tf(${1|a,b|})$0$0", true),
            ("f(${1:x})$0", false),
            ("cost: $ ${1:a\\qb}", false),
        ] {
            let mut completed = item(if text_edit {
                json!({"label": "s", "insertTextFormat": 2, "textEdit": {
                    "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 0}},
                    "newText": snippet
                }})
            } else {
                json!({"label": "s", "insertTextFormat": 2, "insertText": snippet})
            });
            let came = completed.clone();
            assert!(transform(&mut completed), "{snippet:?}");
            let mut expected = came;
            if let Some(CompletionTextEdit::Edit(edit)) = &mut expected.text_edit {
                edit.range = Range::new(Position::new(11, 0), Position::new(11, 0));
            }
            assert_eq!(completed, expected, "{snippet:?}");
            assert_eq!(completed.insert_text_mode, None);
            assert_eq!(
                completed.insert_text_format,
                Some(InsertTextFormat::SNIPPET)
            );
        }
    }

    /// An insert/replace snippet maps through both ranges as its expansion,
    /// and goes out as the snippet over both.
    #[test]
    fn a_prepared_insert_replace_snippet_maps_both_ranges() {
        use tower_lsp_server::ls_types::{CompletionTextEdit, Range};
        let offset = dedented_offset("  foo\n");
        let mut completed = item(json!({"label": "f", "insertTextFormat": 2, "textEdit": {
            "insert": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "replace": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
            "newText": "f(${1:x})$0"
        }}));
        assert!(transform_completion_item(
            &mut completed,
            &offset,
            Position::new(11, 0),
            Some(Position::new(10, 3)),
            ClientInsertMode::default(),
        ));
        let Some(CompletionTextEdit::InsertAndReplace(edit)) = &completed.text_edit else {
            panic!("kept as insert/replace: {completed:?}");
        };
        assert_eq!(edit.new_text, "f(${1:x})$0");
        assert_eq!(
            edit.insert,
            Range::new(Position::new(10, 2), Position::new(10, 3))
        );
        assert_eq!(
            edit.replace,
            Range::new(Position::new(10, 2), Position::new(10, 5))
        );
    }

    #[test]
    fn prepared_insert_replace_edits_keep_their_shared_start() {
        // Clearing the whole content of a dedented line (`if x:`): the
        // replace range covers it, the insert range is empty at its start.
        let mut edit = tower_lsp_server::ls_types::InsertReplaceEdit {
            new_text: String::new(),
            insert: tower_lsp_server::ls_types::Range::new(
                Position::new(0, 0),
                Position::new(0, 0),
            ),
            replace: tower_lsp_server::ls_types::Range::new(
                Position::new(0, 0),
                Position::new(0, 5),
            ),
        };
        assert_eq!(
            translate_insert_replace_edit_to_host(
                &mut edit,
                None,
                &dedented_offset("  if x:\n    y\n")
            ),
            Some(true)
        );
        assert_eq!(edit.insert.start, edit.replace.start);
        assert_eq!(
            edit.replace,
            tower_lsp_server::ls_types::Range::new(Position::new(10, 2), Position::new(10, 7))
        );
    }

    #[test]
    fn prepared_insert_replace_edits_map_both_ranges_and_reindent() {
        // `y` sits at P (1, 2): insert at its start, replace the word.
        let mut edit = tower_lsp_server::ls_types::InsertReplaceEdit {
            new_text: "yes\n  z".to_string(),
            insert: tower_lsp_server::ls_types::Range::new(
                Position::new(1, 2),
                Position::new(1, 2),
            ),
            replace: tower_lsp_server::ls_types::Range::new(
                Position::new(1, 2),
                Position::new(1, 3),
            ),
        };
        assert_eq!(
            translate_insert_replace_edit_to_host(
                &mut edit,
                None,
                &dedented_offset("  if x:\n    y\n")
            ),
            Some(true)
        );
        assert_eq!(edit.insert.start, Position::new(11, 4));
        assert_eq!(edit.replace.end, Position::new(11, 5));
        assert_eq!(edit.new_text, "yes\n    z");
    }

    #[test]
    fn prepared_insert_replace_ranges_sent_with_unequal_starts_are_refused() {
        // `ab`, dedented: the replace range starts before the insert range
        // (against LSP). Moving the mapped replace start up to the insert
        // start would send a replacement the server never asked for, and
        // which no check saw.
        let mut edit = tower_lsp_server::ls_types::InsertReplaceEdit {
            new_text: "x".to_string(),
            insert: tower_lsp_server::ls_types::Range::new(
                Position::new(0, 1),
                Position::new(0, 2),
            ),
            replace: tower_lsp_server::ls_types::Range::new(
                Position::new(0, 0),
                Position::new(0, 2),
            ),
        };
        assert_eq!(
            translate_insert_replace_edit_to_host(&mut edit, None, &dedented_offset("  ab\n  c\n")),
            None
        );
    }

    #[test]
    fn insert_replace_ranges_mapping_the_text_apart_are_refused() {
        // `ab`, dedented: inserted before `b` the line break is followed by
        // `b`, whose line regains the indent; replacing `b` it ends the
        // line. The shared text would differ between the client's choices.
        let mut edit = tower_lsp_server::ls_types::InsertReplaceEdit {
            new_text: "x\n".to_string(),
            insert: tower_lsp_server::ls_types::Range::new(
                Position::new(0, 1),
                Position::new(0, 1),
            ),
            replace: tower_lsp_server::ls_types::Range::new(
                Position::new(0, 1),
                Position::new(0, 2),
            ),
        };
        assert_eq!(
            translate_insert_replace_edit_to_host(&mut edit, None, &dedented_offset("  ab\n  c\n")),
            None
        );
    }

    #[test]
    fn unprepared_insert_replace_ranges_stay_as_sent() {
        // Unequal starts, as a server may send them: only translated.
        use tower_lsp_server::ls_types::{CompletionTextEdit, Range};
        let mut completed = item(json!({"label": "x", "textEdit": {
            "insert": {"start": {"line": 0, "character": 2}, "end": {"line": 0, "character": 3}},
            "replace": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}},
            "newText": "x"
        }}));
        assert!(transform_completion_item(
            &mut completed,
            &RegionOffset::new(10, 4),
            Position::new(12, 0),
            Some(Position::new(10, 6)),
            ClientInsertMode::default(),
        ));
        let Some(CompletionTextEdit::InsertAndReplace(edit)) = completed.text_edit else {
            panic!("an insert/replace edit");
        };
        assert_eq!(
            edit.insert,
            Range::new(Position::new(10, 6), Position::new(10, 7))
        );
        assert_eq!(
            edit.replace,
            Range::new(Position::new(10, 4), Position::new(10, 9))
        );
        assert_eq!(edit.new_text, "x");
    }

    #[test]
    fn additional_edits_must_map_alike_with_either_range() {
        // `ab\n` dedented. An additional edit right after `b`: apart from the
        // insert range (empty before `b`), and meeting the replace range over
        // `b` mid-line between text — both map it alike.
        let offset = dedented_offset("  ab\n");
        let completed = |additional: &str| {
            item(json!({
                "label": "bc",
                "textEdit": {
                    "insert": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 1}},
                    "replace": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 2}},
                    "newText": "bc"
                },
                "additionalTextEdits": [{
                    "range": {"start": {"line": 0, "character": 2}, "end": {"line": 0, "character": 2}},
                    "newText": additional
                }]
            }))
        };
        let mut kept = completed("z");
        assert!(transform_completion_item(
            &mut kept,
            &offset,
            Position::new(11, 0),
            Some(Position::new(10, 3)),
            ClientInsertMode::default(),
        ));
        assert_eq!(
            kept.additional_text_edits,
            Some(vec![tower_lsp_server::ls_types::TextEdit {
                range: tower_lsp_server::ls_types::Range::new(
                    Position::new(10, 4),
                    Position::new(10, 4)
                ),
                new_text: "z".to_string(),
            }])
        );
        // One opening with a line break still maps with the insert range,
        // but meets the replace range with that break, where the set is
        // refused: refused with one range, the additional edits are dropped.
        let mut dropped = completed("\nq");
        assert!(transform_completion_item(
            &mut dropped,
            &offset,
            Position::new(11, 0),
            Some(Position::new(10, 3)),
            ClientInsertMode::default(),
        ));
        assert_eq!(dropped.additional_text_edits, None);
    }

    /// An envelope minted from a prepared document never resolves, even
    /// against a live offset equal to its own (the region's pair lost its
    /// peer since): the map it was minted through is not carried. The mark
    /// survives the round trip through the client.
    #[test]
    fn an_envelope_from_a_prepared_document_describes_no_live_offset() {
        use crate::lsp::bridge::protocol::{VirtualLayout, apply_prepare_result};
        let map = apply_prepare_result("a", &VirtualLayout::single("a"), None)
            .unwrap()
            .map;
        let live = RegionOffset::new(3, 2);
        let as_is = EnvelopeOffset::from(&live);
        assert!(as_is.describes(&live));
        let prepared = EnvelopeOffset::from(&live.clone().with_prepared(map));
        assert!(!prepared.describes(&live));
        let round_trip: EnvelopeOffset =
            serde_json::from_value(serde_json::to_value(&prepared).unwrap()).unwrap();
        assert!(!round_trip.describes(&live));
    }

    /// Run the virt transform for a NON-resolving origin, the shape most
    /// coordinate tests want: ranges translate, `data` passes through bare.
    /// (The codeLens and documentLink twins default the other way — their
    /// helper is named for a resolving origin.)
    fn transform_for_non_resolving_server(
        response: serde_json::Value,
        offset: &RegionOffset,
        region_end: Position,
        request_host_line: Option<u32>,
    ) -> Option<CompletionList> {
        transform_with_resolve_support(response, offset, region_end, request_host_line, false)
    }

    fn transform_with_resolve_support(
        response: serde_json::Value,
        offset: &RegionOffset,
        region_end: Position,
        request_host_line: Option<u32>,
        server_resolves: bool,
    ) -> Option<CompletionList> {
        transform_completion_response_to_host(
            response,
            offset,
            region_end,
            request_host_line.map(|line| Position::new(line, 0)),
            server_resolves,
            &EnvelopeContext {
                server_name: "lua-ls",
                injection_language: "lua",
                incarnation: Some(1),
                host_uri: "file:///test.md",
                region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                offset,
                region_end: Some(region_end),
                host_layer: false,
            },
            ClientInsertMode::default(),
        )
    }

    #[test]
    fn an_incomplete_list_whose_items_all_drop_stays_an_incomplete_list() {
        // As is, and prepared alike: the virt layer, not this transform,
        // decides what a prepared document's empty incomplete list counts as.
        let response = json!({"jsonrpc": "2.0", "id": 1, "result": {
            "isIncomplete": true,
            "items": [{"label": "x", "textEdit": {
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 9, "character": 0}},
                "newText": "x"
            }}]
        }});
        for offset in [
            RegionOffset::new(10, 0),
            RegionOffset::new(10, 0).with_prepared(dedented_map("  a\n")),
        ] {
            let list = transform_with_resolve_support(
                response.clone(),
                &offset,
                Position::new(11, 0),
                Some(10),
                false,
            )
            .expect("a list");
            assert!(list.is_incomplete && list.items.is_empty(), "{list:?}");
        }
    }

    /// The server ranked first keeps its place though its reply came while
    /// the host had moved (an edit to another fence, its pass not settled
    /// yet) and the next one's came once it settled with the content
    /// unchanged: whether the host moved is read once, where the answer is
    /// chosen, not per reply, where it would hand the region to the next.
    #[tokio::test]
    async fn the_server_ranked_first_wins_though_its_reply_came_before_the_host_settled() {
        use crate::lsp::aggregation::server::priority::PriorityEntry;
        use crate::lsp::aggregation::server::{FanInResult, TaggedResult, preferred};
        let map = dedented_map("  a\n");
        let offset = RegionOffset::new(10, 0).with_prepared(std::sync::Arc::clone(&map));
        let pool = crate::lsp::bridge::LanguageServerPool::preparing();
        let host = url::Url::parse("file:///test/two-fences.md").unwrap();
        let host_lsp = crate::lsp::lsp_impl::url_to_uri(&host).unwrap();
        let region = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        pool.open_host_incarnation(&host, 1).await;
        let prepared = crate::lsp::bridge::protocol::PreparedDocument {
            text: "a\n".to_string(),
            map: std::sync::Arc::clone(&map),
        };
        pool.record_latest_prepared_for_test(&host, 1, "lua", region, &prepared);
        let reply = |label: &str| {
            let ctx = pool.response_context_for_test(&host_lsp, &offset);
            transform_completion_response_to_host(
                json!({"jsonrpc": "2.0", "id": 1, "result": [{"label": label, "textEdit": {
                    "range": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 1}},
                    "newText": label
                }}]}),
                ctx.offset,
                Position::new(11, 0),
                Some(Position::new(10, 3)),
                false,
                &EnvelopeContext {
                    server_name: label,
                    injection_language: "lua",
                    incarnation: Some(1),
                    host_uri: "file:///test/two-fences.md",
                    region_id: region,
                    offset: &offset,
                    region_end: Some(Position::new(11, 0)),
                    host_layer: false,
                },
                ClientInsertMode::default(),
            )
        };
        // Another fence edited, its pass not settled: the top server replies.
        pool.note_host_edited(&host, 1, 1);
        let top = reply("top");
        // Settled, this region's text unchanged: the next server replies.
        pool.note_host_settled(&host, 1, 1);
        let next = reply("next");
        assert!(next.is_some());
        let mut tasks = tokio::task::JoinSet::new();
        for (server_name, value) in [("next", next), ("top", top)] {
            tasks.spawn(async move {
                TaggedResult {
                    server_name: server_name.to_string(),
                    value: Ok(value),
                }
            });
        }
        let result = preferred(
            &mut tasks,
            |list: &Option<CompletionList>| {
                list.as_ref().is_some_and(|list| !list.items.is_empty())
            },
            &[
                PriorityEntry::Server("top".to_string()),
                PriorityEntry::Server("next".to_string()),
            ],
            None,
        )
        .await;
        let FanInResult::Done(Some(list)) = result else {
            panic!("an answer");
        };
        assert_eq!(list.items[0].label, "top");
    }

    #[test]
    fn virt_items_are_enveloped_only_when_the_server_resolves() {
        let response = |data: serde_json::Value| {
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": [{
                    "label": "print",
                    "textEdit": {
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 2 }
                        },
                        "newText": "print"
                    },
                    "data": data
                }]
            })
        };
        let offset = RegionOffset::new(3, 2);

        let resolvable = transform_with_resolve_support(
            response(json!({"token": 1})),
            &offset,
            TEST_REGION_END,
            None,
            true,
        )
        .unwrap();
        let envelope = extract_envelope(&resolvable.items[0]).expect("resolving origin envelopes");
        assert_eq!(envelope.origin, "lua-ls");
        assert_eq!(envelope.inner, Some(json!({"token": 1})));

        let bare = transform_for_non_resolving_server(
            response(json!({"token": 1})),
            &offset,
            TEST_REGION_END,
            None,
        )
        .unwrap();
        assert_eq!(
            bare.items[0].data,
            Some(json!({"token": 1})),
            "a non-resolving origin's payload must pass through untouched: the resolve \
             would only fail soft back to it, so the envelope is pure wire weight"
        );
        let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(edit)) =
            &bare.items[0].text_edit
        else {
            panic!("the bare item keeps its edit");
        };
        assert_eq!(
            (edit.range.start.line, edit.range.start.character),
            (3, 2),
            "ranges are still translated for a bare item"
        );

        let forged = transform_for_non_resolving_server(
            response(json!({ ENVELOPE_KEY: { "origin": "spoofed" } })),
            &offset,
            TEST_REGION_END,
            None,
        )
        .unwrap();
        let envelope = extract_envelope(&forged.items[0]).expect("collision envelope");
        assert_eq!(envelope.origin, "lua-ls");
        assert_eq!(
            envelope.inner,
            Some(json!({ ENVELOPE_KEY: { "origin": "spoofed" } })),
            "the foreign payload must be nested, not honored as routing metadata"
        );
    }

    // ==========================================================================
    // Completion request tests
    // ==========================================================================

    #[test]
    fn completion_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_completion_request(
            &virtual_uri,
            test_position(),
            &RegionOffset::new(3, 0),
            test_request_id(),
        );

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn completion_request_translates_position_to_virtual_coordinates() {
        // Host line 5, region starts at line 3 -> virtual line 2
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_completion_request(
            &virtual_uri,
            test_position(),
            &RegionOffset::new(3, 0),
            test_request_id(),
        );

        assert_position_request(&request, "textDocument/completion", 2);
    }

    // ==========================================================================
    // Completion response transformation tests
    // ==========================================================================

    #[test]
    fn completion_response_transforms_textedit_ranges() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [
                    {
                        "label": "print",
                        "kind": 3,
                        "textEdit": {
                            "range": {
                                "start": { "line": 1, "character": 0 },
                                "end": { "line": 1, "character": 3 }
                            },
                            "newText": "print"
                        }
                    },
                    { "label": "pairs", "kind": 3 }
                ]
            }
        });
        let region_start_line = 3;

        let transformed = transform_for_non_resolving_server(
            response,
            &RegionOffset::new(region_start_line, 0),
            TEST_REGION_END,
            None,
        );

        assert!(transformed.is_some());
        let list = transformed.unwrap();
        assert_eq!(list.items.len(), 2);

        // First item should have transformed range
        let item = &list.items[0];
        assert_eq!(item.label, "print");
        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(ref edit)) = item.text_edit
        {
            assert_eq!(edit.range.start.line, 4); // 1 + 3 = 4
            assert_eq!(edit.range.end.line, 4);
        } else {
            panic!("Expected TextEdit");
        }

        // Second item has no textEdit
        assert_eq!(list.items[1].label, "pairs");
        assert!(list.items[1].text_edit.is_none());
    }

    #[rstest]
    #[case::null_result(json!({"jsonrpc": "2.0", "id": 42, "result": null}))]
    #[case::without_result(json!({"jsonrpc": "2.0", "id": 42}))]
    #[case::malformed_result(json!({"jsonrpc": "2.0", "id": 42, "result": "not_a_completion_response"}))]
    #[case::error_response(json!({"jsonrpc": "2.0", "id": 42, "error": {"code": -32600, "message": "Invalid Request"}}))]
    fn completion_response_returns_none_for_invalid_response(#[case] response: serde_json::Value) {
        let transformed = transform_for_non_resolving_server(
            response,
            &RegionOffset::new(3, 0),
            TEST_REGION_END,
            None,
        );
        assert!(transformed.is_none());
    }

    #[test]
    fn completion_response_handles_array_format() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "label": "print",
                "textEdit": {
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 2 }
                    },
                    "newText": "print"
                }
            }]
        });
        let region_start_line = 5;

        let transformed = transform_for_non_resolving_server(
            response,
            &RegionOffset::new(region_start_line, 0),
            TEST_REGION_END,
            None,
        );

        assert!(transformed.is_some());
        let list = transformed.unwrap();
        // Array format is normalized to CompletionList with isIncomplete=false
        assert!(!list.is_incomplete);
        assert_eq!(list.items.len(), 1);

        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(ref edit)) =
            list.items[0].text_edit
        {
            assert_eq!(edit.range.start.line, 5); // 0 + 5 = 5
            assert_eq!(edit.range.end.line, 5);
        } else {
            panic!("Expected TextEdit");
        }
    }

    #[test]
    fn completion_response_transforms_insert_replace_edit() {
        // InsertReplaceEdit format used by rust-analyzer, tsserver, etc.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "println!",
                    "textEdit": {
                        "insert": {
                            "start": { "line": 2, "character": 0 },
                            "end": { "line": 2, "character": 3 }
                        },
                        "replace": {
                            "start": { "line": 2, "character": 0 },
                            "end": { "line": 2, "character": 8 }
                        },
                        "newText": "println!"
                    }
                }]
            }
        });
        let region_start_line = 10;

        let transformed = transform_for_non_resolving_server(
            response,
            &RegionOffset::new(region_start_line, 0),
            TEST_REGION_END,
            None,
        );

        assert!(transformed.is_some());
        let list = transformed.unwrap();

        let item = &list.items[0];
        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::InsertAndReplace(ref edit)) =
            item.text_edit
        {
            // Insert range transformed: line 2 + 10 = 12
            assert_eq!(edit.insert.start.line, 12);
            assert_eq!(edit.insert.end.line, 12);
            // Replace range transformed: line 2 + 10 = 12
            assert_eq!(edit.replace.start.line, 12);
            assert_eq!(edit.replace.end.line, 12);
        } else {
            panic!("Expected InsertReplaceEdit");
        }
    }

    #[test]
    fn completion_response_transforms_additional_text_edits() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "import",
                    "additionalTextEdits": [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 0 }
                        },
                        "newText": "import module\n"
                    }]
                }]
            }
        });
        let region_start_line = 5;

        let transformed = transform_for_non_resolving_server(
            response,
            &RegionOffset::new(region_start_line, 0),
            TEST_REGION_END,
            None,
        );

        assert!(transformed.is_some());
        let list = transformed.unwrap();

        let item = &list.items[0];
        assert!(item.additional_text_edits.is_some());
        let edits = item.additional_text_edits.as_ref().unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].range.start.line, 5); // 0 + 5 = 5
    }

    // ==========================================================================
    // Column offset tests for completion response transformation
    // ==========================================================================

    #[test]
    fn completion_response_applies_column_offset_on_first_virtual_line() {
        // Item on virtual line 0 → column offset added to character positions
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "func",
                    "textEdit": {
                        "range": {
                            "start": { "line": 0, "character": 2 },
                            "end": { "line": 0, "character": 5 }
                        },
                        "newText": "function"
                    }
                }]
            }
        });

        let transformed = transform_for_non_resolving_server(
            response,
            &RegionOffset::new(10, 4),
            TEST_REGION_END,
            None,
        );

        let list = transformed.unwrap();
        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(ref edit)) =
            list.items[0].text_edit
        {
            assert_eq!(edit.range.start.line, 10); // 0 + 10
            assert_eq!(edit.range.start.character, 6); // 2 + 4 (first line)
            assert_eq!(edit.range.end.line, 10);
            assert_eq!(edit.range.end.character, 9); // 5 + 4 (first line)
        } else {
            panic!("Expected TextEdit");
        }
    }

    #[test]
    fn completion_response_ignores_column_offset_on_non_first_virtual_line() {
        // Item on virtual line 1 → only line offset, character unchanged
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "func",
                    "textEdit": {
                        "range": {
                            "start": { "line": 1, "character": 2 },
                            "end": { "line": 1, "character": 5 }
                        },
                        "newText": "function"
                    }
                }]
            }
        });

        let transformed = transform_for_non_resolving_server(
            response,
            &RegionOffset::new(10, 4),
            TEST_REGION_END,
            None,
        );

        let list = transformed.unwrap();
        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(ref edit)) =
            list.items[0].text_edit
        {
            assert_eq!(edit.range.start.line, 11); // 1 + 10
            assert_eq!(edit.range.start.character, 2); // unchanged
            assert_eq!(edit.range.end.line, 11);
            assert_eq!(edit.range.end.character, 5); // unchanged
        } else {
            panic!("Expected TextEdit");
        }
    }

    #[test]
    fn completion_response_column_offset_with_insert_replace_edit() {
        // InsertReplaceEdit on virtual line 0 → column offset on both ranges
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "println!",
                    "textEdit": {
                        "insert": {
                            "start": { "line": 0, "character": 3 },
                            "end": { "line": 0, "character": 6 }
                        },
                        "replace": {
                            "start": { "line": 0, "character": 3 },
                            "end": { "line": 0, "character": 10 }
                        },
                        "newText": "println!"
                    }
                }]
            }
        });

        let transformed = transform_for_non_resolving_server(
            response,
            &RegionOffset::new(5, 7),
            TEST_REGION_END,
            None,
        );

        let list = transformed.unwrap();
        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::InsertAndReplace(ref edit)) =
            list.items[0].text_edit
        {
            // Insert range: line 0→5, char +7
            assert_eq!(edit.insert.start.line, 5);
            assert_eq!(edit.insert.start.character, 10); // 3 + 7
            assert_eq!(edit.insert.end.line, 5);
            assert_eq!(edit.insert.end.character, 13); // 6 + 7
            // Replace range: line 0→5, char +7
            assert_eq!(edit.replace.start.line, 5);
            assert_eq!(edit.replace.start.character, 10); // 3 + 7
            assert_eq!(edit.replace.end.line, 5);
            assert_eq!(edit.replace.end.character, 17); // 10 + 7
        } else {
            panic!("Expected InsertReplaceEdit");
        }
    }

    #[test]
    fn completion_response_column_offset_with_additional_text_edits_on_first_line() {
        // additionalTextEdits on virtual line 0 → column offset applied
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "import",
                    "additionalTextEdits": [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 0 }
                        },
                        "newText": "use std::io;\n"
                    }]
                }]
            }
        });

        let transformed = transform_for_non_resolving_server(
            response,
            &RegionOffset::new(5, 3),
            TEST_REGION_END,
            None,
        );

        let list = transformed.unwrap();
        let edits = list.items[0].additional_text_edits.as_ref().unwrap();
        assert_eq!(edits[0].range.start.line, 5);
        assert_eq!(edits[0].range.start.character, 3); // 0 + 3
        assert_eq!(edits[0].range.end.line, 5);
        assert_eq!(edits[0].range.end.character, 3); // 0 + 3
    }

    #[test]
    fn completion_request_applies_column_offset_on_first_line() {
        // Host position on first line of region → column subtracted in request
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let host_pos = Position {
            line: 5,
            character: 14,
        };
        let request = build_completion_request(
            &virtual_uri,
            host_pos,
            &RegionOffset::new(5, 4),
            test_request_id(),
        );

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["position"]["line"], 0); // 5 - 5
        assert_eq!(json["params"]["position"]["character"], 10); // 14 - 4
    }

    #[test]
    fn completion_request_ignores_column_offset_on_non_first_line() {
        // Host position on non-first line → column NOT subtracted
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let host_pos = Position {
            line: 7,
            character: 14,
        };
        let request = build_completion_request(
            &virtual_uri,
            host_pos,
            &RegionOffset::new(5, 4),
            test_request_id(),
        );

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["position"]["line"], 2); // 7 - 5
        assert_eq!(json["params"]["position"]["character"], 14); // unchanged
    }

    #[test]
    fn completion_range_transformation_saturates_on_overflow() {
        // Test defensive arithmetic: saturating_add prevents panic on overflow
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "label": "test",
                "textEdit": {
                    "range": {
                        "start": { "line": 4294967295u32, "character": 0 },  // u32::MAX
                        "end": { "line": 4294967295u32, "character": 5 }
                    },
                    "newText": "test"
                }
            }]
        });
        let region_start_line = 10;

        let transformed = transform_for_non_resolving_server(
            response,
            &RegionOffset::new(region_start_line, 0),
            TEST_REGION_END,
            None,
        );

        assert!(transformed.is_some());
        let list = transformed.unwrap();
        // Array format is normalized to CompletionList with isIncomplete=false
        assert!(!list.is_incomplete);

        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(ref edit)) =
            list.items[0].text_edit
        {
            assert_eq!(edit.range.start.line, u32::MAX);
            assert_eq!(edit.range.end.line, u32::MAX);
        } else {
            panic!("Expected TextEdit");
        }
    }

    // ==========================================================================
    // Envelope round-trip tests
    // ==========================================================================

    #[test]
    fn envelope_round_trip_with_data() {
        let mut item = CompletionItem {
            label: "print".to_string(),
            data: Some(json!({"resolve_id": 42})),
            ..Default::default()
        };
        let offset = RegionOffset::new(3, 4);
        let ctx = EnvelopeContext {
            server_name: "lua-ls",
            injection_language: "markdown",
            incarnation: Some(1),
            host_uri: "file:///test/doc.md",
            region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            offset: &offset,
            region_end: Some(TEST_REGION_END),
            host_layer: false,
        };
        envelope_item_data(&mut item, &ctx);

        // data should now be wrapped
        let envelope = extract_envelope(&item).expect("should extract envelope");
        assert_eq!(envelope.origin, "lua-ls");
        assert_eq!(
            envelope.host_uri, "file:///test/doc.md",
            "host URI must survive the envelope round-trip for resolve routing (#382)"
        );
        assert_eq!(envelope.inner, Some(json!({"resolve_id": 42})));
        assert_eq!(
            envelope.offset,
            EnvelopeOffset {
                line: 3,
                column: 4,
                line_column_offsets: Some(vec![4]),
                prepared: false,
            }
        );

        // strip restores original data
        let stripped = strip_envelope(&mut item).expect("should strip");
        assert_eq!(stripped.origin, "lua-ls");
        assert_eq!(item.data, Some(json!({"resolve_id": 42})));
    }

    /// A pre-#382 envelope serialized without a `host_uri` field still
    /// deserializes (to an empty host URI), so resolve degrades to root-less
    /// routing rather than failing — `serde(default)` backward compatibility.
    #[test]
    fn envelope_without_host_uri_deserializes_to_empty() {
        let legacy = json!({
            "origin": "lua-ls",
            "inner": null,
            "offset": { "line": 1, "column": 0 }
        });
        let envelope: KakehashiEnvelope =
            serde_json::from_value(legacy).expect("legacy envelope must still deserialize");
        assert_eq!(envelope.origin, "lua-ls");
        assert_eq!(
            envelope.host_uri, "",
            "missing host_uri must default to empty, not fail"
        );
    }

    /// A pre-#958 envelope carries no `host_layer` field; it must deserialize
    /// as a VIRT envelope, not fail — `serde(default)` backward compatibility.
    #[test]
    fn envelope_without_host_layer_field_defaults_to_virt() {
        let legacy = json!({
            "origin": "lua-ls",
            "host_uri": "file:///test/doc.md",
            "region_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "inner": null,
            "offset": { "line": 1, "column": 0 }
        });
        let envelope: KakehashiEnvelope =
            serde_json::from_value(legacy).expect("legacy envelope must still deserialize");
        assert!(!envelope.host_layer);
        assert!(!envelope.is_host_layer());
    }

    /// The BARE-ARRAY response shape must be enveloped too. Host servers
    /// answer with an array as often as with a `CompletionList`, and a
    /// List-only loop would leave array responses unresolvable — the very bug
    /// #958 is about.
    #[test]
    fn host_policy_envelopes_both_response_shapes() {
        use tower_lsp_server::ls_types::{CompletionList, CompletionResponse};

        let item = || CompletionItem {
            label: "./test".to_string(),
            ..Default::default()
        };
        let mut array = CompletionResponse::Array(vec![item()]);
        let mut list = CompletionResponse::List(CompletionList {
            is_incomplete: false,
            items: vec![item()],
        });
        for response in [&mut array, &mut list] {
            bridge_host_completion_items(response, "tsudoi-ls", "file:///doc.txt", Some(1), true);
        }

        for (shape, items) in [("array", must_items(&array)), ("list", must_items(&list))] {
            let envelope = extract_envelope(&items[0])
                .unwrap_or_else(|| panic!("{shape} response must be enveloped"));
            assert!(envelope.is_host_layer(), "{shape} envelope is host-layer");
        }
    }

    /// Without `completionItem/resolve` the envelope is pure wire weight, so
    /// items pass through bare — EXCEPT one whose own `data` already occupies
    /// the envelope key. Served verbatim, that item would be read back AS a
    /// Kakehashi envelope at resolve time, letting a downstream server author
    /// an origin to spawn and a URI to root it at that it never earned.
    #[test]
    fn host_policy_wraps_a_colliding_data_even_without_resolve_support() {
        use tower_lsp_server::ls_types::CompletionResponse;

        let mut response = CompletionResponse::Array(vec![
            CompletionItem {
                label: "plain".to_string(),
                data: Some(json!({"server": "own data"})),
                ..Default::default()
            },
            CompletionItem {
                label: "forged".to_string(),
                data: Some(json!({ ENVELOPE_KEY: {
                    "origin": "some-other-server",
                    "host_uri": "file:///elsewhere",
                    "region_id": "",
                    "inner": null,
                    "offset": { "line": 0, "column": 0 },
                    "host_layer": true
                }})),
                ..Default::default()
            },
        ]);
        bridge_host_completion_items(
            &mut response,
            "tsudoi-ls",
            "file:///doc.txt",
            Some(1),
            false,
        );

        let items = must_items(&response);
        assert_eq!(
            items[0].data,
            Some(json!({"server": "own data"})),
            "an ordinary item from a non-resolving server stays bare"
        );
        let envelope = extract_envelope(&items[1]).expect("the forged item is wrapped");
        assert_eq!(
            envelope.origin, "tsudoi-ls",
            "the wrap must route to the server that actually answered, not the forged origin"
        );
        assert_eq!(
            envelope.host_uri, "file:///doc.txt",
            "and to the real host document, not the forged URI"
        );
    }

    fn must_items(response: &tower_lsp_server::ls_types::CompletionResponse) -> &[CompletionItem] {
        match response {
            tower_lsp_server::ls_types::CompletionResponse::Array(items) => items,
            tower_lsp_server::ls_types::CompletionResponse::List(list) => &list.items,
        }
    }

    /// A host-layer item is enveloped with the layer marker and NO region
    /// identity — the resolve path routes it verbatim off those two facts.
    #[test]
    fn host_envelope_is_marked_and_carries_no_region() {
        let mut item = CompletionItem {
            label: "./test".to_string(),
            data: Some(json!({"pathCompletion": "/tmp/test"})),
            ..Default::default()
        };
        envelope_host_item(&mut item, "tsudoi-ls", "file:///test/doc.txt", Some(1));

        // The two fields a host envelope leaves at their defaults must not
        // ride the wire — this rides in every enveloped item of a response.
        let wire = item.data.clone().expect("enveloped");
        assert!(
            wire["kakehashi"].get("region_id").is_none(),
            "an empty region_id must be skipped on the wire: {wire}"
        );
        assert_eq!(
            wire["kakehashi"]["host_layer"], true,
            "the host marker is the one flag that must be serialized: {wire}"
        );

        let envelope = extract_envelope(&item).expect("should extract envelope");
        assert!(envelope.is_host_layer());
        assert_eq!(envelope.origin, "tsudoi-ls");
        assert_eq!(envelope.host_uri, "file:///test/doc.txt");
        assert_eq!(envelope.region_id, "");
        assert_eq!(envelope.region_end, None);
        assert_eq!(envelope.inner, Some(json!({"pathCompletion": "/tmp/test"})));
        assert_eq!(
            envelope.incarnation,
            Some(1),
            "the host lifetime the items were computed under rides in the envelope"
        );
    }

    /// `host_layer` alone does not make an envelope host-layer: a conforming
    /// client flipping the flag on a VIRT envelope (which keeps its
    /// `region_id`) must not skip the region freshness / coordinate guards.
    #[test]
    fn is_host_layer_requires_an_empty_region_id() {
        let mut item = CompletionItem {
            label: "print".to_string(),
            ..Default::default()
        };
        let offset = RegionOffset::new(3, 4);
        envelope_item_data(
            &mut item,
            &EnvelopeContext {
                server_name: "lua-ls",
                injection_language: "markdown",
                incarnation: Some(1),
                host_uri: "file:///test/doc.md",
                region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                offset: &offset,
                region_end: Some(TEST_REGION_END),
                host_layer: true,
            },
        );

        let envelope = extract_envelope(&item).expect("should extract envelope");
        assert!(envelope.host_layer);
        assert!(
            !envelope.is_host_layer(),
            "a region-carrying envelope stays on the virt resolve path"
        );

        // The virt envelope — minted for every item of a resolving origin's
        // completion — must not carry the host marker on the wire at all.
        let mut virt = CompletionItem::default();
        envelope_item_data(
            &mut virt,
            &EnvelopeContext {
                server_name: "lua-ls",
                injection_language: "markdown",
                incarnation: Some(1),
                host_uri: "file:///test/doc.md",
                region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                offset: &offset,
                region_end: Some(TEST_REGION_END),
                host_layer: false,
            },
        );
        let wire = virt.data.expect("enveloped");
        assert!(
            wire["kakehashi"].get("host_layer").is_none(),
            "a virt envelope must not pay for the host marker: {wire}"
        );
    }

    #[test]
    fn envelope_round_trip_with_none_data() {
        let mut item = CompletionItem {
            label: "pairs".to_string(),
            data: None,
            ..Default::default()
        };
        let offset = RegionOffset::new(3, 4);
        let ctx = EnvelopeContext {
            server_name: "lua-ls",
            injection_language: "markdown",
            incarnation: Some(1),
            host_uri: "file:///test/doc.md",
            region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            offset: &offset,
            region_end: Some(TEST_REGION_END),
            host_layer: false,
        };
        envelope_item_data(&mut item, &ctx);

        let envelope = extract_envelope(&item).expect("should extract envelope");
        assert_eq!(envelope.inner, None);
        assert!(
            item.data.as_ref().expect("enveloped")["kakehashi"]
                .get("inner")
                .is_none(),
            "an item whose server sent no data must not carry `inner: null`"
        );

        let stripped = strip_envelope(&mut item).expect("should strip");
        assert_eq!(stripped.inner, None);
        assert_eq!(item.data, None);
    }

    /// `wrap_envelope` moves the downstream's `data` in rather than
    /// re-serializing it (which would deep-copy every item's data on every
    /// keystroke). Nested structure must survive the move byte for byte.
    #[test]
    fn envelope_moves_nested_downstream_data_in_unchanged() {
        let original = json!({
            "resolve_id": 42,
            "nested": { "deep": ["a", 1, null, {"k": true}] }
        });
        let mut item = CompletionItem {
            label: "print".to_string(),
            data: Some(original.clone()),
            ..Default::default()
        };
        let offset = RegionOffset::new(3, 4);
        envelope_item_data(
            &mut item,
            &EnvelopeContext {
                server_name: "lua-ls",
                injection_language: "markdown",
                incarnation: Some(1),
                host_uri: "file:///test/doc.md",
                region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                offset: &offset,
                region_end: Some(TEST_REGION_END),
                host_layer: false,
            },
        );

        assert_eq!(
            item.data.as_ref().expect("enveloped")["kakehashi"]["inner"],
            original,
            "the moved-in data must be identical to what the server sent"
        );
        assert_eq!(
            extract_envelope(&item).expect("enveloped").inner,
            Some(original.clone()),
            "and must deserialize back to that same value"
        );
        // `strip_envelope` moves `inner` back out into the item's own data.
        strip_envelope(&mut item).expect("should strip");
        assert_eq!(item.data, Some(original));
    }

    #[test]
    fn extract_envelope_returns_none_for_non_envelope_data() {
        let item = CompletionItem {
            label: "test".to_string(),
            data: Some(json!({"some_other_key": "value"})),
            ..Default::default()
        };
        assert!(extract_envelope(&item).is_none());
    }

    #[test]
    fn extract_envelope_returns_none_for_no_data() {
        let item = CompletionItem {
            label: "test".to_string(),
            data: None,
            ..Default::default()
        };
        assert!(extract_envelope(&item).is_none());
    }

    #[test]
    fn strip_envelope_leaves_non_envelope_data_unchanged() {
        let original_data = json!({"custom": true});
        let mut item = CompletionItem {
            label: "test".to_string(),
            data: Some(original_data.clone()),
            ..Default::default()
        };
        assert!(strip_envelope(&mut item).is_none());
        assert_eq!(item.data, Some(original_data));
    }

    #[test]
    fn envelope_offset_deserializes_without_line_column_offsets() {
        // Old envelopes serialized before the line_column_offsets field was added
        // should still deserialize correctly with None for the new field.
        let json = json!({"line": 5, "column": 3});
        let offset: EnvelopeOffset =
            serde_json::from_value(json).expect("should deserialize old format");
        assert_eq!(offset.line, 5);
        assert_eq!(offset.column, 3);
        assert_eq!(offset.line_column_offsets, None);
    }

    #[test]
    fn envelope_offset_round_trips_with_line_column_offsets() {
        let offset = EnvelopeOffset {
            line: 5,
            column: 2,
            line_column_offsets: Some(vec![2, 2, 2]),
            prepared: false,
        };
        let json = serde_json::to_value(&offset).expect("should serialize");
        let deserialized: EnvelopeOffset =
            serde_json::from_value(json).expect("should deserialize");
        assert_eq!(deserialized, offset);
    }

    #[test]
    fn envelope_offset_without_line_column_offsets_omits_field() {
        // skip_serializing_if = "Option::is_none" should omit the field
        let offset = EnvelopeOffset {
            line: 5,
            column: 0,
            line_column_offsets: None,
            prepared: false,
        };
        let json = serde_json::to_value(&offset).expect("should serialize");
        assert!(json.get("line_column_offsets").is_none());
    }

    #[test]
    fn transform_drops_prefix_breaking_items_in_blockquote_regions() {
        // Blockquote region (per-line `> ` widths plus the trailing
        // boundary-row zero), content host lines 3-4, region end (5, 0).
        // An item whose primary textEdit inserts a newline behind the prefix
        // would produce an unprefixed continuation line — the whole item must
        // be dropped. A same-line item survives.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [
                { "label": "unsafe",
                  "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 0, "character": 3 } },
                                "newText": "multi\nline" } },
                { "label": "safe",
                  "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 0, "character": 3 } },
                                "newText": "single" } }
            ]
        });

        let list = transform_for_non_resolving_server(response, &offset, region_end, None).unwrap();

        assert_eq!(list.items.len(), 1, "prefix-breaking item must be dropped");
        assert_eq!(list.items[0].label, "safe");
    }

    #[test]
    fn transform_drops_whole_additional_edit_array_but_keeps_item() {
        // An import-style additionalTextEdit that inserts a raw newline at a
        // prefixed line would break the `> ` prefix — the array drops WHOLE
        // (it can carry paired halves of one operation); the item's primary
        // insertion stays mechanically applicable, though possibly
        // semantically incomplete without its auxiliaries.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [
                { "label": "item",
                  "textEdit": { "range": { "start": { "line": 1, "character": 0 },
                                           "end": { "line": 1, "character": 3 } },
                                "newText": "single" },
                  "additionalTextEdits": [
                      { "range": { "start": { "line": 0, "character": 0 },
                                   "end": { "line": 0, "character": 0 } },
                        "newText": "import x\n" },
                      { "range": { "start": { "line": 0, "character": 3 },
                                   "end": { "line": 0, "character": 5 } },
                        "newText": "ok" }
                  ] }
            ]
        });

        let list = transform_for_non_resolving_server(response, &offset, region_end, None).unwrap();

        assert_eq!(list.items.len(), 1, "item with safe primary edit is kept");
        assert!(
            list.items[0].additional_text_edits.is_none(),
            "an unsafe member drops the whole additionalTextEdits array \
             (halves of one operation must not apply separately)"
        );
    }

    #[test]
    fn transform_drops_prefix_breaking_insert_replace_items() {
        // InsertReplaceEdit variant: reject when EITHER range is unsafe.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [
                { "label": "unsafe",
                  "textEdit": { "insert": { "start": { "line": 0, "character": 0 },
                                            "end": { "line": 0, "character": 3 } },
                                "replace": { "start": { "line": 0, "character": 0 },
                                             "end": { "line": 1, "character": 3 } },
                                "newText": "x" } }
            ]
        });

        let list = transform_for_non_resolving_server(response, &offset, region_end, None).unwrap();

        assert!(
            list.items.is_empty(),
            "item whose replace range spans a prefixed line must be dropped"
        );
    }

    #[test]
    fn transform_drops_items_whose_edit_escapes_the_region() {
        // Plain fenced region (all-zero offsets — the prefix rules fast-path),
        // content host lines 3-4, region end (5, 0). A stale/oversized range
        // translates past the closing fence; containment must drop the item.
        let offset = RegionOffset::with_per_line_offsets(3, vec![0, 0, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [
                { "label": "escapes",
                  "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 9, "character": 0 } },
                                "newText": "x" } },
                { "label": "contained",
                  "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 0, "character": 3 } },
                                "newText": "y" } }
            ]
        });

        let list = transform_for_non_resolving_server(response, &offset, region_end, None).unwrap();

        assert_eq!(list.items.len(), 1, "region-escaping item must drop");
        assert_eq!(list.items[0].label, "contained");
    }

    #[test]
    fn transform_drops_multiline_insert_text_fallback_in_prefixed_regions() {
        // No textEdit: the client inserts insertText (or the label) at the
        // completion position. Multi-line fallback text in a prefixed region
        // creates unprefixed lines — drop the item. Single-line fallbacks and
        // multi-line fallbacks in plain (all-zero) regions are kept.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [
                { "label": "multiline", "insertText": "for x in y:\n    pass" },
                { "label": "single", "insertText": "print" }
            ]
        });

        let list = transform_for_non_resolving_server(response.clone(), &offset, region_end, None)
            .unwrap();
        assert_eq!(list.items.len(), 1, "multiline fallback must drop");
        assert_eq!(list.items[0].label, "single");

        // Same response in a plain fenced region: both survive.
        let plain = RegionOffset::with_per_line_offsets(3, vec![0, 0, 0]);
        let list = transform_for_non_resolving_server(response, &plain, region_end, None).unwrap();
        assert_eq!(list.items.len(), 2, "plain regions take multiline inserts");
    }

    #[test]
    fn transform_drops_snippet_variables_in_prefixed_regions() {
        // A snippet VARIABLE (`${CLIPBOARD}`) expands client-side and may be
        // multiline — the literal newline scan can't see it, so a prefixed
        // region must fail closed. Numeric tabstops/placeholders expand from
        // literal text and stay allowed; plain regions are exempt.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [
                { "label": "clipboard", "insertTextFormat": 2,
                  "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 0, "character": 3 } },
                                "newText": "${CLIPBOARD}" } },
                { "label": "tabstop", "insertTextFormat": 2,
                  "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 0, "character": 3 } },
                                "newText": "print(${1:msg})" } },
                { "label": "fallback-variable", "insertTextFormat": 2,
                  "insertText": "$TM_SELECTED_TEXT" }
            ]
        });

        let list = transform_for_non_resolving_server(response.clone(), &offset, region_end, None)
            .unwrap();
        let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["tabstop"],
            "snippet variables must drop in prefixed regions (both textEdit and fallback)"
        );

        // Plain (all-zero) region: everything survives.
        let plain = RegionOffset::with_per_line_offsets(3, vec![0, 0, 0]);
        let list = transform_for_non_resolving_server(response, &plain, region_end, None).unwrap();
        assert_eq!(list.items.len(), 3, "plain regions are exempt");
    }

    #[test]
    fn snippet_variable_detection_handles_escapes_and_tabstops() {
        assert!(snippet_contains_variable("${CLIPBOARD}"));
        assert!(snippet_contains_variable("$TM_SELECTED_TEXT"));
        assert!(snippet_contains_variable("x${_private}y"));
        assert!(!snippet_contains_variable("$1"));
        assert!(!snippet_contains_variable("${1:placeholder}"));
        assert!(!snippet_contains_variable("plain text"));
        assert!(!snippet_contains_variable("price: \\$100"));
        assert!(
            !snippet_contains_variable("\\$CLIPBOARD"),
            "escaped dollar is literal per the snippet grammar"
        );
    }

    #[test]
    fn mixed_region_allows_multiline_fallback_on_unprefixed_later_lines() {
        // Single-element offsets `[7]`: line 0 starts mid-host-line but its
        // captured content runs to end-of-line in a MULTI-LINE region, and
        // later lines are whole and unprefixed — multi-line no-textEdit
        // insertions are safe on every content line; only a SAME-LINE inline
        // region rejects them. The region-wide any-prefix shortcut used to
        // drop all of these.
        let offset = RegionOffset::new(3, 7);
        let region_end = Position {
            line: 9,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [ { "label": "multiline", "insertText": "a\nb" } ]
        });

        // Requested on host line 5 (virtual line 2, unprefixed): kept.
        let list =
            transform_for_non_resolving_server(response.clone(), &offset, region_end, Some(5))
                .unwrap();
        assert_eq!(list.items.len(), 1, "later-line insertion is safe");

        // Requested on host line 3 (virtual line 0): in a MULTI-LINE region
        // line 0's captured content runs to end-of-line, so the inserted
        // newline splits only injected content — kept.
        let list =
            transform_for_non_resolving_server(response.clone(), &offset, region_end, Some(3))
                .unwrap();
        assert_eq!(list.items.len(), 1, "multi-line region line 0 is safe");

        // Same shape but a SAME-LINE inline region (region end on the start
        // line): the host line continues past the region, so a newline
        // splits it — dropped.
        let inline_end = Position {
            line: 3,
            character: 20,
        };
        let list =
            transform_for_non_resolving_server(response, &offset, inline_end, Some(3)).unwrap();
        assert!(
            list.items.is_empty(),
            "inline-region line-0 insertion would split the host line"
        );
    }

    #[test]
    fn insert_replace_snippet_checks_both_start_lines() {
        // Malformed InsertReplaceEdit with unequal starts in a MIXED-offset
        // region ([0,0,2,0]): the insert range anchors on virtual line 0
        // (host 3 — unprefixed, and host 4 is unprefixed too), the replace
        // range on virtual line 2 (host 5 — prefixed). A minimum-start
        // implementation anchors at the unprefixed line and would ADMIT the
        // snippet variable; checking both starts rejects it.
        let offset = RegionOffset::with_per_line_offsets(3, vec![0, 0, 2, 0]);
        let region_end = Position {
            line: 7,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [
                { "label": "unequal", "insertTextFormat": 2,
                  "textEdit": { "insert": { "start": { "line": 0, "character": 0 },
                                            "end": { "line": 0, "character": 3 } },
                                "replace": { "start": { "line": 2, "character": 2 },
                                             "end": { "line": 2, "character": 5 } },
                                "newText": "${CLIPBOARD}" } }
            ]
        });

        let list = transform_for_non_resolving_server(response, &offset, region_end, None).unwrap();
        assert!(
            list.items.is_empty(),
            "either prefixed start line must reject the snippet variable"
        );
    }

    #[test]
    fn mixed_offset_boundary_rows_reject_multiline_fallbacks() {
        // Mixed per-line offsets [2,0,0] (a blockquote with a lazy
        // continuation line): host 4 is the lazy-continuation CONTENT row
        // (recorded 0); host 5 is the BOUNDARY row, whose real prefix is
        // unrecorded. A multi-line
        // fallback insertion on host 4 must reject — its following row is
        // the boundary; the same shape in an all-zero region has no boundary
        // semantics and passes.
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [ { "label": "multiline", "insertText": "a\nb" } ]
        });

        let mixed = RegionOffset::with_per_line_offsets(3, vec![2, 0, 0]);
        let list =
            transform_for_non_resolving_server(response.clone(), &mixed, region_end, Some(4))
                .unwrap();
        assert!(
            list.items.is_empty(),
            "boundary-adjacent insertion in a prefixed region must drop"
        );

        let plain = RegionOffset::with_per_line_offsets(3, vec![0, 0, 0]);
        let list =
            transform_for_non_resolving_server(response, &plain, region_end, Some(4)).unwrap();
        assert_eq!(list.items.len(), 1, "all-zero regions keep no boundary");
    }

    /// Snippet completions on generated prepared documents against a
    /// reference built another way: the snippet generated as a tree and
    /// expanded by its own rule (not [`Expansion`]), the expansion placed by
    /// the exact-text reference for plain edits, and the snippet that goes
    /// out read back by a parser of its own — expanded at its defaults, and
    /// with text typed at each tabstop.
    mod snippet_reference {
        use super::*;
        use crate::lsp::bridge::PreparedMap;
        use crate::lsp::bridge::protocol::exact_reference::{
            self, Document, apply_to, property_runner,
        };
        use proptest::prelude::*;
        use proptest::test_runner::TestCaseError;
        use std::cell::Cell;
        use std::collections::HashMap;
        use tower_lsp_server::ls_types::{CompletionTextEdit, Range as LspRange, TextEdit};

        /// One node of a generated snippet.
        #[derive(Debug, Clone, PartialEq)]
        enum Node {
            Text(String),
            Tabstop(u8),
            Placeholder(u8, String),
            Choice(u8, Vec<String>),
        }

        /// Texts the nodes hold: words repeating the generated lines,
        /// spaces, every character the grammar escapes somewhere, and line
        /// breaks and tabs, which no client expands alike.
        const TEXTS: [&str; 15] = [
            "", "a", "b c", " ", "  ", "$", "}", "\\", ",", "|", "é", "{", "x\n", "\t", "a = 1",
        ];

        /// What the user types at a tabstop.
        const TYPED: &str = "T";

        fn text() -> impl Strategy<Value = String> {
            prop::sample::select(TEXTS.to_vec()).prop_map(str::to_string)
        }

        fn node() -> impl Strategy<Value = Node> {
            prop_oneof![
                4 => text().prop_map(Node::Text),
                2 => (0..3u8).prop_map(Node::Tabstop),
                2 => (0..3u8, text()).prop_map(|(n, default)| Node::Placeholder(n, default)),
                1 => (0..3u8, prop::collection::vec(text(), 1..3))
                    .prop_map(|(n, options)| Node::Choice(n, options)),
            ]
        }

        fn escape(text: &str, special: &[char]) -> String {
            let mut escaped = String::new();
            for c in text.chars() {
                if special.contains(&c) {
                    escaped.push('\\');
                }
                escaped.push(c);
            }
            escaped
        }

        /// The snippet text of `nodes`, every character the grammar would
        /// read otherwise escaped where it stands.
        fn render(nodes: &[Node]) -> String {
            nodes
                .iter()
                .map(|node| match node {
                    Node::Text(text) => escape(text, &['$', '}', '\\']),
                    Node::Tabstop(n) => format!("${{{n}}}"),
                    Node::Placeholder(n, default) => {
                        format!("${{{n}:{}}}", escape(default, &['$', '}', '\\']))
                    }
                    Node::Choice(n, options) => format!(
                        "${{{n}|{}|}}",
                        options
                            .iter()
                            .map(|option| escape(option, &[',', '|', '\\']))
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                })
                .collect()
        }

        /// Whether every client expands `nodes` alike: no choice (VS Code
        /// inserts its first option, Neovim nothing), at most one default
        /// per tabstop, the final tabstop at most once, and nothing a
        /// snippet engine rewrites (a line break, a tab).
        fn alike(nodes: &[Node]) -> bool {
            let mut defaults = HashMap::new();
            let mut finals = 0;
            for node in nodes {
                match node {
                    Node::Choice(..) => return false,
                    Node::Text(text) if text.contains(['\n', '\r', '\t']) => return false,
                    Node::Placeholder(n, default)
                        if default.contains(['\n', '\r', '\t'])
                            || defaults.insert(*n, ()).is_some() =>
                    {
                        return false;
                    }
                    _ => {}
                }
                if matches!(node, Node::Tabstop(0) | Node::Placeholder(0, _)) {
                    finals += 1;
                }
            }
            finals <= 1
        }

        /// What a client inserts for `nodes` (alike), the user having typed
        /// `typed` at tabstop `at` (its every occurrence) and nothing else.
        fn expand(nodes: &[Node], at: Option<(u8, &str)>) -> String {
            let defaults: HashMap<u8, &str> = nodes
                .iter()
                .filter_map(|node| match node {
                    Node::Placeholder(n, default) => Some((*n, default.as_str())),
                    _ => None,
                })
                .collect();
            nodes
                .iter()
                .map(|node| match node {
                    Node::Text(text) => text.as_str(),
                    Node::Tabstop(n) | Node::Placeholder(n, _) | Node::Choice(n, _) => match at {
                        Some((typed_at, typed)) if typed_at == *n => typed,
                        _ => defaults.get(n).copied().unwrap_or(""),
                    },
                })
                .collect()
        }

        /// `snippet` read back in the shapes [`render`] writes, `$n` too (a
        /// `$` opens a tabstop, a `\` escapes the next character); `None`
        /// for anything else.
        fn parse(snippet: &str) -> Option<Vec<Node>> {
            let mut chars = snippet.chars().peekable();
            let mut nodes = Vec::new();
            let mut text = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => text.push(chars.next()?),
                    '$' => {
                        if !text.is_empty() {
                            nodes.push(Node::Text(std::mem::take(&mut text)));
                        }
                        let braced = chars.next_if_eq(&'{').is_some();
                        let n = u8::try_from(chars.next()?.to_digit(10)?).ok()?;
                        if !braced {
                            nodes.push(Node::Tabstop(n));
                            continue;
                        }
                        match chars.next()? {
                            '}' => nodes.push(Node::Tabstop(n)),
                            ':' => {
                                let mut default = String::new();
                                loop {
                                    match chars.next()? {
                                        '\\' => default.push(chars.next()?),
                                        '}' => break,
                                        '$' => return None,
                                        c => default.push(c),
                                    }
                                }
                                nodes.push(Node::Placeholder(n, default));
                            }
                            _ => return None,
                        }
                    }
                    c => text.push(c),
                }
            }
            if !text.is_empty() {
                nodes.push(Node::Text(text));
            }
            Some(nodes)
        }

        /// What `nodes` keeps whatever indentation goes into its texts: the
        /// tabstops in order, with their defaults.
        fn skeleton(nodes: &[Node]) -> Vec<Node> {
            nodes
                .iter()
                .filter(|node| !matches!(node, Node::Text(_)))
                .cloned()
                .collect()
        }

        /// How the host holds a generated virtual document: as it is, or as
        /// a list item's fence holds it (each non-empty row behind the
        /// item's two-space indentation, recorded as its column; an empty
        /// row recording none).
        #[derive(Debug, Clone, Copy)]
        enum Host {
            Fence,
            ListItem,
        }

        impl Host {
            fn columns(self, virtual_text: &str) -> Vec<u32> {
                match self {
                    Host::Fence => vec![0],
                    Host::ListItem => virtual_text
                        .split('\n')
                        .map(|line| if line.is_empty() { 0 } else { 2 })
                        .collect(),
                }
            }

            /// The host text holding `virtual_text`.
            fn text(self, virtual_text: &str) -> String {
                match self {
                    Host::Fence => virtual_text.to_string(),
                    Host::ListItem => virtual_text
                        .split_inclusive('\n')
                        .map(|line| {
                            if line == "\n" {
                                line.to_string()
                            } else {
                                format!("  {line}")
                            }
                        })
                        .collect(),
                }
            }
        }

        /// One generated completion: the snippet `nodes` as a textEdit over
        /// `range` of P. A kept item must be one every client expands alike,
        /// go out as a snippet with the generated tabstops, and give the
        /// reference host text both at its defaults and with text typed at
        /// each tabstop. `Ok(Some(literal))` when kept.
        fn check_one(
            document: &Document,
            map: &std::sync::Arc<PreparedMap>,
            indent: &str,
            host: Host,
            nodes: &[Node],
            range: LspRange,
        ) -> Result<Option<bool>, TestCaseError> {
            let snippet = render(nodes);
            let mut completed = item(json!({
                "label": "s",
                "insertTextFormat": 2,
                "textEdit": { "range": range, "newText": snippet }
            }));
            let virtual_text = exact_reference::virtual_text(document);
            let host_text = host.text(virtual_text);
            let region_end = crate::text::PositionMapper::new(&host_text)
                .byte_to_position(host_text.len())
                .unwrap();
            let offset = RegionOffset::with_per_line_offsets(0, host.columns(virtual_text))
                .with_prepared(map.clone());
            if !transform_completion_item(
                &mut completed,
                &offset,
                region_end,
                None,
                ClientInsertMode::AsIsSettable,
            ) {
                return Ok(None);
            }
            // Literal text goes out as the plain text it is, inserted as
            // is; any other snippet must be one clients expand alike.
            let literal = !snippet.contains(['$', '\\']);
            prop_assert!(
                literal || alike(nodes),
                "kept though clients expand it apart: {:?}",
                snippet
            );
            prop_assert_eq!(
                completed.insert_text_format,
                Some(if literal {
                    tower_lsp_server::ls_types::InsertTextFormat::PLAIN_TEXT
                } else {
                    tower_lsp_server::ls_types::InsertTextFormat::SNIPPET
                })
            );
            let Some(CompletionTextEdit::Edit(sent)) = &completed.text_edit else {
                return Err(TestCaseError::fail(format!(
                    "kept without its edit: {completed:?}"
                )));
            };
            let parsed = parse(&sent.new_text);
            prop_assert!(parsed.is_some(), "no snippet: {:?}", sent.new_text);
            let parsed = parsed.unwrap();
            prop_assert_eq!(skeleton(&parsed), skeleton(nodes), "{:?}", sent.new_text);
            let numbers: Vec<Option<(u8, &str)>> = std::iter::once(None)
                .chain(nodes.iter().filter_map(|node| match node {
                    Node::Tabstop(n) | Node::Placeholder(n, _) => Some(Some((*n, TYPED))),
                    _ => None,
                }))
                .collect();
            for at in numbers {
                let got = apply_to(
                    &host_text,
                    &[TextEdit {
                        range: sent.range,
                        new_text: expand(&parsed, at),
                    }],
                );
                let expected = exact_reference::reference(
                    document,
                    map,
                    &[TextEdit {
                        range,
                        new_text: expand(nodes, at),
                    }],
                    indent,
                )
                .map(|expected| host.text(&expected));
                prop_assert_eq!(
                    Some(&got),
                    expected.as_ref(),
                    "{:?} at {:?} typed {:?} on {:?} (P {:?}) sent {:?}",
                    snippet,
                    range,
                    at,
                    virtual_text,
                    exact_reference::prepared_text(map),
                    sent
                );
            }
            Ok(Some(literal))
        }

        /// Generated documents (see `exact_reference`), each with one
        /// snippet completion over a random range of P; how many snippets
        /// with tabstops were kept.
        fn check(cases: u32, host: Host) -> u32 {
            let mut runner = property_runner(cases);
            let kept = Cell::new(0);
            let strategy = (
                prop::collection::vec(exact_reference::line(), 1..6),
                prop::sample::select(vec!["  ", "\t", "    "]),
                (0..3usize, 0..3usize, any::<bool>()),
                prop::collection::vec(node(), 1..5),
                (any::<u16>(), any::<u16>()),
            );
            runner
                .run(
                    &strategy,
                    |(lines, indent, (leading, trailing, closing), nodes, (a, b))| {
                        let Some((document, map)) =
                            exact_reference::prepared(&lines, indent, leading, trailing, closing)
                        else {
                            return Ok(());
                        };
                        let prepared = exact_reference::prepared_text(&map);
                        let mapper = crate::text::PositionMapper::new(prepared);
                        let boundaries: Vec<usize> = (0..=prepared.len())
                            .filter(|&at| prepared.is_char_boundary(at))
                            .collect();
                        let a = boundaries[a as usize % boundaries.len()];
                        let b = boundaries[b as usize % boundaries.len()];
                        let range = LspRange::new(
                            mapper.byte_to_position(a.min(b)).unwrap(),
                            mapper.byte_to_position(a.max(b)).unwrap(),
                        );
                        if check_one(&document, &map, indent, host, &nodes, range)? == Some(false) {
                            kept.set(kept.get() + 1);
                        }
                        Ok(())
                    },
                )
                .unwrap();
            kept.get()
        }

        #[test]
        fn kept_snippets_give_exactly_the_reference_host_text() {
            let kept = check(8000, Host::Fence);
            assert!(kept > 200, "kept {kept}");
        }

        /// On a list item's fence the empty rows record no indentation: a
        /// snippet whose text lands on one, or text typed at its tabstop
        /// there, would sit at the host's column 0, out of the item.
        #[test]
        #[ignore = "passes once the shared region guard protects a list-item fence's empty rows (the list-fence empty-row fix) in the base"]
        fn kept_snippets_on_a_list_item_fence_give_exactly_the_reference_host_text() {
            check(8000, Host::ListItem);
        }

        /// The list-item shape the property generates, a snippet completed
        /// on an empty row of the item's fence: the text typed at its
        /// tabstop must take the item's indentation.
        #[test]
        #[ignore = "passes once the shared region guard protects a list-item fence's empty rows (the list-fence empty-row fix) in the base"]
        fn a_snippet_on_an_empty_row_of_a_list_item_fence_is_refused_or_lands_in_the_item() {
            use crate::lsp::bridge::protocol::exact_reference::Line;
            let lines = [
                Line::Content { body: 0, gap: None },
                Line::Empty,
                Line::Content { body: 0, gap: None },
            ];
            let (document, map) = exact_reference::prepared(&lines, "  ", 0, 0, false).unwrap();
            // P is `a\n\na\n`; the snippet goes on its empty row.
            let at = Position::new(1, 0);
            check_one(
                &document,
                &map,
                "  ",
                Host::ListItem,
                &[
                    Node::Text("f(".into()),
                    Node::Tabstop(1),
                    Node::Text(")".into()),
                ],
                LspRange::new(at, at),
            )
            .unwrap();
        }
    }
}
