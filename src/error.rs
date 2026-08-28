//! Error handling types for kakehashi
//!
//! This module provides error types used throughout the LSP server.

use std::sync::PoisonError;
use thiserror::Error;

/// Error type for LSP operations
#[derive(Debug, Error)]
pub enum LspError {
    /// Query execution or parsing failed
    #[error("Query error: {message}")]
    Query { message: String },

    /// Generic internal error
    #[error("Internal error: {0}")]
    Internal(String),
}

/// Result type for LSP operations
pub type LspResult<T> = Result<T, LspError>;

/// LSP `ContentModified` (-32801): the staleness-reject signal of the
/// parse-snapshot model (ADR §3) — a position/range request whose coordinates
/// were authored against text newer than the latest parse snapshot cannot be
/// answered correctly, so it is rejected rather than served at wrong positions.
/// The spec does not mandate client auto-retry; the answer arrives on the
/// client's next natural request.
pub(crate) fn content_modified_error() -> tower_lsp_server::jsonrpc::Error {
    tower_lsp_server::jsonrpc::Error {
        code: tower_lsp_server::jsonrpc::ErrorCode::ServerError(-32801),
        message: std::borrow::Cow::Borrowed("content modified"),
        data: None,
    }
}

#[derive(Debug, Error)]
#[error("workspace diagnostic cancelled by downstream server: {message}")]
pub(crate) struct WorkspaceDiagnosticServerCancelled {
    message: String,
    data: Option<serde_json::Value>,
}

impl WorkspaceDiagnosticServerCancelled {
    pub(crate) fn from_response(response: &serde_json::Value) -> Option<Self> {
        let error = response.get("error")?;
        (error.get("code").and_then(serde_json::Value::as_i64) == Some(-32802)
            && error
                .pointer("/data/retriggerRequest")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true))
        .then(|| Self {
            message: error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Server cancelled")
                .to_owned(),
            data: error.get("data").cloned(),
        })
    }

    pub(crate) fn to_jsonrpc_error(&self) -> tower_lsp_server::jsonrpc::Error {
        tower_lsp_server::jsonrpc::Error {
            code: tower_lsp_server::jsonrpc::ErrorCode::ServerError(-32802),
            message: self.message.clone().into(),
            data: self.data.clone(),
        }
    }
}

pub(crate) fn map_workspace_diagnostic_error(
    error: std::io::Error,
) -> tower_lsp_server::jsonrpc::Error {
    error
        .get_ref()
        .and_then(|error| error.downcast_ref::<WorkspaceDiagnosticServerCancelled>())
        .map_or_else(
            tower_lsp_server::jsonrpc::Error::internal_error,
            WorkspaceDiagnosticServerCancelled::to_jsonrpc_error,
        )
}

/// Helper trait to recover from poisoned locks with logging.
pub trait LockResultExt<T> {
    /// Recover from a poisoned lock, logging a warning with the given context.
    ///
    /// Always succeeds — `PoisonError::into_inner()` is infallible.
    ///
    /// Accepts any `Display` value as context: pass a `&str` for static context,
    /// or `format_args!(…)` for dynamic context without heap allocation.
    fn recover_poison(self, context: impl std::fmt::Display) -> T;
}

impl<T> LockResultExt<T> for Result<T, PoisonError<T>> {
    fn recover_poison(self, context: impl std::fmt::Display) -> T {
        match self {
            Ok(guard) => guard,
            Err(poisoned) => {
                log::warn!(
                    target: "kakehashi::lock_recovery",
                    "Recovered from poisoned lock in {}",
                    context
                );
                poisoned.into_inner()
            }
        }
    }
}

impl LspError {
    pub fn query(message: impl Into<String>) -> Self {
        LspError::Query {
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        LspError::Internal(message.into())
    }
}
