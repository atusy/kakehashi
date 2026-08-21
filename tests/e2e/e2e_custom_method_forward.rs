//! E2E tests for custom-method-host-forwarding: a method kakehashi does not
//! implement is forwarded to the host document's servers when — and only
//! when — `bridge._self.aggregation` names it explicitly. Requests get the
//! first non-empty downstream result; notifications fan out to every
//! selected server. The `custom-echo` mock advertises no capability for the
//! methods, so a successful echo also proves the forward is capability-blind.

use crate::helpers::lsp_client::LspClient;
use serde_json::{Value, json};

fn mock_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

const MARKDOWN: &str = "# Title\n\nprose\n";
const MARKDOWN_URI: &str = "file:///test_custom_forward.md";

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
/// LSP `RequestFailed`: well-formed request, refused for a server-side reason.
const REQUEST_FAILED: i64 = -32803;

const FORWARDING_CONFIG: &str = r#"
[languages.markdown.bridge._self]
enabled = true

[languages.markdown.bridge._self.aggregation."custom/echo"]
priorities = ["mock-host"]

[languages.markdown.bridge._self.aggregation."custom/ping"]
priorities = ["mock-host"]

[languages.markdown.bridge._self.aggregation."custom/merged"]
priorities = ["mock-host"]
strategy = "concatenated"

# Inherited by every method entry above that sets no strategy of its own.
# The typed host methods ignore an inherited `concatenated`; so must the
# forward, or this one line would break every forwarded method.
[languages.markdown.bridge._self.aggregation._]
strategy = "concatenated"
"#;

/// One `custom-echo` mock per name, all host candidates for markdown.
fn language_servers(names: &[&str]) -> Value {
    let mut servers = serde_json::Map::new();
    for name in names {
        servers.insert(
            (*name).to_owned(),
            json!({ "cmd": [mock_bin(), "custom-echo"], "languages": ["markdown"] }),
        );
    }
    Value::Object(servers)
}

struct Session {
    client: LspClient,
    _config_dir: tempfile::TempDir,
    wire_log: std::path::PathBuf,
}

fn init_session(config_toml: &str, servers: &[&str]) -> Session {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("custom_forward.toml");
    std::fs::write(&config_path, config_toml).expect("write config");
    // Every mock appends `method\turi` per inbound message here, so a test
    // can assert what did — and did not — reach the downstream servers.
    let wire_log = config_dir.path().join("mock_wire.log");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .env(
            "MOCK_LSP_WIRE_LOG",
            wire_log.to_str().expect("temp path should be UTF-8"),
        )
        .build();
    let _init = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": { "languageServers": language_servers(servers) }
        }),
    );
    client.send_notification("initialized", json!({}));
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": MARKDOWN_URI,
                "languageId": "markdown",
                "version": 1,
                "text": MARKDOWN
            }
        }),
    );
    Session {
        client,
        _config_dir: config_dir,
        wire_log,
    }
}

fn init_client(config_toml: &str) -> Session {
    init_session(config_toml, &["mock-host"])
}

impl Session {
    fn shutdown(&mut self) {
        let _ = self.client.send_request("shutdown", json!(null));
        self.client.send_notification("exit", json!(null));
    }

    /// Methods the mocks received, in arrival order.
    fn wire_methods(&self) -> Vec<String> {
        std::fs::read_to_string(&self.wire_log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split('\t').next().map(str::to_owned))
            .collect()
    }

