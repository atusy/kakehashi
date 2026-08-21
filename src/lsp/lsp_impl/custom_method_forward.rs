//! Handlers for custom-method-host-forwarding: `kakehashi/forward/request`
//! and `kakehashi/forward/notification`.
//!
//! A method kakehashi does not implement reaches these through
//! [`crate::lsp::CustomMethodForwarder`], rewritten into
//! `{ "method", "params" }`; a client may also call them directly. Either
//! way the same eligibility applies: the host document named by
//! `params.textDocument.uri` must have `bridge._self.enabled = true` and a
//! **literal** `bridge._self.aggregation` entry for the method. The params
//! go to the host servers verbatim — real URI, no translation — and the
//! first non-empty `result` in priority order comes back untouched.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_lsp_server::jsonrpc::{Error, ErrorCode, Result};
use tower_lsp_server::ls_types::Uri;
use url::Url;

use super::bridge_context::{HostRequestContext, UpstreamRegistrySweepGuard, is_empty_layer_value};
use super::uri_to_url;
use super::{Kakehashi, is_reserved_method};
use crate::config::settings::AggregationStrategy;
use crate::lsp::aggregation::server::{dispatch_host_preferred, select_host_servers};
use crate::lsp::bridge::HostDocument;

/// Wire name of the request-forwarding method.
pub const FORWARD_REQUEST_METHOD: &str = "kakehashi/forward/request";
/// Wire name of the notification-forwarding method.
pub const FORWARD_NOTIFICATION_METHOD: &str = "kakehashi/forward/notification";

/// Params of both forwarding methods: the original method name and its
/// params, verbatim.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ForwardParams {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Why a message is not forwarded. Requests answer with the JSON-RPC error
/// each variant maps to; notifications log and drop.
#[derive(Debug, PartialEq, Eq)]
enum Rejection {
    /// No `textDocument.uri` in the params: there is no host document to
    /// pick servers for. `InvalidParams` — the method IS forwardable, the
    /// call is malformed for it.
    NoTextDocument,
    /// `textDocument.uri` is present but not an absolute URI kakehashi can
    /// key a document by. `InvalidParams`, with the offending value.
    InvalidTextDocumentUri(String),
    /// Not opted in: unknown document, host bridging off for its language,
    /// no literal aggregation entry, or no host-capable server. For a
    /// request this keeps the router's `MethodNotFound` — from the client's
    /// side nothing changed.
    NotForwardable(&'static str),
    /// A strategy other than `preferred`: results of unknown shape cannot
    /// be merged, so the entry is a configuration error.
    UnsupportedStrategy(AggregationStrategy),
    /// Lifecycle or sync method the bridge owns ([`is_reserved_method`]);
    /// refused even when configured.
    Reserved,
}

/// LSP `RequestFailed` (3.17+): the request was well-formed but cannot be
/// served — here, for a configuration or policy reason the client did not
/// cause. tower-lsp-server 0.23 has no named variant. Distinct from
/// `InvalidParams`, which would tell the client to fix its request.
const REQUEST_FAILED: ErrorCode = ErrorCode::ServerError(-32803);

fn request_failed(message: String) -> Error {
    Error {
        code: REQUEST_FAILED,
        message: message.into(),
        data: None,
    }
}

impl Rejection {
    /// Human-readable reason, without the method (callers prefix it).
    fn describe(&self) -> String {
        match self {
            Self::NoTextDocument => {
                "forwarding needs params.textDocument.uri to pick a host document".to_owned()
            }
            Self::InvalidTextDocumentUri(raw) => {
                format!("params.textDocument.uri {raw:?} is not an absolute URI")
            }
            Self::NotForwardable(reason) => (*reason).to_owned(),
            Self::UnsupportedStrategy(strategy) => format!(
                "bridge._self.aggregation strategy {strategy:?} is not supported for forwarded \
                 methods; only `preferred` can combine results of unknown shape"
            ),
            Self::Reserved => {
                "lifecycle and document-sync methods are owned by the bridge and are never \
                 forwarded"
                    .to_owned()
            }
        }
    }

