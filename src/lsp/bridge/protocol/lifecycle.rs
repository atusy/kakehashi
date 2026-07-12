//! LSP lifecycle message builders.
//!
//! Provides builders for initialize, shutdown, and exit messages
//! used during connection lifecycle management.

use std::str::FromStr;

use tower_lsp_server::ls_types::{
    ClientCapabilities, DidChangeConfigurationParams, DidChangeWorkspaceFoldersParams,
    DidCloseTextDocumentParams, InitializeParams, InitializedParams, ServerCapabilities,
    TextDocumentIdentifier, Uri, WorkspaceFolder, WorkspaceFoldersChangeEvent,
};

use super::client_capabilities::build_bridge_client_capabilities;
use super::jsonrpc::{JsonRpcNotification, JsonRpcRequest};
use super::request_id::RequestId;

/// Build an LSP initialize request.
///
/// `root_uri` and `workspace_folders` are forwarded from the upstream client;
/// `upstream_capabilities` are merged into the bridge defaults. `experimental`
/// is the process-wide `KAKEHASHI_EXPERIMENTAL=true` opt-in, threaded through
/// to the declared client capabilities.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_initialize_request(
    request_id: RequestId,
    initialization_options: Option<serde_json::Value>,
    root_uri: Option<String>,
    workspace_folders: Option<Vec<WorkspaceFolder>>,
    upstream_capabilities: Option<&ClientCapabilities>,
    advertise_configuration: bool,
    experimental: bool,
) -> JsonRpcRequest<InitializeParams> {
    let root_path = root_uri.as_deref().and_then(|uri| {
        url::Url::parse(uri)
            .ok()
            .and_then(|u| u.to_file_path().ok())
            .map(|p| p.to_string_lossy().into_owned())
    });
    let root_uri = root_uri.as_deref().and_then(|s| Uri::from_str(s).ok());

    #[allow(deprecated)] // root_uri and root_path are deprecated in favor of workspace_folders
    let params = InitializeParams {
        process_id: Some(std::process::id()),
        root_uri,
        root_path,
        workspace_folders,
        capabilities: build_bridge_client_capabilities(
            upstream_capabilities,
            advertise_configuration,
            experimental,
        ),
        initialization_options,
        ..Default::default()
    };

    JsonRpcRequest::new(request_id.as_i64(), "initialize", params)
}

/// Build an LSP initialized notification.
///
/// Sent after receiving the initialize response to signal
/// that the client is ready to receive requests.
pub(crate) fn build_initialized_notification() -> JsonRpcNotification<InitializedParams> {
    JsonRpcNotification::new("initialized", InitializedParams {})
}

/// Build an LSP shutdown request.
pub(crate) fn build_shutdown_request(request_id: RequestId) -> JsonRpcRequest<()> {
    JsonRpcRequest::new(request_id.as_i64(), "shutdown", ())
}

/// Build an LSP exit notification.
///
/// Sent after receiving the shutdown response to terminate the server.
pub(crate) fn build_exit_notification() -> JsonRpcNotification<()> {
    JsonRpcNotification::new("exit", ())
}

/// Build a textDocument/didClose notification.
pub(crate) fn build_didclose_notification(
    uri: &str,
) -> Option<JsonRpcNotification<DidCloseTextDocumentParams>> {
    let uri = match Uri::from_str(uri) {
        Ok(u) => u,
        Err(e) => {
            log::warn!(
                target: "kakehashi::bridge",
                "Skipping didClose for invalid URI '{}': {}", uri, e
            );
            return None;
        }
    };

    let params = DidCloseTextDocumentParams {
        text_document: TextDocumentIdentifier { uri },
    };

    Some(JsonRpcNotification::new("textDocument/didClose", params))
}

/// Build a `workspace/didChangeWorkspaceFolders` notification announcing newly
/// added workspace folders (#391). Removal is not modeled — the shared-instance
/// opt-in only ever grows a connection's folder set; idle eviction is a
/// separate follow-up.
pub(crate) fn build_did_change_workspace_folders_notification(
    added: Vec<WorkspaceFolder>,
) -> JsonRpcNotification<DidChangeWorkspaceFoldersParams> {
    let params = DidChangeWorkspaceFoldersParams {
        event: WorkspaceFoldersChangeEvent {
            added,
            removed: Vec::new(),
        },
    };
    JsonRpcNotification::new("workspace/didChangeWorkspaceFolders", params)
}

/// Build a `workspace/didChangeConfiguration` notification carrying this
/// server's workspace settings (downstream-settings-propagation).
///
/// Sent after `initialized` to seed push-model servers, and again whenever the
/// merged settings for this server change. Pull-model servers ignore the
/// `settings` payload and re-request via `workspace/configuration` instead.
pub(crate) fn build_did_change_configuration_notification(
    settings: serde_json::Value,
) -> JsonRpcNotification<DidChangeConfigurationParams> {
    JsonRpcNotification::new(
        "workspace/didChangeConfiguration",
        DidChangeConfigurationParams { settings },
    )
}

