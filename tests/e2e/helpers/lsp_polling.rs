//! LSP polling utilities for E2E tests.
//!
//! Provides retry-with-timeout patterns for waiting on async LSP responses.

// These functions are shared across multiple test binaries but not all tests use every function.
// Allow dead_code to suppress per-binary warnings.
#![allow(dead_code)]

use std::time::Duration;

use super::lsp_client::LspClient;
use serde_json::json;

/// Poll until a predicate returns Some(T) or max attempts reached.
///
/// # Arguments
/// * `max_attempts` - Maximum number of polling attempts
/// * `delay_ms` - Delay between attempts in milliseconds
/// * `predicate` - Function that returns Some(T) on success, None to retry
///
/// # Returns
/// * `Some(T)` if predicate succeeded within max_attempts
/// * `None` if max_attempts reached without success
pub fn poll_until<T, F>(max_attempts: usize, delay_ms: u64, mut predicate: F) -> Option<T>
where
    F: FnMut() -> Option<T>,
{
    for attempt in 1..=max_attempts {
        if let Some(result) = predicate() {
            eprintln!(
                "poll_until succeeded on attempt {}/{}",
                attempt, max_attempts
            );
            return Some(result);
        }

        if attempt < max_attempts {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
    }

    eprintln!("poll_until exhausted {} attempts", max_attempts);
    None
}

/// Poll for LSP position-based request results with retries.
///
/// Many LSP requests (hover, completion, definition, etc.) need time for the downstream
/// language server to index files. This helper retries until a non-null result is returned.
///
/// # Arguments
/// * `client` - The LSP client to send requests through
/// * `method` - The LSP method (e.g., "textDocument/hover", "textDocument/completion")
/// * `uri` - The document URI
/// * `line` - Zero-indexed line number
/// * `character` - Zero-indexed character position
/// * `max_attempts` - Maximum number of polling attempts
/// * `delay_ms` - Delay between attempts in milliseconds
///
/// # Returns
/// * `Some(response)` - The full JSON-RPC response if a non-null result was received
/// * `None` - If max_attempts reached without a successful non-null response
pub fn poll_for_lsp_result(
    client: &mut LspClient,
    method: &str,
    uri: &str,
    line: u32,
    character: u32,
    max_attempts: u32,
    delay_ms: u64,
) -> Option<serde_json::Value> {
    for attempt in 1..=max_attempts {
        let response = client.send_request(
            method,
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }),
        );

        if response.get("error").is_some() {
            eprintln!(
                "{} attempt {}/{}: Error: {:?}",
                method,
                attempt,
                max_attempts,
                response.get("error")
            );
            std::thread::sleep(Duration::from_millis(delay_ms));
            continue;
        }

        if let Some(result) = response.get("result")
            && !result.is_null()
        {
            eprintln!(
                "{} succeeded on attempt {}/{}",
                method, attempt, max_attempts
            );
            return Some(response);
        }

        eprintln!(
            "{} attempt {}/{}: null result, retrying...",
            method, attempt, max_attempts
        );
        std::thread::sleep(Duration::from_millis(delay_ms));
    }

    eprintln!(
        "{} exhausted {} attempts without non-null result",
        method, max_attempts
    );
    None
}

/// Poll for hover results with retries.
///
/// Convenience wrapper around [`poll_for_lsp_result`] for hover requests.
pub fn poll_for_hover(
    client: &mut LspClient,
    uri: &str,
    line: u32,
    character: u32,
    max_attempts: u32,
    delay_ms: u64,
) -> Option<serde_json::Value> {
    poll_for_lsp_result(
        client,
        "textDocument/hover",
        uri,
        line,
        character,
        max_attempts,
        delay_ms,
    )
}

/// Poll for completion results with retries.
///
/// Convenience wrapper around [`poll_for_lsp_result`] for completion requests.
pub fn poll_for_completions(
    client: &mut LspClient,
    uri: &str,
    line: u32,
    character: u32,
    max_attempts: u32,
    delay_ms: u64,
) -> Option<serde_json::Value> {
    poll_for_lsp_result(
        client,
        "textDocument/completion",
        uri,
        line,
        character,
        max_attempts,
        delay_ms,
    )
}