    /// The JSON-RPC error a request gets. Pure: logging is the caller's.
    fn into_error(self, method: &str) -> Error {
        match self {
            Self::NoTextDocument | Self::InvalidTextDocumentUri(_) => {
                Error::invalid_params(format!("{method}: {}", self.describe()))
            }
            Self::NotForwardable(_) => {
                let mut error = Error::method_not_found();
                error.data = Some(Value::String(method.to_owned()));
                error
            }
            Self::UnsupportedStrategy(_) | Self::Reserved => {
                request_failed(format!("{method}: {}", self.describe()))
            }
        }
    }
}

/// `params.textDocument.uri`, the one field the forward reads, as both the
/// wire `Uri` (forwarded verbatim) and the `Url` the document store is keyed
/// by. Absent and malformed are told apart so the client learns which.
fn text_document_uri(params: &Value) -> std::result::Result<(Uri, Url), Rejection> {
    let raw = params
        .pointer("/textDocument/uri")
        .and_then(Value::as_str)
        .ok_or(Rejection::NoTextDocument)?;
    let lsp_uri = raw
        .parse::<Uri>()
        .map_err(|_| Rejection::InvalidTextDocumentUri(raw.to_owned()))?;
    let url =
        uri_to_url(&lsp_uri).map_err(|_| Rejection::InvalidTextDocumentUri(raw.to_owned()))?;
    Ok((lsp_uri, url))
}

/// Per-settings-generation cache behind [`Kakehashi::custom_method_gate`].
struct ForwardableMethods {
    generation: u64,
    methods: std::sync::Arc<std::collections::HashSet<String>>,
}

impl Kakehashi {
    /// A predicate for the dispatch layer: could `method` be forwarded for
    /// SOME document under the current settings? The forwarder consults it
    /// before cloning a request's params, so the standard methods — the hot
    /// path — never pay for a forward that cannot happen. Recomputed only
    /// when the settings generation moves; otherwise one atomic load, one
    /// lock, one hash lookup.
    pub fn custom_method_gate(&self) -> impl Fn(&str) -> bool + Send + Sync + Clone + 'static {
        let settings_manager = std::sync::Arc::clone(&self.settings_manager);
        let cache = std::sync::Arc::new(std::sync::Mutex::new(None::<ForwardableMethods>));
        move |method: &str| {
            let snapshot = settings_manager.load_settings_pair();
            let mut cache = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let current = match cache.as_ref() {
                Some(cached) if cached.generation == snapshot.generation => {
                    std::sync::Arc::clone(&cached.methods)
                }
                _ => {
                    let methods = std::sync::Arc::new(snapshot.settings.host_forwardable_methods());
                    *cache = Some(ForwardableMethods {
                        generation: snapshot.generation,
                        methods: std::sync::Arc::clone(&methods),
                    });
                    methods
                }
            };
            drop(cache);
            current.contains(method)
        }
    }

    /// Resolve the host servers a forwarded message goes to, or why it
    /// does not go anywhere.
    fn resolve_forward_target(
        &self,
        params: &ForwardParams,
    ) -> std::result::Result<HostRequestContext, Rejection> {
        if is_reserved_method(&params.method) {
            return Err(Rejection::Reserved);
        }
        let (lsp_uri, url) = text_document_uri(&params.params)?;
        let language = self
            .document_language(&url)
            .ok_or(Rejection::NotForwardable("document is not open"))?;
        let settings = self.settings_manager.load_settings();
        let Some(lang_settings) = settings
            .resolve_host_language_settings(&language)
            .filter(|settings| settings.has_explicit_host_aggregation(&params.method))
        else {
            return Err(Rejection::NotForwardable(
                "no literal bridge._self.aggregation entry for the method",
            ));
        };
        // Only the entry's OWN strategy counts: an inherited `concatenated`
        // from the `_` method wildcard is ignored here exactly as the typed
        // verbatim host paths ignore it.
        if let Some(strategy) = lang_settings
            .explicit_host_aggregation_strategy(&params.method)
            .filter(|strategy| *strategy != AggregationStrategy::Preferred)
        {
            return Err(Rejection::UnsupportedStrategy(strategy));
        }
        self.resolve_host_bridge_context(&lsp_uri, &params.method)
            .ok_or(Rejection::NotForwardable(
                "host bridge context unavailable (see the preceding debug line)",
            ))
    }