/// Validates a JSON-RPC initialize response.
///
/// Uses lenient interpretation to maximize compatibility with non-conformant
/// servers: a non-null `error` field is prioritized, a null `error` alongside a
/// result is accepted, and a null/missing result is rejected.
pub(crate) fn validate_initialize_response(response: &serde_json::Value) -> std::io::Result<()> {
    // 1. Check for error response (prioritize error if present)
    if let Some(error) = response.get("error").filter(|e| !e.is_null()) {
        // Error field is non-null: treat as error regardless of result
        let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");

        return Err(std::io::Error::other(format!(
            "bridge: initialize failed (code {}): {}",
            code, message
        )));
    }

    // 2. Reject if result is absent or null
    let Some(result) = response.get("result").filter(|r| !r.is_null()) else {
        return Err(std::io::Error::other(
            "bridge: initialize response missing valid result",
        ));
    };

    if let Some(capabilities) = result
        .get("capabilities")
        .filter(|capabilities| !capabilities.is_null())
    {
        serde_json::from_value::<ServerCapabilities>(capabilities.clone()).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("bridge: invalid initialize capabilities: {error}"),
            )
        })?;
    }

    validate_utf16_encoding(
        result
            .get("capabilities")
            .and_then(|capabilities| capabilities.get("positionEncoding")),
        "capabilities.positionEncoding",
    )?;
    validate_utf16_encoding(result.get("offsetEncoding"), "offsetEncoding")?;

    Ok(())
}

