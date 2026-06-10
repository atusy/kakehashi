#![cfg(feature = "e2e")]
mod helpers;
use helpers::lsp_client::LspClientBuilder;
use serde_json::json;

const DOC: &str = "# Title\n\n```python\ndef f():\n    pass\n```\n";

#[test]
fn probe_gmalloc_node_inline_then_edit() {
    let dir = tempfile::tempdir().unwrap();
    for (lang, q) in [
        ("markdown", "(atx_heading) @context\n"),
        ("python", "(function_definition) @context\n"),
    ] {
        let d = dir.path().join("queries").join(lang);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("context.scm"), q).unwrap();
    }
    let mut client = LspClientBuilder::new()
        .env("DYLD_INSERT_LIBRARIES", "/usr/lib/libgmalloc.dylib")
        .build();
    client.send_request("initialize", json!({
        "processId": std::process::id(), "rootUri": null, "capabilities": {},
        "initializationOptions": { "searchPaths": [dir.path().to_str().unwrap(), "${KAKEHASHI_DATA_DIR}"] }
    }));
    client.send_notification("initialized", json!({}));
    client.send_notification("textDocument/didOpen", json!({
        "textDocument": { "uri": "file:///g.md", "languageId": "markdown", "version": 1, "text": DOC }
    }));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        client.send_request(
            "kakehashi/node",
            json!({
                "textDocument": { "uri": "file:///g.md" },
                "position": { "line": 0, "character": 3 },
                "injection": true
            }),
        )
    }));
    eprintln!("G node request: ok={}", r.is_ok());
    if r.is_ok() {
        client.send_notification(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": "file:///g.md", "version": 2 },
                "contentChanges": [{ "text": format!("{DOC}\nmore\n") }]
            }),
        );
        std::thread::sleep(std::time::Duration::from_secs(3));
        let r2 = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.send_request("kakehashi/node", json!({
                "textDocument": { "uri": "file:///g.md" }, "position": { "line": 0, "character": 1 }
            }))
        }));
        eprintln!("G after edit: ok={}", r2.is_ok());
    }
}
