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
use tower_lsp_server::jsonrpc::{Error, Result};
use tower_lsp_server::ls_types::Uri;

use super::Kakehashi;
use super::bridge_context::{HostRequestContext, UpstreamRegistrySweepGuard, is_empty_layer_value};
use super::uri_to_url;
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
    /// Not opted in: unknown document, host bridging off for its language,
    /// no literal aggregation entry, or no host-capable server. For a
    /// request this keeps the router's `MethodNotFound` — from the client's
    /// side nothing changed.
    NotForwardable(&'static str),
    /// A strategy other than `preferred`: results of unknown shape cannot
    /// be merged, so the entry is a configuration error.
    UnsupportedStrategy(AggregationStrategy),
}

impl Rejection {
    fn into_error(self, method: &str) -> Error {
        match self {
            Self::NoTextDocument => Error::invalid_params(format!(
                "{method}: forwarding needs params.textDocument.uri to pick a host document"
            )),
            Self::NotForwardable(reason) => {
                log::debug!("{method}: not forwarded: {reason}");
                let mut error = Error::method_not_found();
                error.data = Some(Value::String(method.to_owned()));
                error
            }
            Self::UnsupportedStrategy(strategy) => Error::invalid_params(format!(
                "{method}: bridge._self.aggregation strategy {strategy:?} is not supported for \
                 forwarded methods; only `preferred` can combine results of unknown shape"
            )),
        }
    }
}

/// `params.textDocument.uri`, the one field the forward reads.
fn text_document_uri(params: &Value) -> Option<Uri> {
    params
        .pointer("/textDocument/uri")
        .and_then(Value::as_str)
        .and_then(|raw| raw.parse::<Uri>().ok())
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
        let lsp_uri = text_document_uri(&params.params).ok_or(Rejection::NoTextDocument)?;
        let url = uri_to_url(&lsp_uri).map_err(|_| Rejection::NoTextDocument)?;
        let language = self
            .document_language(&url)
            .ok_or(Rejection::NotForwardable("document is not open"))?;
        let explicit = self
            .settings_manager
            .load_settings()
            .resolve_host_language_settings(&language)
            .is_some_and(|settings| settings.has_explicit_host_aggregation(&params.method));
        if !explicit {
            return Err(Rejection::NotForwardable(
                "no literal bridge._self.aggregation entry for the method",
            ));
        }
        let ctx = self
            .resolve_host_bridge_context(&lsp_uri, &params.method)
            .ok_or(Rejection::NotForwardable(
                "host bridging is not opted in for the document's language, or no server handles it",
            ))?;
        if ctx.strategy != AggregationStrategy::Preferred {
            return Err(Rejection::UnsupportedStrategy(ctx.strategy));
        }
        Ok(ctx)
    }

    /// `kakehashi/forward/request`: forward a request kakehashi does not
    /// implement to the host servers and answer with the first non-empty
    /// downstream result (`preferred`), or `null`.
    pub async fn forward_custom_request(&self, params: ForwardParams) -> Result<Value> {
        let ctx = self
            .resolve_forward_target(&params)
            .map_err(|rejection| rejection.into_error(&params.method))?;

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
        let ctx = match self.resolve_forward_target(&params) {
            Ok(ctx) => ctx,
            Err(Rejection::NotForwardable(reason)) => {
                log::debug!("{}: notification not forwarded: {reason}", params.method);
                return;
            }
            Err(rejection) => {
                log::warn!(
                    "{}: notification dropped: {}",
                    params.method,
                    rejection.into_error(&params.method).message
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
        for server in select_host_servers(&ctx) {
            if let Err(error) = pool
                .send_host_custom_notification(
                    &server.server_name,
                    &server.config,
                    &doc,
                    &params.method,
                    params.params.clone(),
                )
                .await
            {
                log::warn!(
                    "{}: notification not delivered to {}: {error}",
                    params.method,
                    server.server_name
                );
            }
        }
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
                .is_some()
        );
        assert!(text_document_uri(&serde_json::json!({ "uri": "file:///a.md" })).is_none());
        assert!(text_document_uri(&Value::Null).is_none());
    }

    #[test]
    fn rejections_map_to_the_contracted_error_codes() {
        use tower_lsp_server::jsonrpc::ErrorCode;
        assert_eq!(
            Rejection::NoTextDocument.into_error("custom/x").code,
            ErrorCode::InvalidParams
        );
        let not_found = Rejection::NotForwardable("why").into_error("custom/x");
        assert_eq!(not_found.code, ErrorCode::MethodNotFound);
        assert_eq!(not_found.data, Some(Value::String("custom/x".into())));
        assert_eq!(
            Rejection::UnsupportedStrategy(AggregationStrategy::Concatenated)
                .into_error("custom/x")
                .code,
            ErrorCode::InvalidParams
        );
    }
}
