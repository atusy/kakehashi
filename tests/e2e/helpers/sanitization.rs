//! Sanitization utilities for E2E test snapshot testing.
//!
//! Provides helpers to normalize LSP responses by replacing non-deterministic
//! data (file URIs, timestamps, etc.) with stable placeholders.

// These functions are shared across multiple test binaries but not all tests use every function.
// Allow dead_code to suppress per-binary warnings.
#![allow(dead_code)]

use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

// Compiled regex patterns for temp path sanitization
// These are compiled once at first use and cached for efficiency
fn macos_temp_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"/var/folders/[^/]+/[^/]+/[TP]/[^:]+").unwrap())
}

fn linux_temp_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"/tmp/[^:]+").unwrap())
}

/// Sanitize hover response for snapshot testing.
///
/// Replaces non-deterministic data:
/// - File URIs -> "<TEST_FILE_URI>"
/// - Temp file paths in hover content -> "<TEMP_PATH>"
///
/// # Arguments
/// * `hover` - The Hover response object from LSP
///
/// # Returns
/// * Sanitized Hover object suitable for snapshot comparison
pub fn sanitize_hover_response(hover: &Value) -> Value {
    let mut sanitized = hover.clone();

    // Sanitize range if present
    if let Some(range) = sanitized.get_mut("range")
        && let Some(uri) = range.get_mut("uri")
    {
        *uri = Value::String("<TEST_FILE_URI>".to_string());
    }

    // Sanitize contents
    if let Some(contents) = sanitized.get_mut("contents") {
        *contents = sanitize_hover_contents(contents);
    }

    sanitized
}

/// Sanitize hover contents (MarkedString, MarkupContent, or array).
fn sanitize_hover_contents(contents: &Value) -> Value {
    match contents {
        Value::String(s) => Value::String(sanitize_text(s)),
        Value::Object(obj) => {
            let mut sanitized = obj.clone();
            // MarkupContent has "value" field
            if let Some(value) = sanitized.get_mut("value")
                && let Some(s) = value.as_str()
            {
                *value = Value::String(sanitize_text(s));
            }
            // MarkedString has "language" and "value" fields
            if sanitized.get("language").is_some()
                && let Some(value) = sanitized.get_mut("value")
                && let Some(s) = value.as_str()
            {
                *value = Value::String(sanitize_text(s));
            }
            Value::Object(sanitized)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sanitize_hover_contents).collect()),
        _ => contents.clone(),
    }
}

/// Sanitize text content by replacing temp file paths.
fn sanitize_text(text: &str) -> String {
    // Replace /var/folders/... (macOS temp) and /tmp/... (Linux temp) paths
    // Match the path but stop before : (line number separator)
    let sanitized = macos_temp_regex().replace_all(text, "<TEMP_PATH>");
    let sanitized = linux_temp_regex().replace_all(&sanitized, "<TEMP_PATH>");
    sanitized.to_string()
}

/// Sanitize completion response for snapshot testing.
///
/// Removes volatile `data` fields and temp paths from completion responses before snapshotting.
pub fn sanitize_completion_response(completion: &Value) -> Value {
    sanitize_recursive(completion, true)
}

/// Sanitize references response for snapshot testing.
///
/// Normalizes URIs and any embedded temp paths for deterministic snapshots.
pub fn sanitize_references_response(references: &Value) -> Value {
    sanitize_recursive(references, false)
}

/// Sanitize definition response by normalizing URIs in Location/LocationLink objects.
pub fn sanitize_definition_response(result: &Value) -> Value {
    match result {
        Value::Array(locations) => {
            Value::Array(locations.iter().map(sanitize_definition_object).collect())
        }
        Value::Object(_) => sanitize_definition_object(result),
        _ => result.clone(),
    }
}

