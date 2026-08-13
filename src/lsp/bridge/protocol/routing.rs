//! Downstream-facing `kakehashi/bridge/routing` protocol.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub(crate) const ROUTING_METHOD: &str = "kakehashi/bridge/routing";
pub(crate) const ROUTING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RoutingTextDocument {
    pub(crate) uri: String,
    pub(crate) language_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) host: Option<RoutingHostDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RoutingHostDocument {
    pub(crate) uri: String,
    pub(crate) language_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RoutingLanguageServer {
    pub(crate) languages: Vec<String>,
    /// Each marker is either one marker name or an ordered marker group.
    pub(crate) workspace_markers: Vec<serde_json::Value>,
    pub(crate) prefer_shared_instance: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RoutingParams {
    pub(crate) text_document: RoutingTextDocument,
    pub(crate) language_servers: BTreeMap<String, RoutingLanguageServer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RoutingEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) workspace_folders: Option<Option<Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) enabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RoutingAnswer {
    pub(crate) routing: BTreeMap<String, RoutingEntry>,
}

/// Parse the response body of a routing request.
///
/// Routing is fail-open: a JSON-RPC error, a null result, or a malformed
/// result means that Kakehashi keeps its own routing decision. The caller
/// separately clears the capability when the error is `MethodNotFound`.
pub(crate) fn parse_routing_response(
    response: &serde_json::Value,
) -> std::io::Result<Option<RoutingAnswer>> {
    if response.get("error").is_some_and(|error| !error.is_null()) {
        return Ok(None);
    }
    let Some(result) = response.get("result") else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bridge: routing response missing result",
        ));
    };
    if result.is_null() {
        return Ok(None);
    }
    serde_json::from_value(result.clone())
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

pub(crate) fn jsonrpc_error_code(response: &serde_json::Value) -> Option<i64> {
    response
        .get("error")
        .filter(|error| !error.is_null())
        .and_then(|error| error.get("code"))
        .and_then(serde_json::Value::as_i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serializes_host_only_and_injection_documents() {
        let mut language_servers = BTreeMap::new();
        language_servers.insert(
            "policy".to_string(),
            RoutingLanguageServer {
                languages: vec!["markdown".to_string()],
                workspace_markers: vec![serde_json::json!("deno.json")],
                prefer_shared_instance: true,
            },
        );
        let host = RoutingParams {
            text_document: RoutingTextDocument {
                uri: "file:///workspace/a.md".to_string(),
                language_id: "markdown".to_string(),
                host: None,
            },
            language_servers: language_servers.clone(),
        };
        assert_eq!(
            serde_json::to_value(host).unwrap()["textDocument"],
            serde_json::json!({"uri":"file:///workspace/a.md","languageId":"markdown"}),
        );

        let injection = RoutingParams {
            text_document: RoutingTextDocument {
                uri: "file:///tmp/virtual.lua".to_string(),
                language_id: "lua".to_string(),
                host: Some(RoutingHostDocument {
                    uri: "file:///workspace/a.md".to_string(),
                    language_id: "markdown".to_string(),
                }),
            },
            language_servers,
        };
        assert!(serde_json::to_value(injection).unwrap()["textDocument"]["host"].is_object());
    }

    #[test]
    fn response_parser_is_fail_open_and_preserves_explicit_fields() {
        assert_eq!(
            parse_routing_response(&serde_json::json!({"result":null})).unwrap(),
            None
        );
        assert_eq!(
            parse_routing_response(&serde_json::json!({"error":{"code":-32601}})).unwrap(),
            None
        );
        let answer = parse_routing_response(&serde_json::json!({
            "result": {"routing": {"lua": {"enabled": false, "workspaceFolders": []}}}
        }))
        .unwrap()
        .unwrap();
        assert_eq!(answer.routing["lua"].enabled, Some(false));
        assert_eq!(answer.routing["lua"].workspace_folders, Some(Some(vec![])));
    }

    #[test]
    fn method_not_found_code_is_identifiable() {
        assert_eq!(
            jsonrpc_error_code(&serde_json::json!({"error":{"code":-32601}})),
            Some(-32601)
        );
        assert_eq!(
            jsonrpc_error_code(&serde_json::json!({"result":null})),
            None
        );
    }
}
