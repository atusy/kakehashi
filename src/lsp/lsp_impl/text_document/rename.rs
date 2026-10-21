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
        let passed_over = super::super::bridge_context::StaleAnswerFallback::default();
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
                // Read as the race decides: a prepared answer moved or stale
                // is no answer of its layer, and the race falls through to
                // the next layer's (a settling one still wins, then waited on
                // below). Passed over, it is still the answer when no other
                // layer's is, which the check below drops as ContentModified.
                |sourced: &Sourced<WorkspaceEdit>| {
                    passed_over.answers(sourced, rename_workspace_edit_has_result, |sourced| {
                        self.answer_may_keep(&lsp_uri, sourced)
                    })
                },
            )
            .await;
        let answer = passed_over.or_kept(answer)?;
        // The final answer chosen: its edits checked once more against the
        // host text, which may have moved while it was held. A rename comes
        // once per action, like formatting, so the check waits for the
        // host's last edit to settle and its regions' holds to clear (see
        // `formatting_sources_current`). A stale answer is answered as
        // `ContentModified`, not `null` (which reads as nothing to rename):
        // an editor showing the error lets the user rename again (how it
        // is shown is the client's).
        let Some(sourced) = answer else {
            return Ok(None);
        };
        match self
            .settled_answer(&lsp_uri, sourced, "rename", log::Level::Warn)
            .await
        {
            Ok(edit) => Ok(Some(edit)),
            Err(verdict) => {
                let mut error = crate::error::content_modified_error();
                error.message = format!("kakehashi: rename result dropped: {verdict}").into();
                Err(error)
            }
        }
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

    /// Passed over, a prepared rename is still the answer when no other
    /// layer answers (nothing, or an error), for the final check to answer
    /// ContentModified; a cancel stays a cancel.
    #[tokio::test]
    async fn a_rename_passed_over_answers_when_no_other_layer_does() {
        use crate::config::settings::LayerSource::{Host, Native, Virt};
        use crate::lsp::lsp_impl::bridge_context::race_layers_preferred;
        use tower_lsp_server::jsonrpc::{Error, ErrorCode};
        let host_uri = url::Url::parse("file:///test/rename-race.md").unwrap();
        let map = crate::lsp::bridge::apply_prepare_result(
            "a",
            &crate::lsp::bridge::VirtualLayout::single("a"),
            None,
        )
        .unwrap()
        .map;
        let edit = WorkspaceEdit {
            changes: Some(std::collections::HashMap::from([(
                "file:///test/a.lua".parse::<Uri>().unwrap(),
                vec![tower_lsp_server::ls_types::TextEdit::default()],
            )])),
            ..Default::default()
        };
        let walk = |host: Result<Option<Sourced<WorkspaceEdit>>>| {
            let virt = Sourced::prepared_for_test(edit.clone(), &host_uri, "lua", "r", &map, 1, 0);
            async move {
                let passed_over =
                    super::super::super::bridge_context::StaleAnswerFallback::default();
                let result = race_layers_preferred(
                    &[Virt, Host, Native],
                    std::future::ready(Ok(Some(virt))),
                    std::future::ready(host),
                    std::future::ready(Ok(None)),
                    |sourced: &Sourced<WorkspaceEdit>| {
                        passed_over.answers(sourced, rename_workspace_edit_has_result, |_| false)
                    },
                )
                .await;
                passed_over.or_kept(result)
            }
        };
        for host in [Ok(None), Err(Error::internal_error())] {
            let answer = walk(host).await.unwrap().expect("the passed-over answer");
            assert!(answer.is_prepared());
        }
        assert_eq!(
            walk(Err(Error::request_cancelled()))
                .await
                .map(|_| ())
                .unwrap_err()
                .code,
            ErrorCode::RequestCancelled
        );
        let host_answer = walk(Ok(Some(Sourced::unprepared(edit.clone()))))
            .await
            .unwrap()
            .unwrap();
        assert!(!host_answer.is_prepared());
    }

    /// Under a cross-layer `preferred`, a prepared rename the race finds
    /// moved or stale is no answer: the host layer's wins, waited for when
    /// it is still running. One that may be kept wins as ever.
    #[tokio::test]
    async fn a_rename_that_may_not_be_kept_falls_through_to_the_host_layers() {
        let edit = |name: &str| {
            let uri: Uri = format!("file:///test/{name}.lua").parse().unwrap();
            WorkspaceEdit {
                changes: Some(std::collections::HashMap::from([(
                    uri,
                    vec![tower_lsp_server::ls_types::TextEdit {
                        range: Default::default(),
                        new_text: name.to_string(),
                    }],
                )])),
                ..Default::default()
            }
        };
        for (may_keep, host_delay_ms) in [(true, 0), (false, 0), (false, 20)] {
            let won = super::super::super::bridge_context::race_prepared_virt_against_host(
                edit("virt"),
                edit("host"),
                host_delay_ms,
                may_keep,
                rename_workspace_edit_has_result,
            )
            .await;
            assert_eq!(
                won,
                edit(if may_keep { "virt" } else { "host" }),
                "{may_keep} {host_delay_ms}"
            );
        }
    }
}
