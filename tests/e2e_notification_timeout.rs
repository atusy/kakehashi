//! End-to-end test pinning the exact-deadline guarantee of the E2E LspClient
//! notification waits (issue #385).
//!
//! Run with: `cargo test --test e2e_notification_timeout --features e2e`
//!
//! Background: `wait_for_notification` used to poll `BufReader::fill_buf()`,
//! which blocks on a pipe with no data. Against a server that stays *alive but
//! silent* — exactly the situation a forwarding regression produces — the wait
//! would hang indefinitely instead of returning at its deadline. The fix routes
//! reads through a background thread + `mpsc::recv_timeout`, so the deadline is
//! honored exactly. This test would hang on the old implementation and returns
//! at ~the requested timeout on the new one.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::json;
use std::time::{Duration, Instant};

#[test]
fn wait_for_notification_honors_deadline_against_silent_server() {
    let mut client = LspClient::new();

    // Bring the server up so it is genuinely alive — not exited — for the wait.
    let init = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );
    assert!(init.get("result").is_some(), "initialize should succeed");
    client.send_notification("initialized", json!({}));

    // The server will never emit this method, and we open no documents, so it
    // stays alive and silent. The wait must return None at its deadline.
    let timeout = Duration::from_secs(1);
    let start = Instant::now();
    let result = client.wait_for_notification("nonexistent/method", timeout);
    let elapsed = start.elapsed();

    assert!(
        result.is_none(),
        "no notification should arrive for a method the server never sends"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "wait must return at its deadline against an alive-but-silent server, \
         took {elapsed:?} (would hang on the pre-fix fill_buf implementation)"
    );
}
