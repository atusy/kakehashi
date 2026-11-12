//! Rename method for Kakehashi.
//!
//! Walks the resolved layer order (cross-layer-aggregation): the virt layer
//! bridges the injection region under the cursor, the host layer
//! (host-document-bridge) bridges the host document itself with the real URI
//! and the response as-is except that bridge-local
//! `TextDocumentEdit.version`s are nulled. The first layer producing a
//! non-empty result wins (`preferred`).

use tower_lsp_server::jsonrpc::{Error, ErrorCode, Result};
use tower_lsp_server::ls_types::{NumberOrString, Position, RenameParams, Uri, WorkspaceEdit};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::dispatch_preferred;
use crate::lsp::bridge::WorkspaceEditRefusal;
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

        // Before any layer reads the host's text: the revision the virt
        // layer's answer is made on (see `Sourced::from_region`), and the
        // one another layer's must still be for to stand in for it.
        let race = self.prepared_race(&lsp_uri);
        let virt = self.rename_virt_layer(
            &lsp_uri,
            position,
            &new_name,
            work_done_token,
            race.revision(),
        );
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
                // Read as the race decides: a prepared answer moved or stale
                // is no answer of its layer, and the race falls through to
                // the next layer's.
                |sourced: &Sourced<WorkspaceEdit>| {
                    race.answers(sourced, rename_workspace_edit_has_result)
                },
            )
            .await;
        // The final answer chosen (see `PreparedRace::conclude`): a prepared
        // one checked once more against the host, which may have moved
        // while it was held; another layer's standing in for one passed
        // over only while the host is still the text the request read. A
        // dropped answer is `ContentModified`, not `null` (which reads as
        // nothing to rename): an editor showing the error lets the user
        // rename again (how it is shown is the client's).
        race.conclude(answer, &lsp_uri, "rename", log::Level::Warn)
    }

    /// Virt layer: bridge the injection region under the cursor.
    async fn rename_virt_layer(
        &self,
        lsp_uri: &Uri,
        position: Position,
        new_name: &str,
        client_progress_token: Option<NumberOrString>,
        revision: Option<crate::lsp::bridge::HostRevision>,
    ) -> Result<Option<Sourced<WorkspaceEdit>>> {
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
        // A reply its prepared map refused, the highest-ranked to arrive
        // so: its server counts as not answering, so one ranked below it
        // may, and it is told only when none does (below).
        let refused = std::sync::Arc::new(std::sync::OnceLock::new());
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            |t| {
                let new_name = new_name.to_string();
                let refused = std::sync::Arc::clone(&refused);
                async move {
                    let reply = t
                        .pool
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
                        .await?;
                    Ok(reply.unwrap_or_else(|refusal| {
                        let _ = refused.set(refusal);
                        None
                    }))
                }
            },
            rename_option_has_result,
            cancel_rx,
        )
        .await;
        let edit = result.handle(&self.notifier(), "rename", None, Ok).await?;
        let edit = refused_unless_answered(edit, refused.get().copied())?;
        Ok(edit.map(|edit| Sourced::from_region(&ctx.document, edit, revision)))
    }
}