/// Sanitize signature help response for snapshot testing.
///
/// Removes temp paths from documentation and labels while preserving signature structure.
pub fn sanitize_signature_help_response(signature_help: &Value) -> Value {
    let mut sanitized = signature_help.clone();

    // Sanitize signatures array
    if let Some(signatures) = sanitized.get_mut("signatures")
        && let Value::Array(sigs) = signatures
    {
        for sig in sigs {
            if let Value::Object(sig_obj) = sig {
                // Sanitize label
                if let Some(label) = sig_obj.get_mut("label")
                    && let Some(s) = label.as_str()
                {
                    *label = Value::String(sanitize_text(s));
                }
                // Sanitize documentation
                if let Some(doc) = sig_obj.get_mut("documentation") {
                    *doc = sanitize_signature_documentation(doc);
                }
                // Sanitize parameters
                if let Some(params) = sig_obj.get_mut("parameters")
                    && let Value::Array(param_arr) = params
                {
                    for param in param_arr {
                        if let Value::Object(param_obj) = param {
                            if let Some(label) = param_obj.get_mut("label")
                                && let Some(s) = label.as_str()
                            {
                                *label = Value::String(sanitize_text(s));
                            }
                            if let Some(doc) = param_obj.get_mut("documentation") {
                                *doc = sanitize_signature_documentation(doc);
                            }
                        }
                    }
                }
            }
        }
    }

    sanitized
}

/// Sanitize signature documentation (string or MarkupContent).
fn sanitize_signature_documentation(doc: &Value) -> Value {
    match doc {
        Value::String(s) => Value::String(sanitize_text(s)),
        Value::Object(obj) => {
            let mut sanitized = obj.clone();
            if let Some(value) = sanitized.get_mut("value")
                && let Some(s) = value.as_str()
            {
                *value = Value::String(sanitize_text(s));
            }
            Value::Object(sanitized)
        }
        _ => doc.clone(),
    }
}

/// Sanitize selection range response for snapshot testing.
///
/// SelectionRange is a recursive structure with ranges and optional parent pointers.
/// This sanitizes all ranges in the tree structure.
pub fn sanitize_selection_range_response(selection_ranges: &Value) -> Value {
    match selection_ranges {
        Value::Array(arr) => {
            Value::Array(arr.iter().map(sanitize_single_selection_range).collect())
        }
        _ => selection_ranges.clone(),
    }
}

/// Sanitize a single SelectionRange (may have parent chain).
fn sanitize_single_selection_range(selection_range: &Value) -> Value {
    match selection_range {
        Value::Object(obj) => {
            let mut sanitized = obj.clone();
            // Recursively sanitize parent if present
            if let Some(parent) = sanitized.get("parent") {
                sanitized.insert(
                    "parent".to_string(),
                    sanitize_single_selection_range(parent),
                );
            }
            Value::Object(sanitized)
        }
        _ => selection_range.clone(),
    }
}

fn sanitize_definition_object(value: &Value) -> Value {
    let mut map = value.clone();
    if let Value::Object(obj) = &mut map {
        if let Some(uri) = obj.get_mut("targetUri") {
            *uri = Value::String("<TEST_FILE_URI>".to_string());
        }
        if let Some(uri) = obj.get_mut("uri") {
            *uri = Value::String("<TEST_FILE_URI>".to_string());
        }
        if let Some(range) = obj.get_mut("range") {
            *range = sanitize_definition_range(range);
        }
        if let Some(range) = obj.get_mut("targetSelectionRange") {
            *range = sanitize_definition_range(range);
        }
    }
    map
}

fn sanitize_definition_range(value: &Value) -> Value {
    match value {
        Value::Object(obj) => {
            let mut sanitized = obj.clone();
            for v in sanitized.values_mut() {
                *v = sanitize_recursive(v, false);
            }
            Value::Object(sanitized)
        }
        _ => sanitize_recursive(value, false),
    }
}

fn sanitize_recursive(value: &Value, drop_data_field: bool) -> Value {
    match value {
        Value::String(s) => Value::String(sanitize_text(s)),
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|v| sanitize_recursive(v, drop_data_field))
                .collect(),
        ),
        Value::Object(obj) => {
            let mut map = serde_json::Map::new();
            for (key, val) in obj {
                if drop_data_field && key == "data" {
                    continue;
                }
                if key == "uri"
                    && let Some(uri) = val.as_str()
                {
                    map.insert(key.clone(), Value::String(sanitize_uri(uri)));
                    continue;
                }
                map.insert(key.clone(), sanitize_recursive(val, drop_data_field));
            }
            Value::Object(map)
        }
        _ => value.clone(),
    }
}