    /// Send `method` (an echo method) until the host server answers. Like
    /// every host request, the forward fails fast while the downstream is
    /// still initializing (an empty result; "the next request gets it"), and
    /// didOpen is what spawned it, so the first probes after open may come
    /// back `null`.
    fn echo_until_answered(&mut self, method: &str, params: Value) -> Value {
        for _ in 0..300 {
            let response = self.client.send_request(method, params.clone());
            assert!(
                response.get("error").is_none(),
                "forwarded request must not error; got {response}"
            );
            if !response["result"].is_null() {
                return response["result"].clone();
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("host server never answered {method}");
    }

    /// Poll `echo_method` until the server behind it reports at least one
    /// recorded notification (the notification and the probe are independent
    /// handler tasks, so the probe may overtake), returning the list. Bounded:
    /// the first call already waited for the server, later ones answer at once.
    fn notifications_seen_by(&mut self, echo_method: &str) -> Value {
        let params = json!({ "textDocument": { "uri": MARKDOWN_URI } });
        let mut recorded =
            self.echo_until_answered(echo_method, params.clone())["notifications"].clone();
        for _ in 0..100 {
            if recorded.as_array().is_some_and(|n| !n.is_empty()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            recorded =
                self.client.send_request(echo_method, params.clone())["result"]["notifications"]
                    .clone();
        }
        recorded
    }
}

fn error_code(response: &Value) -> Option<i64> {
    response.pointer("/error/code").and_then(Value::as_i64)
}

#[test]
fn e2e_custom_request_is_forwarded_verbatim_to_the_host_server() {
    let mut session = init_client(FORWARDING_CONFIG);

    let params = json!({
        "textDocument": { "uri": MARKDOWN_URI },
        "position": { "line": 2, "character": 1 },
        "extra": { "nested": [1, 2, 3] }
    });
    let result = session.echo_until_answered("custom/echo", params.clone());
    assert_eq!(result["method"], "custom/echo");
    assert_eq!(
        result["params"], params,
        "params must reach the server verbatim (real URI, no translation)"
    );
    assert_eq!(
        result["opened"], true,
        "the host document must be synced to the server before the request"
    );

    session.shutdown();
}

#[test]
fn e2e_custom_notification_is_forwarded_to_the_host_server() {
    let mut session = init_client(FORWARDING_CONFIG);

    let ping = json!({ "textDocument": { "uri": MARKDOWN_URI }, "n": 7 });
    session
        .client
        .send_notification("custom/ping", ping.clone());

    assert_eq!(
        session.notifications_seen_by("custom/echo"),
        json!([{ "method": "custom/ping", "params": ping }]),
        "the notification must reach the host server exactly once, verbatim"
    );

    session.shutdown();
}

#[test]
fn e2e_custom_notification_reaches_every_listed_server_unless_capped() {
    const TWO_SERVERS: &str = r#"
[languages.markdown.bridge._self]
enabled = true

# One private echo per server, so each server's record can be read alone.
[languages.markdown.bridge._self.aggregation."custom/echoA"]
priorities = ["mock-a"]
[languages.markdown.bridge._self.aggregation."custom/echoB"]
priorities = ["mock-b"]

[languages.markdown.bridge._self.aggregation."custom/ping"]
priorities = ["mock-b", "mock-a"]

[languages.markdown.bridge._self.aggregation."custom/capped"]
priorities = ["mock-b", "mock-a"]
maxFanOut = 1
"#;
    let mut session = init_session(TWO_SERVERS, &["mock-a", "mock-b"]);

    let ping = json!({ "textDocument": { "uri": MARKDOWN_URI }, "n": 1 });
    session
        .client
        .send_notification("custom/ping", ping.clone());
    let expected_ping = json!([{ "method": "custom/ping", "params": ping }]);
    assert_eq!(session.notifications_seen_by("custom/echoB"), expected_ping);
    assert_eq!(
        session.notifications_seen_by("custom/echoA"),
        expected_ping,
        "every listed server receives the notification, not only the first"
    );

    let capped = json!({ "textDocument": { "uri": MARKDOWN_URI }, "n": 2 });
    session
        .client
        .send_notification("custom/capped", capped.clone());
    // A second uncapped ping after the capped one. Each server's deliveries
    // arrive in send order, so once mock-a shows this marker, a wrongly
    // fanned-out capped ping would already be visible before it — "not
    // arrived yet" cannot fake a pass.
    let marker = json!({ "textDocument": { "uri": MARKDOWN_URI }, "n": 3 });
    session
        .client
        .send_notification("custom/ping", marker.clone());
    let expected_capped = json!({ "method": "custom/capped", "params": capped });
    let expected_marker = json!({ "method": "custom/ping", "params": marker });
    let settled = |session: &mut Session, echo: &str| -> Value {
        for _ in 0..300 {
            let seen = session.notifications_seen_by(echo);
            if seen
                .as_array()
                .is_some_and(|n| n.last() == Some(&expected_marker))
            {
                return seen;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("{echo}: marker ping never arrived");
    };
    assert_eq!(
        settled(&mut session, "custom/echoB"),
        json!([expected_ping[0], expected_capped, expected_marker]),
        "mock-b is first in priorities: the one server under maxFanOut = 1"
    );
    assert_eq!(
        settled(&mut session, "custom/echoA"),
        json!([expected_ping[0], expected_marker]),
        "maxFanOut = 1 must stop the capped notification at the first server"
    );

    session.shutdown();
}

#[test]
fn e2e_empty_priorities_answer_null_without_forwarding() {
    // The per-method kill switch: eligible, selects nobody. A request
    // answers null (no error, no downstream traffic), as for typed methods.
    let mut session = init_client(
        r#"
[languages.markdown.bridge._self]
enabled = true

[languages.markdown.bridge._self.aggregation."custom/echo"]
priorities = ["mock-host"]

[languages.markdown.bridge._self.aggregation."custom/nobody"]
priorities = []
"#,
    );
    session.echo_until_answered(
        "custom/echo",
        json!({ "textDocument": { "uri": MARKDOWN_URI } }),
    );

    let response = session.client.send_request(
        "custom/nobody",
        json!({ "textDocument": { "uri": MARKDOWN_URI } }),
    );
    assert!(response.get("error").is_none(), "got {response}");
    assert_eq!(response["result"], Value::Null);
    assert!(
        !session.wire_methods().iter().any(|m| m == "custom/nobody"),
        "an empty allowlist must reach no server"
    );

    session.shutdown();
}

#[test]
fn e2e_unlisted_custom_request_keeps_method_not_found() {
    let mut session = init_client(FORWARDING_CONFIG);

    let response = session.client.send_request(
        "custom/unlisted",
        json!({ "textDocument": { "uri": MARKDOWN_URI } }),
    );
    assert_eq!(
        error_code(&response),
        Some(METHOD_NOT_FOUND),
        "a method without a literal aggregation entry is not forwarded; got {response}"
    );

    session.shutdown();
}

#[test]
fn e2e_custom_request_for_an_unopened_document_keeps_method_not_found() {
    let mut session = init_client(FORWARDING_CONFIG);

    let response = session.client.send_request(
        "custom/echo",
        json!({ "textDocument": { "uri": "file:///never_opened.md" } }),
    );
    assert_eq!(
        error_code(&response),
        Some(METHOD_NOT_FOUND),
        "only an open host document has servers to forward to; got {response}"
    );
    assert!(
        !session.wire_methods().iter().any(|m| m == "custom/echo"),
        "nothing must reach the downstream for an unopened document"
    );

    session.shutdown();
}

#[test]
fn e2e_custom_request_without_host_opt_in_keeps_method_not_found() {
    // The aggregation entry exists but `_self.enabled` does not: the host
    // layer is off, so nothing is forwarded and the router's answer stands.
    let mut session = init_client(
        r#"
[languages.markdown.bridge._self.aggregation."custom/echo"]
priorities = ["mock-host"]
"#,
    );

    let response = session.client.send_request(
        "custom/echo",
        json!({ "textDocument": { "uri": MARKDOWN_URI } }),
    );
    assert_eq!(
        error_code(&response),
        Some(METHOD_NOT_FOUND),
        "got {response}"
    );

    session.shutdown();
}

#[test]
fn e2e_custom_request_without_text_document_is_invalid_params() {
    let mut session = init_client(FORWARDING_CONFIG);

    for params in [json!({ "no": "document" }), json!(null), json!(5)] {
        let response = session.client.send_request("custom/echo", params.clone());
        assert_eq!(
            error_code(&response),
            Some(INVALID_PARAMS),
            "a forwardable method needs textDocument.uri to pick a host; params {params}, got \
             {response}"
        );
    }

    session.shutdown();
}

#[test]
fn e2e_concatenated_strategy_on_a_forwarded_request_is_request_failed() {
    let mut session = init_client(FORWARDING_CONFIG);

    let response = session.client.send_request(
        "custom/merged",
        json!({ "textDocument": { "uri": MARKDOWN_URI } }),
    );
    assert_eq!(
        error_code(&response),
        Some(REQUEST_FAILED),
        "only `preferred` can combine results of unknown shape; got {response}"
    );

    session.shutdown();
}

#[test]
fn e2e_reserved_method_is_refused_even_when_configured() {
    let mut session = init_client(
        r#"
[languages.markdown.bridge._self]
enabled = true

[languages.markdown.bridge._self.aggregation."custom/echo"]
priorities = ["mock-host"]

[languages.markdown.bridge._self.aggregation."shutdown"]
priorities = ["mock-host"]
"#,
    );
    // Make sure the server is up, so a leaked shutdown would be observable.
    session.echo_until_answered(
        "custom/echo",
        json!({ "textDocument": { "uri": MARKDOWN_URI } }),
    );

    let response = session.client.send_request(
        "kakehashi/forward/request",
        json!({ "method": "shutdown", "params": { "textDocument": { "uri": MARKDOWN_URI } } }),
    );
    assert_eq!(
        error_code(&response),
        Some(REQUEST_FAILED),
        "got {response}"
    );
    assert!(
        !session.wire_methods().iter().any(|m| m == "shutdown"),
        "a forwarded shutdown must never reach a downstream server"
    );

    session.shutdown();
}

#[test]
fn e2e_built_in_method_is_not_shadowed_by_forwarding() {
    // `textDocument/hover` has a handler; the router answers it and the
    // forward never runs — proven by the mock's wire log, not by the answer
    // (the mock answers hover and a forwarded hover alike, so the response
    // alone could not tell a shadowed router from a working one).
    let mut session = init_client(
        r#"
[languages.markdown.bridge._self]
enabled = true

[languages.markdown.bridge._self.aggregation."custom/echo"]
priorities = ["mock-host"]

[languages.markdown.bridge._self.aggregation."textDocument/hover"]
priorities = ["mock-host"]
"#,
    );
    // The server is up and has the document: a forward WOULD reach it now.
    session.echo_until_answered(
        "custom/echo",
        json!({ "textDocument": { "uri": MARKDOWN_URI } }),
    );

    let response = session.client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": MARKDOWN_URI },
            "position": { "line": 0, "character": 0 }
        }),
    );
    assert!(response.get("error").is_none(), "got {response}");
    assert_eq!(
        response["result"],
        Value::Null,
        "the mock advertises no hoverProvider, so the typed host path skips it"
    );
    assert!(
        !session
            .wire_methods()
            .iter()
            .any(|m| m == "textDocument/hover"),
        "hover must be answered by kakehashi's handler, never forwarded blind; wire: {:?}",
        session.wire_methods()
    );

    session.shutdown();
}
