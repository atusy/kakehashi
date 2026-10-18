//! Completion method for Kakehashi.
//!
//! Walks the resolved layer order (cross-layer-aggregation): the virt layer
//! bridges the injection region under the cursor, the host layer
//! (host-document-bridge) bridges the host document itself with the real URI
//! and the response verbatim. The first layer producing completion items, or an
//! incomplete `CompletionList` that asks the client to re-query, wins
//! (`preferred`).
//!
//! The host layer cannot use the generic verbatim raw-value walk: its items
//! need the routing envelope that makes `completionItem/resolve` reach the host
//! server that produced them (#958), so it dispatches typed per server to keep
//! the server name and its advertised capabilities, and hands both to the
//! bridge's `bridge_host_completion_items` policy.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    CompletionList, CompletionParams, CompletionResponse, Position, Uri,
};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::{
    HostFanOutTask, dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::{HostDocument, bridge_host_completion_items};
use crate::lsp::lsp_impl::bridge_context::parse_host_verbatim;

const METHOD: &str = "textDocument/completion";

impl Kakehashi {
    pub(crate) async fn completion_impl(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let requery = std::sync::atomic::AtomicBool::new(false);
        let virt = self.completion_virt_layer(&lsp_uri, position, &requery);
        let host = self.completion_host_layer(&lsp_uri, raw_params);
        let (response, layers) = self
            .walk_layer_futures_in_order(
                &lsp_uri,
                METHOD,
                METHOD,
                virt,
                host,
                std::future::ready(Ok(None)),
                completion_response_has_result,
            )
            .await;
        let asked = requery.load(std::sync::atomic::Ordering::Relaxed);
        match response {
            Ok(response) => {
                let requery = asked && keeps_requery_ask(response.is_some(), &layers);
                Ok(asking_to_requery(response, requery))
            }
            // A layer failing is no answer of its own, and the ask is all
            // the virt layer said: an empty incomplete list.
            Err(error) if asked && !error_is_cancel(&error) => Ok(asking_to_requery(None, true)),
            Err(error) => Err(error),
        }
    }

    /// Host layer: forward the params verbatim to the host language's own
    /// servers, then envelope each item so a later `completionItem/resolve`
    /// routes back to the server that produced it (#958). Coordinates stay
    /// untranslated — they are already real.
    ///
    /// The envelope is minted only for a server that advertises
    /// `completionItem/resolve` (reserved-key collision aside): for one that
    /// does not, it would be pure wire weight on every item of every
    /// completion, and the resolve would fail soft back to the unresolved
    /// item anyway.
    ///
    /// Minting also happens only for the server that WINS the fan-in. Each
    /// fanned-out server carries its identity back beside its response
    /// ([`HostCompletion`]) instead of enveloping in its own task, so a
    /// multi-server `bridge._self` does not pay a per-item pass in every
    /// losing task on every keystroke only to discard it.
    async fn completion_host_layer(
        &self,
        lsp_uri: &Uri,
        raw_params: serde_json::Value,
    ) -> Result<Option<CompletionResponse>> {
        let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, METHOD) else {
            return Ok(None);
        };
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());
        // The lifetime the text was read under travels with the items and a
        // reply synchronized under another one is refused: a close and reopen
        // racing the request would otherwise answer for the closed text under
        // the reopened document's incarnation.
        let expected_incarnation = ctx.incarnation;
        let revision = crate::lsp::bridge::HostRevision {
            incarnation: expected_incarnation,
            content_version: ctx.content_version,
        };
        let pool = self.bridge.pool_arc();
        let f = move |t: HostFanOutTask| {
            let params = raw_params.clone();
            async move {
                let raw = t
                    .pool
                    .send_host_raw_request(
                        &t.server_name,
                        &t.server_config,
                        &HostDocument {
                            uri: &t.uri,
                            language_id: &t.language_id,
                            text: &t.text,
                            revision: Some(revision),
                        },
                        METHOD,
                        params,
                        t.upstream_id,
                    )
                    .await?;
                let Some(raw) = raw else {
                    return Ok(None);
                };
                if raw.incarnation != expected_incarnation {
                    log::debug!(
                        target: "kakehashi::bridge",
                        "textDocument/completion (host): {} was reopened while {} answered; discarding items computed on the closed text",
                        t.uri,
                        t.server_name
                    );
                    return Ok(None);
                }
                let Some(response) = parse_host_verbatim::<CompletionResponse>(raw.value) else {
                    return Ok(None);
                };
                Ok(Some(HostCompletion {
                    response,
                    incarnation: expected_incarnation,
                    // Two allocations per SERVER, versus an envelope per item
                    // in a task whose result the fan-in may well discard.
                    server_resolves: raw.handle.has_capability("completionItem/resolve"),
                    server_name: t.server_name,
                    host_uri: t.uri.into(),
                }))
            }
        };
        // No layer-level `unregister_all` here: the virt layer runs
        // concurrently under the SAME upstream id, so wiping the registry on
        // this layer's completion would drop the sibling's live cancel
        // registrations. `run_layer_race` sweeps after the whole race.
        let fan_in = dispatch_host_preferred(
            &ctx,
            pool.clone(),
            f,
            |opt| matches!(opt, Some(v) if completion_response_has_result(&v.response)),
            cancel_rx,
        )
        .await;
        // The envelope pass rides in `on_done`, so only the WINNER pays it.
        self.host_layer_result(fan_in, &ctx, METHOD, |won| {
            won.map(HostCompletion::into_enveloped_response)
        })
        .await
    }

    /// Virt layer: bridge the injection region under the cursor.
    ///
    /// On a prepared document an incomplete list without items (the
    /// server's own, or one whose every item did not map back) is no result:
    /// it would win over another server's items or the host layer's. When no
    /// server of the layer answers with items, its ask to re-query is kept in
    /// `requery` for whatever answer wins: the next keystroke can move the
    /// caret to where items map (off an emptied gap's end, say). Asks from a
    /// layer that answered are moot (its items win again on re-query) and,
    /// arriving in any order around the winner, would make the ask depend on
    /// timing: they are not kept.
    async fn completion_virt_layer(
        &self,
        lsp_uri: &Uri,
        position: Position,
        requery: &std::sync::atomic::AtomicBool,
    ) -> Result<Option<CompletionResponse>> {
        // Use shared preamble to resolve injection context with ALL matching servers
        let Some(ctx) = self
            .resolve_bridge_contexts(lsp_uri, position, METHOD)
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        // Fan-out completion requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        // Only a prepared document's servers can ask without answering:
        // what each answered, read once the fan-in decided.
        let answers = ctx
            .document
            .prepared
            .is_some()
            .then(|| std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, bool)>::new())));
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            |t| {
                let answers = answers.clone();
                async move {
                    let server_name = answers.as_ref().map(|_| t.server_name.clone());
                    let list = t
                        .pool
                        .send_completion_request(
                            &t.server_name,
                            &t.server_config,
                            &t.uri,
                            position,
                            t.region_end(),
                            &t.injection_language,
                            &t.region_id,
                            t.offset,
                            &t.virtual_content,
                            t.upstream_id,
                        )
                        .await?;
                    let Some(answers) = answers else {
                        return Ok(list);
                    };
                    let record = |asked| {
                        if let Ok(mut answers) = answers.lock() {
                            answers.push((server_name.unwrap_or_default(), asked));
                        }
                    };
                    Ok(match list {
                        Some(list) if list.items.is_empty() && list.is_incomplete => {
                            record(true);
                            None
                        }
                        Some(list) if completion_list_has_result(&list) => {
                            record(false);
                            Some(list)
                        }
                        list => list,
                    })
                }
            },
            |opt| opt.as_ref().is_some_and(completion_list_has_result),
            cancel_rx,
        )
        .await;

        let response = result
            .handle(&self.notifier(), "completion", None, |v| {
                Ok(v.map(CompletionResponse::List))
            })
            .await?;
        if let Some(answers) = answers
            && let Ok(answers) = answers.lock()
            && asked_above_the_winner(
                &answers,
                &crate::lsp::aggregation::server::truncate_entries(
                    crate::lsp::aggregation::server::expand_priorities(
                        &ctx.document.priorities,
                        &ctx.document.configs,
                    ),
                    ctx.document.max_fan_out,
                ),
            )
        {
            requery.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(response)
    }
}

