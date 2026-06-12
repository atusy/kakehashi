//! `textDocument/onTypeFormatting` bridge handler (#354).
//!
//! Position-based like hover/completion, but returns `TextEdit[]` that must be
//! translated virtual→host like formatting — so the request side reuses
//! [`build_text_document_position_params`] and the response side reuses
//! [`super::formatting::transform_formatting_response_to_host`].
//!
//! # Double filter (#354)
//!
//! kakehashi advertises a config-driven trigger-character union upstream
//! (downstream trigger sets are unknown at initialize time). A typed character
//! from that union is forwarded to a downstream server only when that server's
//! own `documentOnTypeFormattingProvider` declares it, so a misconfigured
//! superset degrades to no-ops instead of bad requests.

use std::io;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{
    DocumentOnTypeFormattingOptions, DocumentOnTypeFormattingParams, FormattingOptions, Position,
    TextEdit,
};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri,
    build_text_document_position_params,
};
use super::formatting::{count_lines, transform_formatting_response_to_host};

impl LanguageServerPool {
    /// Send an onTypeFormatting request and wait for the response.
    ///
    /// Returns `Ok(None)` when the downstream server does not advertise
    /// `documentOnTypeFormattingProvider`, or advertises it without declaring
    /// the typed character `ch` as a trigger.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_on_type_formatting_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        host_position: Position,
        ch: &str,
        options: FormattingOptions,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<Vec<TextEdit>>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/onTypeFormatting") {
            return Ok(None);
        }
        let static_provider = handle
            .server_capabilities()
            .and_then(|caps| caps.document_on_type_formatting_provider.as_ref());
        if !downstream_declares_trigger(static_provider, ch) {
            log::debug!(
                target: "kakehashi::bridge",
                "[{}] onTypeFormatting: downstream does not declare {:?} as a trigger; skipping",
                server_name,
                ch
            );
            return Ok(None);
        }
        let virtual_line_count = count_lines(virtual_content);
        self.execute_position_bridge_request_with_handle(
            handle,
            server_name,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            host_position,
            "textDocument/onTypeFormatting",
            |virtual_uri, request_id| {
                build_on_type_formatting_request(
                    virtual_uri,
                    host_position,
                    ch,
                    options,
                    &offset,
                    request_id,
                )
            },
            |response, ctx| {
                transform_formatting_response_to_host(response, ctx.offset, virtual_line_count)
            },
        )
        .await
    }
}

/// Whether the downstream's `documentOnTypeFormattingProvider` declares `ch`
/// as a trigger character (the second half of the double filter).
///
/// `static_provider` is the provider from the initialize response, if any.
/// `None` means the method support came only from dynamic registration, whose
/// trigger set lives in registration options the bridge does not parse —
/// forward in that case and let the server answer (it may return null). Only
/// called after `has_capability` confirmed the method is supported.
fn downstream_declares_trigger(
    static_provider: Option<&DocumentOnTypeFormattingOptions>,
    ch: &str,
) -> bool {
    let Some(provider) = static_provider else {
        return true;
    };
    provider.first_trigger_character == ch
        || provider
            .more_trigger_character
            .as_ref()
            .is_some_and(|more| more.iter().any(|t| t == ch))
}

/// Build a JSON-RPC onTypeFormatting request for a downstream language server:
/// position translated host→virtual, typed character and editor formatting
/// options forwarded unchanged.
fn build_on_type_formatting_request(
    virtual_uri: &VirtualDocumentUri,
    host_position: Position,
    ch: &str,
    options: FormattingOptions,
    offset: &RegionOffset,
    request_id: RequestId,
) -> JsonRpcRequest<DocumentOnTypeFormattingParams> {
    let params = DocumentOnTypeFormattingParams {
        text_document_position: build_text_document_position_params(
            virtual_uri,
            host_position,
            offset,
        ),
        ch: ch.to_string(),
        options,
    };
    JsonRpcRequest::new(request_id.as_i64(), "textDocument/onTypeFormatting", params)
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;

    fn default_options() -> FormattingOptions {
        FormattingOptions {
            tab_size: 4,
            insert_spaces: true,
            ..Default::default()
        }
    }

    #[test]
    fn on_type_formatting_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_on_type_formatting_request(
            &virtual_uri,
            test_position(),
            "}",
            default_options(),
            &RegionOffset::new(3, 0),
            test_request_id(),
        );

        assert_uses_virtual_uri(&request, "lua");
    }

    fn provider(first: &str, more: Option<Vec<&str>>) -> DocumentOnTypeFormattingOptions {
        DocumentOnTypeFormattingOptions {
            first_trigger_character: first.to_string(),
            more_trigger_character: more.map(|m| m.into_iter().map(String::from).collect()),
        }
    }

    #[test]
    fn trigger_filter_accepts_first_trigger_character() {
        let p = provider("}", Some(vec![";"]));
        assert!(downstream_declares_trigger(Some(&p), "}"));
    }

    #[test]
    fn trigger_filter_accepts_more_trigger_character() {
        let p = provider("}", Some(vec![";"]));
        assert!(downstream_declares_trigger(Some(&p), ";"));
    }

    #[test]
    fn trigger_filter_rejects_undeclared_character() {
        let p = provider("}", Some(vec![";"]));
        assert!(!downstream_declares_trigger(Some(&p), "x"));

        let no_more = provider("}", None);
        assert!(
            !downstream_declares_trigger(Some(&no_more), ";"),
            "absent moreTriggerCharacter declares nothing beyond the first"
        );
    }

    #[test]
    fn trigger_filter_trusts_dynamic_only_registration() {
        // No static provider = the method came from dynamic registration whose
        // trigger set the bridge does not parse; forward and let the server
        // answer (it may return null).
        assert!(downstream_declares_trigger(None, "x"));
    }

    #[test]
    fn on_type_formatting_request_translates_position_and_forwards_ch_and_options() {
        // Host line 5, region starts at line 3 -> virtual line 2.
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_on_type_formatting_request(
            &virtual_uri,
            test_position(),
            "}",
            default_options(),
            &RegionOffset::new(3, 0),
            RequestId::new(7),
        );

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["method"], "textDocument/onTypeFormatting");
        assert_eq!(json["params"]["position"]["line"], 2);
        assert_eq!(json["params"]["ch"], "}");
        assert_eq!(json["params"]["options"]["tabSize"], 4);
        assert_eq!(json["params"]["options"]["insertSpaces"], true);
    }
}
