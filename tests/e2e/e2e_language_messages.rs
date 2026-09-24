//! Display escaping at the real LSP notification boundary.

use crate::helpers::lsp_client::LspClient;
use serde_json::json;
use std::time::Duration;

#[test]
fn missing_parser_notification_escapes_document_language() {
    let directory = tempfile::tempdir().unwrap();
    let mut client = LspClient::builder().current_dir(directory.path()).build();
    let initialized = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "initializationOptions": {
                "searchPaths": [],
                "languages": {"_": {"autoInstall": false}}
            }
        }),
    );
    assert!(initialized.get("error").is_none(), "{initialized}");
    client.send_notification("initialized", json!({}));
    let language = "日本語\n\u{1b}[31m\u{202e}";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///missing-parser-message.unknown",
                "languageId": language,
                "version": 1,
                "text": ""
            }
        }),
    );
    let (_, params) = client
        .wait_for_notification_where(&["window/logMessage"], Duration::from_secs(10), |params| {
            params["message"]
                .as_str()
                .is_some_and(|message| message.contains("Auto-install is disabled"))
        })
        .expect("manual-install guidance must reach the client");
    assert_eq!(params["type"], 2);
    let message = params["message"].as_str().unwrap();
    assert!(
        message.contains(r"日本語\n\u{1b}[31m\u{202e}"),
        "{message:?}"
    );
    assert!(!message.chars().any(char::is_control));
}