/// One host server's completion response plus the identity the resolve
/// envelope needs, carried through the fan-in so only the WINNER is enveloped.
struct HostCompletion {
    response: CompletionResponse,
    /// The server that answered — the `origin` a later resolve routes to.
    server_name: String,
    /// The real host document URI the resolve reconnects on.
    host_uri: String,
    /// Whether that server advertises `completionItem/resolve`.
    server_resolves: bool,
    /// The host lifetime the items were computed under.
    incarnation: u64,
}

impl HostCompletion {
    fn into_enveloped_response(mut self) -> CompletionResponse {
        bridge_host_completion_items(
            &mut self.response,
            &self.server_name,
            &self.host_uri,
            Some(self.incarnation),
            self.server_resolves,
        );
        self.response
    }
}

/// Whether the virt layer's ask to re-query stands for the answer the layer
/// race gave in the order `layers`: with nothing answered, or with virt
/// ranked before the host layer, the only other one answering completion
/// (native contributes none): its answer then came once virt had finished
/// without items, and a re-query may bring virt's. Not when host ranks
/// before virt: it could win before virt finished, so the ask would depend
/// on timing, and a re-query would be won by host again.
fn keeps_requery_ask(answered: bool, layers: &[crate::config::settings::LayerSource]) -> bool {
    use crate::config::settings::LayerSource::{Host, Virt};
    let rank = |layer| layers.iter().position(|candidate| *candidate == layer);
    !answered || matches!((rank(Virt), rank(Host)), (Some(virt), Some(host)) if virt < host)
}