/// The virt layer's answer once its servers answered: a rename a server
/// made that the prepared map refused, no other server renaming, is an
/// error saying why. `null` would read as nothing to rename, and asking
/// again changes nothing. As its layer's error, it still gives way to
/// another layer's rename.
fn refused_unless_answered(
    edit: Option<WorkspaceEdit>,
    refusal: Option<WorkspaceEditRefusal>,
) -> Result<Option<WorkspaceEdit>> {
    match (edit, refusal) {
        (None, Some(refusal)) => Err(Error {
            code: ErrorCode::ServerError(tower_lsp_server::ls_types::error_codes::REQUEST_FAILED),
            message: format!("kakehashi: rename refused: {refusal}").into(),
            data: None,
        }),
        (edit, _) => Ok(edit),
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

    /// A rename the map refused is told only when no server renamed.
    #[test]
    fn a_refused_rename_is_an_error_only_when_no_server_renamed() {
        let edit = workspace_edit(json!({
            "changes": { "file:///test.md": [{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 1 }
                },
                "newText": "x"
            }] }
        }));
        let refusal = WorkspaceEditRefusal::Unmappable;

        let error = refused_unless_answered(None, Some(refusal)).unwrap_err();
        assert_eq!(error.code, ErrorCode::ServerError(-32803));
        assert!(
            error.message.contains(&refusal.to_string()),
            "{}",
            error.message
        );
        // A server ranked below the one refused renamed: its rename stands.
        assert_eq!(
            refused_unless_answered(Some(edit.clone()), Some(refusal)).unwrap(),
            Some(edit)
        );
        assert_eq!(refused_unless_answered(None, None).unwrap(), None);
    }

    /// The refusal is its layer's error: another layer's rename wins over
    /// it, and it is the answer when no other layer renames.
    #[tokio::test]
    async fn a_refused_rename_gives_way_to_another_layers_rename() {
        use crate::config::settings::LayerSource::{Host, Native, Virt};
        use crate::lsp::lsp_impl::bridge_context::race_layers_preferred;
        let edit = workspace_edit(json!({
            "changes": { "file:///test.md": [{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 1 }
                },
                "newText": "x"
            }] }
        }));
        let walk = |host: Option<WorkspaceEdit>| {
            race_layers_preferred(
                &[Virt, Host, Native],
                std::future::ready(refused_unless_answered(
                    None,
                    Some(WorkspaceEditRefusal::Unmappable),
                )),
                std::future::ready(Ok(host)),
                std::future::ready(Ok(None)),
                rename_workspace_edit_has_result,
            )
        };
        assert_eq!(walk(Some(edit.clone())).await.unwrap(), Some(edit));
        assert_eq!(
            walk(None).await.unwrap_err().code,
            ErrorCode::ServerError(-32803)
        );
    }

    /// Passed over, a prepared rename is why the request is answered
    /// `ContentModified` when no other layer renames (nothing, or an error,
    /// which it would have outranked); a cancel stays a cancel, and another
    /// layer's rename stands in while the host is unchanged.
    #[tokio::test]
    async fn a_rename_passed_over_is_content_modified_when_no_other_layer_renames() {
        use crate::config::settings::LayerSource::{Host, Native, Virt};
        use crate::lsp::bridge::HostRevision;
        use crate::lsp::lsp_impl::bridge_context::{PreparedRace, race_layers_preferred};
        use tower_lsp_server::jsonrpc::{Error, ErrorCode};
        let mut prepared = crate::lsp::bridge::apply_prepare_result(
            "a",
            &crate::lsp::bridge::VirtualLayout::single("a"),
            None,
        )
        .unwrap();
        // Stale: its map revoked, the host unmoved.
        let revocation = std::sync::Arc::new(crate::lsp::bridge::Revocation::default());
        std::sync::Arc::get_mut(&mut prepared.map)
            .unwrap()
            .set_revocation(std::sync::Arc::clone(&revocation));
        revocation.revoke(u64::MAX);
        let map = prepared.map;
        let edit = WorkspaceEdit {
            changes: Some(std::collections::HashMap::from([(
                "file:///test/a.lua".parse::<Uri>().unwrap(),
                vec![tower_lsp_server::ls_types::TextEdit::default()],
            )])),
            ..Default::default()
        };
        let uri: Uri = "file:///test/rename-race.md".parse().unwrap();
        let walk = |host: Result<Option<Sourced<WorkspaceEdit>>>| {
            let virt = Sourced::prepared_for_test(edit.clone(), &map, 1, 0);
            let uri = uri.clone();
            async move {
                let race = PreparedRace::new(|| {
                    Some(HostRevision {
                        incarnation: 1,
                        content_version: 0,
                    })
                });
                let result = race_layers_preferred(
                    &[Virt, Host, Native],
                    std::future::ready(Ok(Some(virt))),
                    std::future::ready(host),
                    std::future::ready(Ok(None)),
                    |sourced: &Sourced<WorkspaceEdit>| {
                        race.answers(sourced, rename_workspace_edit_has_result)
                    },
                )
                .await;
                race.conclude(result, &uri, "rename", log::Level::Debug)
            }
        };
        for host in [Ok(None), Err(Error::internal_error())] {
            let error = walk(host).await.unwrap_err();
            assert_eq!(error.code, ErrorCode::ServerError(-32801));
            assert!(error.message.contains("rename result dropped"), "{error}");
        }
        assert_eq!(
            walk(Err(Error::request_cancelled()))
                .await
                .unwrap_err()
                .code,
            ErrorCode::RequestCancelled
        );
        assert_eq!(
            walk(Ok(Some(Sourced::unprepared(edit.clone()))))
                .await
                .unwrap(),
            Some(edit.clone())
        );
    }

    /// Under a cross-layer `preferred`, a prepared rename the race finds
    /// stale is no answer: the host layer's stands in, waited for when it
    /// is still running. One the host moved under leaves none to stand in:
    /// the host layer's rename was read for the text it moved from too. One
    /// still current wins as ever.
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
        use crate::lsp::lsp_impl::bridge_context::Verdict;
        for (verdict, host_delay_ms) in [
            (Verdict::Current, 0),
            (Verdict::Stale, 0),
            (Verdict::Stale, 20),
            (Verdict::Moved, 0),
            (Verdict::Moved, 20),
        ] {
            let won = super::super::super::bridge_context::race_prepared_virt_against_host(
                edit("virt"),
                edit("host"),
                host_delay_ms,
                verdict,
                rename_workspace_edit_has_result,
            )
            .await;
            match verdict {
                Verdict::Current => assert_eq!(won.unwrap(), Some(edit("virt"))),
                Verdict::Stale => {
                    assert_eq!(won.unwrap(), Some(edit("host")), "{host_delay_ms}")
                }
                Verdict::Moved => assert_eq!(
                    won.unwrap_err().code,
                    tower_lsp_server::jsonrpc::ErrorCode::ServerError(-32801),
                    "{host_delay_ms}"
                ),
            }
        }
    }
}
