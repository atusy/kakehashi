//! E2E tests: capability advertisement for all bridged LSP methods.
//!
//! Each `#[case]` verifies that kakehashi advertises the given capability key
//! in its `InitializeResult.capabilities`. These tests do **not** require
//! lua-language-server — they only exercise the kakehashi binary itself.
//!
//! Run with: `cargo test --test e2e_lsp_capability --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use helpers::lua_bridge::shutdown_client;
use rstest::rstest;
use serde_json::json;

#[rstest]
#[case("declarationProvider")]
#[case("documentHighlightProvider")]
#[case("documentLinkProvider")]
#[case("documentSymbolProvider")]
#[case("implementationProvider")]
#[case("inlayHintProvider")]
#[case("referencesProvider")]
#[case("renameProvider")]
#[case("typeDefinitionProvider")]
fn e2e_capability_advertised(#[case] capability: &str) {
    let mut client = LspClient::new();

    let init_response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );

    let capabilities = init_response
        .get("result")
        .and_then(|r| r.get("capabilities"))
        .expect("Should have capabilities in init response");

    assert!(
        capabilities.get(capability).is_some(),
        "{capability} should be advertised in server capabilities"
    );

    shutdown_client(&mut client);
}

/// Experimental capabilities follow the `KAKEHASHI_EXPERIMENTAL=true`
/// runtime opt-in: advertised with it, absent (or `null`) without it —
/// hermetically, even when the developer's shell exports the variable.
#[rstest]
#[case("colorProvider")]
fn e2e_experimental_capability_follows_the_env_opt_in(#[case] capability: &str) {
    for (experimental, expected) in [(true, true), (false, false)] {
        let builder = LspClient::builder();
        let mut client = if experimental {
            builder.env("KAKEHASHI_EXPERIMENTAL", "true")
        } else {
            builder.env_remove("KAKEHASHI_EXPERIMENTAL")
        }
        .build();

        let init_response = client.send_request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": null,
                "capabilities": {}
            }),
        );

        let capabilities = init_response
            .get("result")
            .and_then(|r| r.get("capabilities"))
            .expect("Should have capabilities in init response");

        let advertised = capabilities.get(capability).is_some_and(|v| !v.is_null());
        assert_eq!(
            advertised, expected,
            "{capability} advertised={advertised} with KAKEHASHI_EXPERIMENTAL set to {experimental}"
        );

        shutdown_client(&mut client);
    }
}