/// Whether `error` is the client's cancellation, which no answer replaces.
fn error_is_cancel(error: &tower_lsp_server::jsonrpc::Error) -> bool {
    error.code == tower_lsp_server::jsonrpc::Error::request_cancelled().code
}

/// Whether a server of a prepared document asked to re-query, ranked
/// strictly above the server whose items won (any, with no winner): one
/// ranked above was waited for before the winner was taken, so its ask is in
/// whatever the timing, and a re-query may bring its items. One ranked below,
/// or in the winner's `"*"` group (first-win, unranked), is not counted: it
/// may not have answered yet, and the winner wins again on re-query.
/// `answers`: each answering server's name, and whether it asked (or
/// answered with items).
/// `entries`: the fan-in's priority walk.
fn asked_above_the_winner(
    answers: &[(String, bool)],
    entries: &[crate::lsp::aggregation::server::priority::PriorityEntry],
) -> bool {
    use crate::lsp::aggregation::server::priority::PriorityEntry;
    if !answers.iter().any(|(_, asked)| *asked) {
        return false;
    }
    let rank = |server: &str| {
        entries.iter().position(|entry| match entry {
            PriorityEntry::Server(name) => name == server,
            PriorityEntry::Rest(names) => names.iter().any(|name| name == server),
        })
    };
    let winner = answers
        .iter()
        .filter(|(_, asked)| !*asked)
        .filter_map(|(server, _)| rank(server))
        .min();
    answers
        .iter()
        .filter(|(_, asked)| *asked)
        .filter_map(|(server, _)| rank(server))
        .any(|asker| winner.is_none_or(|winner| asker < winner))
}