fn sanitize_uri(uri: &str) -> String {
    if uri.starts_with("file://") {
        "<TEST_FILE_URI>".to_string()
    } else {
        sanitize_text(uri)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_sanitize_hover_with_markup_content() {
        let hover = json!({
            "contents": {
                "kind": "markdown",
                "value": "```rust\nfn main()\n```\nDefined at /tmp/rust_abc123.md:4"
            },
            "range": {
                "start": {"line": 3, "character": 3},
                "end": {"line": 3, "character": 7}
            }
        });

        let sanitized = sanitize_hover_response(&hover);
        let contents = sanitized["contents"]["value"].as_str().unwrap();

        assert!(contents.contains("<TEMP_PATH>"));
        assert!(!contents.contains("/tmp/"));
    }

    #[test]
    fn test_sanitize_hover_with_string_contents() {
        let hover = json!({
            "contents": "fn main() at /var/folders/xy/z123/T/temp.md:4"
        });

        let sanitized = sanitize_hover_response(&hover);
        let contents = sanitized["contents"].as_str().unwrap();

        assert!(contents.contains("<TEMP_PATH>"));
        assert!(!contents.contains("/var/folders/"));
    }

    #[test]
    fn test_sanitize_text_macos_temp() {
        let text = "Defined at /var/folders/ab/cd123456/T/rust_xyz.md:10";
        let sanitized = sanitize_text(text);

        assert_eq!(sanitized, "Defined at <TEMP_PATH>:10");
    }

    #[test]
    fn test_sanitize_text_linux_temp() {
        let text = "Defined at /tmp/rust_xyz123.md:10";
        let sanitized = sanitize_text(text);

        assert_eq!(sanitized, "Defined at <TEMP_PATH>:10");
    }

    #[test]
    fn test_sanitize_text_no_temp_paths() {
        let text = "fn main() -> ()";
        let sanitized = sanitize_text(text);

        assert_eq!(sanitized, text);
    }

    #[test]
    fn test_sanitize_completion_response_removes_temp_paths() {
        let completion = json!({
            "items": [{
                "label": "example",
                "detail": "Defined at /tmp/test.file:4"
            }]
        });

        let sanitized = sanitize_completion_response(&completion);
        let detail = sanitized["items"][0]["detail"].as_str().unwrap();

        assert!(detail.contains("<TEMP_PATH>"));
        assert!(!detail.contains("/tmp/"));
    }

    #[test]
    fn test_sanitize_completion_response_removes_data_field() {
        let completion = json!([
            {
                "label": "example",
                "data": {
                    "something": "secret"
                }
            }
        ]);

        let sanitized = sanitize_completion_response(&completion);

        assert!(sanitized[0].get("data").is_none());
    }

    #[test]
    fn test_sanitize_references_response_normalizes_uri() {
        let references = json!([
            {
                "uri": "file:///tmp/tmpfile.md",
                "range": {
                    "start": {"line": 1, "character": 2},
                    "end": {"line": 1, "character": 3}
                }
            }
        ]);

        let sanitized = sanitize_references_response(&references);
        let uri = sanitized[0]["uri"].as_str().unwrap();

        assert_eq!(uri, "<TEST_FILE_URI>");
        assert!(sanitized[0]["range"]["start"]["line"].is_number());
    }

    #[test]
    fn test_sanitize_definition_response_replaces_uris() {
        let response = json!([
            {
                "targetUri": "file:///tmp/tmpfile.md",
                "targetSelectionRange": {
                    "start": { "line": 1, "character": 2 },
                    "end": { "line": 1, "character": 10 }
                }
            }
        ]);

        let sanitized = sanitize_definition_response(&response);
        let uri = sanitized[0]["targetUri"].as_str().unwrap();

        assert_eq!(uri, "<TEST_FILE_URI>");
        assert!(sanitized[0]["targetSelectionRange"]["start"]["line"].is_number());
    }
}
