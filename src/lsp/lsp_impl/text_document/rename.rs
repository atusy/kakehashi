//! Rename method for Kakehashi.
//!
//! Walks the resolved layer order (cross-layer-aggregation): the virt layer
//! bridges the injection region under the cursor, the host layer
//! (host-document-bridge) bridges the host document itself with the real URI
//! and the response as-is except that bridge-local
//! `TextDocumentEdit.version`s are nulled. The first layer producing a
//! non-empty result wins (`preferred`).

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{NumberOrString, Position, RenameParams, Uri, WorkspaceEdit};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::dispatch_preferred;
use crate::lsp::bridge::strip_bridge_local_versions;
use crate::lsp::bridge::workspace_edit_has_effect;
use crate::lsp::lsp_impl::bridge_context::{Sourced, parse_host_verbatim};

const METHOD: &str = "textDocument/rename";

impl Kakehashi {
    pub(crate) async fn rename_impl(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let work_done_token = params.work_done_progress_params.work_done_token;
        let lsp_uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let new_name = params.new_name;

        let virt = self.rename_virt_layer(&lsp_uri, position, &new_name, work_done_token);
        let native = async {
            let edit = self
                .native_bindings_answer(&lsp_uri, position, |ctx| {
                    super::native_bindings::native_rename(ctx, &lsp_uri, &new_name)
                })
                .await?;
            Ok(edit.map(Sourced::unprepared))
        };
        let answer = self
            .walk_layers_with_native(
                &lsp_uri,
                METHOD,
                METHOD,
                raw_params,
                virt,
                native,
                // Host-layer edits are verbatim except versions: a downstream's
                // document versions are bridge-local and would read as stale to
                // version-checking editors.
                |value| {
                    let mut edit = parse_host_verbatim::<WorkspaceEdit>(value)?;
                    strip_bridge_local_versions(&mut edit);
                    Some(Sourced::unprepared(edit))
                },
                |sourced: &Sourced<WorkspaceEdit>| rename_workspace_edit_has_result(&sourced.value),
            )
            .await?;
        // The final answer chosen: its edits checked once more against the
        // host text, which may have moved while it was held. A rename comes
        // once per action, like formatting, so the check waits for the
        // host's last edit to settle and its regions' holds to clear (see
        // `formatting_sources_current`); a stale answer is no answer.
        let Some(sourced) = answer else {
            return Ok(None);
        };
        let verdict = self
            .settled_verdict_of(&lsp_uri, std::iter::once(&sourced))
            .await;
        if !verdict.keeps() {
            log::warn!(
                target: "kakehashi::bridge",
                "rename result for {} dropped: {verdict}",
                lsp_uri.as_str()
            );
            return Ok(None);
        }
        Ok(Some(sourced.value))
    }

    /// Virt layer: bridge the injection region under the cursor.
    async fn rename_virt_layer(
        &self,
        lsp_uri: &Uri,
        position: Position,
        new_name: &str,
        client_progress_token: Option<NumberOrString>,
    ) -> Result<Option<Sourced<WorkspaceEdit>>> {
        // Before the text is read: see `Sourced::from_region`.
        let revision = self.prepared_request_revision(lsp_uri);
        let Some(mut ctx) = self
            .resolve_bridge_contexts(lsp_uri, position, METHOD)
            .await
        else {
            return Ok(None);
        };
        ctx.document.client_progress_token = client_progress_token;

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        // Fan-out rename requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            |t| {
                let new_name = new_name.to_string();
                async move {
                    t.pool
                        .send_rename_request(
                            &t.server_name,
                            &t.server_config,
                            &t.uri,
                            position,
                            t.region_end(),
                            &t.injection_language,
                            &t.region_id,
                            t.offset,
                            &t.virtual_content,
                            &new_name,
                            t.upstream_id,
                            t.client_progress_token,
                        )
                        .await
                }
            },
            rename_option_has_result,
            cancel_rx,
        )
        .await;
        let edit = result.handle(&self.notifier(), "rename", None, Ok).await?;
        Ok(edit.map(|edit| {
            Sourced::from_region(&ctx.document, edit, self.bridge.node_tracker(), revision)
        }))
    }
}

fn rename_workspace_edit_has_result(edit: &WorkspaceEdit) -> bool {
    workspace_edit_has_effect(edit)
}

fn rename_option_has_result(edit: &Option<WorkspaceEdit>) -> bool {
    edit.as_ref().is_some_and(rename_workspace_edit_has_result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workspace_edit(value: serde_json::Value) -> WorkspaceEdit {
        serde_json::from_value(value).expect("valid WorkspaceEdit")
    }

    #[test]
    fn rename_predicates_reject_empty_workspace_edits() {
        let empty = workspace_edit(json!({ "changes": {} }));

        assert!(!rename_workspace_edit_has_result(&empty));
        assert!(!rename_option_has_result(&Some(empty)));
        assert!(!rename_option_has_result(&None));
    }

    #[test]
    fn rename_predicates_accept_workspace_edits_with_effect() {
        let edit = workspace_edit(json!({
            "changes": {
                "file:///test.md": [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 1 }
                    },
                    "newText": "x"
                }]
            }
        }));

        assert!(rename_workspace_edit_has_result(&edit));
        assert!(rename_option_has_result(&Some(edit)));
    }
}