fn validate_utf16_encoding(
    announced: Option<&serde_json::Value>,
    field: &str,
) -> std::io::Result<()> {
    let Some(announced) = announced else {
        return Ok(());
    };
    if announced.is_null() {
        // Optional JSON-RPC fields are commonly serialized as explicit null;
        // like absence, that announces no alternative to the UTF-16 default.
        return Ok(());
    }
    let Some(encoding) = announced.as_str() else {
        return Err(std::io::Error::other(format!(
            "bridge: downstream initialize {field} is non-string; UTF-16 is required"
        )));
    };
    if encoding != "utf-16" {
        return Err(std::io::Error::other(format!(
            "bridge: downstream initialize {field} announced {encoding:?}; UTF-16 is required"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// Snapshot suffixes predate the runtime opt-in (they matched the old
    /// "experimental" cargo feature); both variants now run in one process.
    const EXPERIMENTAL_VARIANTS: [(bool, &str); 2] = [(false, "default"), (true, "experimental")];

    #[test]
    fn initialize_request_has_correct_structure() {
        for (experimental, suffix) in EXPERIMENTAL_VARIANTS {
            let request = build_initialize_request(
                RequestId::new(1),
                None,
                None,
                None,
                None,
                true,
                experimental,
            );

            insta::with_settings!({snapshot_suffix => suffix}, {
                insta::assert_json_snapshot!(request, {
                    ".params.processId" => "[PID]",
                });
            });
        }
    }

    #[test]
    fn initialize_request_includes_initialization_options() {
        let options = serde_json::json!({
            "settings": {
                "lua": {
                    "diagnostics": { "globals": ["vim"] }
                }
            }
        });
        for (experimental, suffix) in EXPERIMENTAL_VARIANTS {
            let request = build_initialize_request(
                RequestId::new(42),
                Some(options.clone()),
                None,
                None,
                None,
                true,
                experimental,
            );

            insta::with_settings!({snapshot_suffix => suffix}, {
                insta::assert_json_snapshot!(request, {
                    ".params.processId" => "[PID]",
                });
            });
        }
    }

    #[test]
    fn initialize_request_includes_root_uri_when_provided() {
        let root_uri = "file:///home/user/project";
        let request = build_initialize_request(
            RequestId::new(1),
            None,
            Some(root_uri.to_string()),
            None,
            None,
            true,
            false,
        );

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["rootUri"], root_uri);
    }

    #[test]
    fn initialize_request_has_null_root_uri_when_not_provided() {
        let request =
            build_initialize_request(RequestId::new(1), None, None, None, None, true, false);

        let json = serde_json::to_value(&request).unwrap();
        assert!(json["params"]["rootUri"].is_null());
    }

    #[test]
    fn initialize_request_includes_workspace_folders_when_provided() {
        let folders = vec![WorkspaceFolder {
            uri: Uri::from_str("file:///home/user/project").unwrap(),
            name: "project".to_string(),
        }];
        for (experimental, suffix) in EXPERIMENTAL_VARIANTS {
            let request = build_initialize_request(
                RequestId::new(1),
                None,
                None,
                Some(folders.clone()),
                None,
                true,
                experimental,
            );

            insta::with_settings!({snapshot_suffix => suffix}, {
                insta::assert_json_snapshot!(request, {
                    ".params.processId" => "[PID]",
                });
            });
        }
    }

    #[test]
    fn initialize_request_has_null_workspace_folders_when_not_provided() {
        let request =
            build_initialize_request(RequestId::new(1), None, None, None, None, true, false);

        let json = serde_json::to_value(&request).unwrap();
        assert!(json["params"]["workspaceFolders"].is_null());
    }

    #[test]
    fn initialize_request_includes_root_path_derived_from_root_uri() {
        let root_uri = "file:///home/user/project";
        let request = build_initialize_request(
            RequestId::new(1),
            None,
            Some(root_uri.to_string()),
            None,
            None,
            true,
            false,
        );

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["rootPath"], "/home/user/project");
    }

    #[test]
    fn initialize_request_has_null_root_path_when_no_root_uri() {
        let request =
            build_initialize_request(RequestId::new(1), None, None, None, None, true, false);

        let json = serde_json::to_value(&request).unwrap();
        assert!(json["params"]["rootPath"].is_null());
    }

    #[test]
    fn initialize_request_with_upstream_capabilities() {
        use tower_lsp_server::ls_types::{
            CompletionClientCapabilities, CompletionItemCapability,
            CompletionItemCapabilityResolveSupport, MarkupKind, TextDocumentClientCapabilities,
        };

        let upstream = ClientCapabilities {
            text_document: Some(TextDocumentClientCapabilities {
                completion: Some(CompletionClientCapabilities {
                    completion_item: Some(CompletionItemCapability {
                        documentation_format: Some(vec![
                            MarkupKind::Markdown,
                            MarkupKind::PlainText,
                        ]),
                        resolve_support: Some(CompletionItemCapabilityResolveSupport {
                            properties: vec!["documentation".to_string()],
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        for (experimental, suffix) in EXPERIMENTAL_VARIANTS {
            let request = build_initialize_request(
                RequestId::new(1),
                None,
                None,
                None,
                Some(&upstream),
                true,
                experimental,
            );

            insta::with_settings!({snapshot_suffix => suffix}, {
                insta::assert_json_snapshot!(request, {
                    ".params.processId" => "[PID]",
                });
            });
        }
    }

    #[test]
    fn initialized_notification_has_correct_structure() {
        let notification = build_initialized_notification();

        insta::assert_json_snapshot!(notification);
    }

    #[test]
    fn shutdown_request_has_correct_structure() {
        let request = build_shutdown_request(RequestId::new(99));

        insta::assert_json_snapshot!(request);
    }

    #[test]
    fn exit_notification_has_correct_structure() {
        let notification = build_exit_notification();

        insta::assert_json_snapshot!(notification);
    }

    #[test]
    fn didclose_notification_has_correct_structure() {
        let notification = build_didclose_notification("file:///project/test.lua");

        insta::assert_json_snapshot!(notification);
    }

    #[test]
    fn didclose_notification_with_virtual_uri() {
        let uri = "file:///project/kakehashi-virtual-uri-abc123.lua";
        let notification = build_didclose_notification(uri).expect("valid URI");

        let json = serde_json::to_value(&notification).unwrap();
        assert_eq!(json["params"]["textDocument"]["uri"], uri);
    }

    // Tests for validate_initialize_response

    #[rstest]
    #[case::valid_result_without_error(
        serde_json::json!({"result": {"capabilities": {}}})
    )]
    #[case::valid_result_with_null_error(
        serde_json::json!({"result": {"capabilities": {}}, "error": null})
    )]
    #[case::omitted_capabilities(serde_json::json!({"result": {}}))]
    #[case::null_capabilities(serde_json::json!({"result": {"capabilities": null}}))]
    #[case::explicit_utf16_position_encoding(
        serde_json::json!({
            "result": {"capabilities": {"positionEncoding": "utf-16"}}
        })
    )]
    #[case::legacy_utf16_offset_encoding(
        serde_json::json!({
            "result": {"capabilities": {}, "offsetEncoding": "utf-16"}
        })
    )]
    #[case::null_standard_encoding(
        serde_json::json!({
            "result": {"capabilities": {"positionEncoding": null}}
        })
    )]
    #[case::null_legacy_encoding(
        serde_json::json!({
            "result": {"capabilities": {}, "offsetEncoding": null}
        })
    )]
    #[case::complex_result_object(
        serde_json::json!({
            "result": {
                "capabilities": {
                    "textDocumentSync": 1,
                    "completionProvider": {
                        "triggerCharacters": ["."]
                    }
                },
                "serverInfo": {
                    "name": "test-server",
                    "version": "1.0.0"
                }
            }
        })
    )]
    #[trace]
    fn validate_accepts_valid_response(#[case] response: serde_json::Value) {
        assert!(
            validate_initialize_response(&response).is_ok(),
            "Expected valid response to be accepted: {:?}",
            response
        );
    }

    #[test]
    fn validate_rejects_non_object_capabilities() {
        let response = serde_json::json!({
            "result": { "capabilities": "not-an-object" }
        });

        let error = validate_initialize_response(&response)
            .expect_err("non-object server capabilities must fail the handshake");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("initialize capabilities"));
    }

    #[rstest]
    #[case::null_result(
        serde_json::json!({"result": null}),
        "missing valid result"
    )]
    #[case::missing_result_and_error(
        serde_json::json!({}),
        "missing valid result"
    )]
    #[case::null_result_with_null_error(
        serde_json::json!({"result": null, "error": null}),
        "missing valid result"
    )]
    #[trace]
    fn validate_rejects_missing_result(
        #[case] response: serde_json::Value,
        #[case] expected_error: &str,
    ) {
        let result = validate_initialize_response(&response);
        assert!(result.is_err(), "Expected rejection for: {:?}", response);
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(expected_error),
            "Expected error containing {:?}, got: {}",
            expected_error,
            err_msg
        );
    }

    #[rstest]
    #[case::standard_utf8(
        serde_json::json!({
            "result": {"capabilities": {"positionEncoding": "utf-8"}}
        }),
        "positionEncoding",
        "utf-8",
    )]
    #[case::legacy_utf8(
        serde_json::json!({
            "result": {"capabilities": {}, "offsetEncoding": "utf-8"}
        }),
        "offsetEncoding",
        "utf-8",
    )]
    #[case::malformed_standard_encoding(
        serde_json::json!({
            "result": {"capabilities": {"positionEncoding": 16}}
        }),
        "positionEncoding",
        "non-string",
    )]
    #[trace]
    fn validate_rejects_non_utf16_position_encoding(
        #[case] response: serde_json::Value,
        #[case] field: &str,
        #[case] announced: &str,
    ) {
        let error = validate_initialize_response(&response)
            .expect_err("non-UTF-16 downstream encoding must fail initialization");
        let message = error.to_string();
        assert!(message.contains(field), "unexpected error: {message}");
        assert!(message.contains(announced), "unexpected error: {message}");
    }

    #[rstest]
    #[case::error_response(
        serde_json::json!({
            "error": {
                "code": -32600,
                "message": "Invalid Request"
            }
        }),
        "code -32600",
        "Invalid Request"
    )]
    #[case::error_response_even_with_result(
        serde_json::json!({
            "result": {"capabilities": {}},
            "error": {
                "code": -32603,
                "message": "Internal error"
            }
        }),
        "code -32603",
        "Internal error"
    )]
    #[case::malformed_error_missing_code(
        serde_json::json!({
            "error": {
                "message": "Something went wrong"
            }
        }),
        "code -1",  // Default code
        "Something went wrong"
    )]
    #[case::malformed_error_missing_message(
        serde_json::json!({
            "error": {
                "code": -32700
            }
        }),
        "code -32700",
        "unknown error"  // Default message
    )]
    #[case::malformed_error_empty_object(
        serde_json::json!({"error": {}}),
        "code -1",
        "unknown error"
    )]
    #[case::malformed_error_wrong_types(
        serde_json::json!({
            "error": {
                "code": "not-a-number",
                "message": 123
            }
        }),
        "code -1",  // Can't parse string as i64
        "unknown error"  // Can't parse number as str
    )]
    #[trace]
    fn validate_rejects_error_response(
        #[case] response: serde_json::Value,
        #[case] expected_code: &str,
        #[case] expected_message: &str,
    ) {
        let result = validate_initialize_response(&response);
        assert!(result.is_err(), "Expected rejection for: {:?}", response);
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(expected_code),
            "Expected code {:?} in error: {}",
            expected_code,
            err_msg
        );
        assert!(
            err_msg.contains(expected_message),
            "Expected message {:?} in error: {}",
            expected_message,
            err_msg
        );
    }

    #[test]
    fn did_change_configuration_notification_carries_settings_verbatim() {
        let settings = serde_json::json!({ "rust-analyzer": { "cargo": { "features": "all" } } });
        let notif = build_did_change_configuration_notification(settings.clone());
        let wire = serde_json::to_value(&notif).expect("serializable");

        assert_eq!(wire["method"], "workspace/didChangeConfiguration");
        assert_eq!(wire["params"]["settings"], settings);
    }
}