/// Poll for diagnostic results with exponential backoff.
///
/// This is used for textDocument/diagnostic requests which may need time
/// for downstream language servers to analyze the document.
///
/// Uses exponential backoff starting at `initial_delay_ms`, doubling each retry
/// up to `max_attempts` attempts.
///
/// # Arguments
/// * `client` - The LSP client to send requests through
/// * `uri` - The document URI
/// * `max_attempts` - Maximum number of polling attempts (default: 5)
/// * `initial_delay_ms` - Initial delay between attempts in milliseconds (default: 100)
///
/// # Returns
/// * `Some(response)` - The full JSON-RPC response with non-empty diagnostics
/// * `None` - If max_attempts reached without diagnostics
pub fn poll_for_diagnostics(
    client: &mut LspClient,
    uri: &str,
    max_attempts: u32,
    initial_delay_ms: u64,
) -> Option<serde_json::Value> {
    let mut delay_ms = initial_delay_ms;

    for attempt in 1..=max_attempts {
        let response = client.send_request(
            "textDocument/diagnostic",
            json!({
                "textDocument": { "uri": uri }
            }),
        );

        if response.get("error").is_some() {
            eprintln!(
                "textDocument/diagnostic attempt {}/{}: Error: {:?}",
                attempt,
                max_attempts,
                response.get("error")
            );
            std::thread::sleep(Duration::from_millis(delay_ms));
            delay_ms = delay_ms.saturating_mul(2); // Exponential backoff
            continue;
        }

        // Check if result has items (diagnostics)
        if let Some(result) = response.get("result")
            && let Some(items) = result.get("items").and_then(|i| i.as_array())
            && !items.is_empty()
        {
            eprintln!(
                "textDocument/diagnostic succeeded on attempt {}/{} with {} diagnostics",
                attempt,
                max_attempts,
                items.len()
            );
            return Some(response);
        }

        eprintln!(
            "textDocument/diagnostic attempt {}/{}: no diagnostics yet, retrying in {}ms...",
            attempt, max_attempts, delay_ms
        );
        std::thread::sleep(Duration::from_millis(delay_ms));
        delay_ms = delay_ms.saturating_mul(2); // Exponential backoff
    }

    eprintln!(
        "textDocument/diagnostic exhausted {} attempts without diagnostics",
        max_attempts
    );
    None
}

/// Wait for the language server to be ready by polling diagnostics.
///
/// Unlike `poll_for_diagnostics`, this waits for the document to be processed
/// but doesn't require non-empty diagnostics. It just waits for a valid response.
///
/// Uses exponential backoff starting at `initial_delay_ms`.
///
/// # Arguments
/// * `client` - The LSP client to send requests through
/// * `uri` - The document URI
/// * `max_attempts` - Maximum number of polling attempts (default: 5)
/// * `initial_delay_ms` - Initial delay between attempts in milliseconds (default: 100)
pub fn wait_for_server_ready(
    client: &mut LspClient,
    uri: &str,
    max_attempts: u32,
    initial_delay_ms: u64,
) {
    let mut delay_ms = initial_delay_ms;

    for attempt in 1..=max_attempts {
        let response = client.send_request(
            "textDocument/diagnostic",
            json!({
                "textDocument": { "uri": uri }
            }),
        );

        // Any valid response (not error) indicates server is ready
        if response.get("error").is_none() && response.get("result").is_some() {
            eprintln!(
                "Server ready on attempt {}/{} (waited ~{}ms total)",
                attempt,
                max_attempts,
                (0..attempt)
                    .map(|i| initial_delay_ms * 2u64.pow(i))
                    .sum::<u64>()
            );
            return;
        }

        eprintln!(
            "Waiting for server, attempt {}/{}, delay {}ms",
            attempt, max_attempts, delay_ms
        );
        std::thread::sleep(Duration::from_millis(delay_ms));
        delay_ms = delay_ms.saturating_mul(2);
    }

    eprintln!(
        "Server wait exhausted {} attempts, proceeding anyway",
        max_attempts
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_poll_until_succeeds_immediately() {
        let result = poll_until(5, 10, || Some(42));
        assert_eq!(result, Some(42));
    }

    #[test]
    fn test_poll_until_succeeds_after_retries() {
        let mut counter = 0;
        let result = poll_until(5, 10, || {
            counter += 1;
            if counter >= 3 { Some(counter) } else { None }
        });
        assert_eq!(result, Some(3));
    }

    #[test]
    fn test_poll_until_exhausts_attempts() {
        let result = poll_until(3, 10, || None::<i32>);
        assert_eq!(result, None);
    }
}