/// `response`, still asking the client to re-query when a prepared
/// document's server did (see `completion_virt_layer`): marked incomplete, or
/// an empty incomplete list when nothing else answered.
fn asking_to_requery(
    response: Option<CompletionResponse>,
    requery: bool,
) -> Option<CompletionResponse> {
    if !requery {
        return response;
    }
    let mut list = match response {
        None => CompletionList::default(),
        Some(CompletionResponse::Array(items)) => CompletionList {
            is_incomplete: false,
            items,
        },
        Some(CompletionResponse::List(list)) => list,
    };
    list.is_incomplete = true;
    Some(CompletionResponse::List(list))
}

fn completion_response_has_result(resp: &CompletionResponse) -> bool {
    match resp {
        CompletionResponse::Array(items) => !items.is_empty(),
        CompletionResponse::List(list) => completion_list_has_result(list),
    }
}

fn completion_list_has_result(list: &CompletionList) -> bool {
    // Keep the incomplete-empty rule in sync with `is_empty_layer_value`,
    // which applies the same LSP CompletionList semantics to raw JSON.
    list.is_incomplete || !list.items.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_ranked_above_the_winner_asks_for_the_answer() {
        use crate::lsp::aggregation::server::priority::PriorityEntry::{Rest, Server};
        let entries = [
            Server("a".to_string()),
            Server("b".to_string()),
            Rest(vec!["c".to_string(), "d".to_string()]),
        ];
        let answers = |answers: &[(&str, bool)]| -> Vec<(String, bool)> {
            answers
                .iter()
                .map(|(server, asked)| (server.to_string(), *asked))
                .collect()
        };
        // Above the winner: asks.
        assert!(asked_above_the_winner(
            &answers(&[("a", true), ("b", false)]),
            &entries
        ));
        // Nothing won: any ask.
        assert!(asked_above_the_winner(&answers(&[("d", true)]), &entries));
        // Below the winner, or in its `*` group: no ask.
        assert!(!asked_above_the_winner(
            &answers(&[("a", false), ("b", true)]),
            &entries
        ));
        assert!(!asked_above_the_winner(
            &answers(&[("c", false), ("d", true)]),
            &entries
        ));
        assert!(!asked_above_the_winner(&answers(&[("a", false)]), &entries));
    }

    #[test]
    fn a_requery_ask_stands_unless_the_host_layer_ranks_above_virt() {
        use crate::config::settings::LayerSource::{Host, Native, Virt};
        assert!(keeps_requery_ask(false, &[Host, Virt]));
        assert!(keeps_requery_ask(true, &[Virt, Host, Native]));
        assert!(keeps_requery_ask(true, &[Native, Virt, Host]));
        assert!(!keeps_requery_ask(true, &[Host, Virt, Native]));
        assert!(!keeps_requery_ask(true, &[]));
    }

    #[test]
    fn a_requery_ask_marks_whatever_answers_incomplete() {
        use tower_lsp_server::ls_types::CompletionItem;
        let item = CompletionItem {
            label: "x".to_string(),
            ..Default::default()
        };
        assert_eq!(
            asking_to_requery(None, true),
            Some(CompletionResponse::List(CompletionList {
                is_incomplete: true,
                items: vec![]
            }))
        );
        assert_eq!(
            asking_to_requery(Some(CompletionResponse::Array(vec![item.clone()])), true),
            Some(CompletionResponse::List(CompletionList {
                is_incomplete: true,
                items: vec![item.clone()]
            }))
        );
        // Without the ask, answers pass as they are.
        assert_eq!(
            asking_to_requery(Some(CompletionResponse::Array(vec![item.clone()])), false),
            Some(CompletionResponse::Array(vec![item]))
        );
        assert_eq!(asking_to_requery(None, false), None);
    }

    #[test]
    fn incomplete_empty_completion_list_counts_as_result() {
        let response = CompletionResponse::List(CompletionList {
            is_incomplete: true,
            items: vec![],
        });

        assert!(completion_response_has_result(&response));
    }

    #[test]
    fn complete_empty_completion_list_counts_as_empty() {
        let response = CompletionResponse::List(CompletionList {
            is_incomplete: false,
            items: vec![],
        });

        assert!(!completion_response_has_result(&response));
    }
}
