//! LSP protocol types and transformations for bridge communication.
//!
//! This module provides types for virtual document URIs and message
//! transformation between host and virtual document coordinates.
//!
//! ## Module Structure
//!
//! - `client_capabilities` - Baseline client capabilities advertised downstream
//! - `jsonrpc` - JSON-RPC message types and error inspection
//! - `lifecycle` - Initialize/shutdown message builders
//! - `request_id` - RequestId type for type-safe request ID handling
//! - `virtual_uri` - VirtualDocumentUri type for encoding injection region references
//! - `request` - Request builders for downstream language servers
//! - `response` - Response transformers for coordinate translation
//! - `translation` - Position/range translation between host and virtual coordinates
//! - `workspace_edit` - WorkspaceEdit virtual→host transformation

mod client_capabilities;
mod jsonrpc;
mod lifecycle;
mod request;
mod request_id;
mod response;
mod translation;
mod virtual_uri;
mod workspace_edit;

// Re-export all public items for external use
pub(crate) use jsonrpc::{
    JsonRpcNotification, JsonRpcRequest, jsonrpc_error_summary, response_has_jsonrpc_error,
};
pub(crate) use lifecycle::*;
pub(crate) use request::*;
pub(crate) use request_id::RequestId;
pub(crate) use response::*;
pub(crate) use translation::*;
pub(crate) use virtual_uri::VirtualDocumentUri;
pub(crate) use workspace_edit::transform_workspace_edit_to_host;