    /// `kakehashi/forward/request`: forward a request kakehashi does not
    /// implement to the host servers and answer with the first non-empty
    /// downstream result (`preferred`), or `null`.
    pub async fn forward_custom_request(&self, params: ForwardParams) -> Result<Value> {
        let ctx = self.resolve_forward_target(&params).map_err(|rejection| {
            if let Rejection::NotForwardable(reason) = &rejection {
                log::debug!("{:?}: not forwarded: {reason}", params.method);
            }
            rejection.into_error(&params.method)
        })?;

        // Standalone host dispatch (no layer race): the RAII sweep clears
        // the upstream-registry entries an aborted per-server task may not
        // reach itself — same discipline as willSaveWaitUntil.
        let _sweep = UpstreamRegistrySweepGuard::new(
            self.bridge.pool_arc(),
            ctx.upstream_request_id.clone(),
        );
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());
        let pool = self.bridge.pool_arc();
        let method: std::sync::Arc<str> = params.method.as_str().into();
        let raw_params = params.params;
        let result = dispatch_host_preferred(
            &ctx,
            pool.clone(),
            move |t| {
                let params = raw_params.clone();
                let method = std::sync::Arc::clone(&method);
                async move {
                    t.pool
                        .send_host_custom_request(
                            &t.server_name,
                            &t.server_config,
                            &HostDocument {
                                uri: &t.uri,
                                language_id: &t.language_id,
                                text: &t.text,
                            },
                            &method,
                            params,
                            t.upstream_id,
                        )
                        .await
                }
            },
            |opt| matches!(opt, Some(v) if !is_empty_layer_value(v)),
            cancel_rx,
        )
        .await;
        let value = self
            .host_layer_result(result, &params.method, |value| value)
            .await?;
        Ok(value.unwrap_or(Value::Null))
    }

    /// `kakehashi/forward/notification`: forward a notification kakehashi
    /// does not implement to **every** selected host server, in priority
    /// order. Nothing comes back; failures are logged per server.
    pub async fn forward_custom_notification(&self, params: ForwardParams) {
        // `{:?}` on the method: it is a client-controlled string going to a
        // line-oriented log.
        let ctx = match self.resolve_forward_target(&params) {
            Ok(ctx) => ctx,
            Err(Rejection::NotForwardable(reason)) => {
                log::debug!("{:?}: notification not forwarded: {reason}", params.method);
                return;
            }
            Err(rejection) => {
                log::warn!(
                    "{:?}: notification dropped: {}",
                    params.method,
                    rejection.describe()
                );
                return;
            }
        };
        let pool = self.bridge.pool_arc();
        let doc = HostDocument {
            uri: &ctx.uri,
            language_id: &ctx.language_id,
            text: &ctx.text,
        };
        // Current text at sync time, not the snapshot taken before the
        // per-server initialization wait (same lock-order reasoning as the
        // reader in `debounced_diagnostics`: read and dropped inside the
        // closure, never held across an await).
        let documents = std::sync::Arc::clone(&self.documents);
        let reader_uri = ctx.uri.clone();
        let live_text_reader: crate::lsp::bridge::HostTextReader =
            std::sync::Arc::new(move || documents.get(&reader_uri).map(|doc| doc.text_arc()));
        // Every selected server independently: one server's initialization
        // wait must not hold back delivery to the others.
        let deliveries = select_host_servers(&ctx).into_iter().map(|server| {
            let pool = std::sync::Arc::clone(&pool);
            let doc = &doc;
            let method = &params.method;
            let payload = params.params.clone();
            let live_text_reader = &live_text_reader;
            async move {
                if let Err(error) = pool
                    .send_host_custom_notification(
                        &server.server_name,
                        &server.config,
                        doc,
                        Some(live_text_reader),
                        method,
                        payload,
                    )
                    .await
                {
                    log::warn!(
                        "{method:?}: notification not delivered to {}: {error}",
                        server.server_name
                    );
                }
            }
        });
        futures::future::join_all(deliveries).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_params_default_params_to_null() {
        let parsed: ForwardParams =
            serde_json::from_value(serde_json::json!({ "method": "custom/x" })).unwrap();
        assert_eq!(parsed.method, "custom/x");
        assert_eq!(parsed.params, Value::Null);
    }

    #[test]
    fn text_document_uri_reads_only_the_standard_location() {
        assert!(
            text_document_uri(&serde_json::json!({ "textDocument": { "uri": "file:///a.md" } }))
                .is_ok()
        );
        for params in [
            serde_json::json!({ "uri": "file:///a.md" }),
            Value::Null,
            serde_json::json!(5),
            serde_json::json!([]),
            serde_json::json!({ "textDocument": { "uri": 7 } }),
        ] {
            assert_eq!(
                text_document_uri(&params).unwrap_err(),
                Rejection::NoTextDocument,
                "{params}"
            );
        }
    }

    #[test]
    fn malformed_text_document_uri_is_reported_as_such() {
        let params = serde_json::json!({ "textDocument": { "uri": "not a uri" } });
        assert_eq!(
            text_document_uri(&params).unwrap_err(),
            Rejection::InvalidTextDocumentUri("not a uri".to_owned())
        );
        assert_eq!(
            Rejection::InvalidTextDocumentUri("x".into())
                .into_error("custom/x")
                .code,
            ErrorCode::InvalidParams
        );
    }

    #[test]
    fn reserved_methods_cover_lifecycle_sync_and_namespaces() {
        for method in [
            "initialize",
            "shutdown",
            "exit",
            "textDocument/didClose",
            "window/workDoneProgress/cancel",
            "$/cancelRequest",
            "kakehashi/forward/request",
        ] {
            assert!(is_reserved_method(method), "{method} must be reserved");
        }
        assert!(!is_reserved_method("textDocument/inlineCompletion"));
        assert!(!is_reserved_method("custom/ping"));
    }

    #[test]
    fn rejections_map_to_the_contracted_error_codes() {
        assert_eq!(
            Rejection::NoTextDocument.into_error("custom/x").code,
            ErrorCode::InvalidParams
        );
        let not_found = Rejection::NotForwardable("why").into_error("custom/x");
        assert_eq!(not_found.code, ErrorCode::MethodNotFound);
        assert_eq!(not_found.data, Some(Value::String("custom/x".into())));
        // Config/policy refusals are RequestFailed: the client's request was
        // well-formed, so InvalidParams would send it fixing the wrong side.
        assert_eq!(
            Rejection::UnsupportedStrategy(AggregationStrategy::Concatenated)
                .into_error("custom/x")
                .code,
            ErrorCode::ServerError(-32803)
        );
        assert_eq!(
            Rejection::Reserved.into_error("shutdown").code,
            ErrorCode::ServerError(-32803)
        );
    }
}
