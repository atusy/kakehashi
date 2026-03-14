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

/// Helper trait to convert PoisonError to LspError
pub trait LockResultExt<T> {
    /// Convert a PoisonError to LspError with recovery and logging.
    ///
    /// The context parameter identifies which operation triggered lock recovery,
    /// helping developers debug thread safety issues.
    fn recover_poison(self, context: &str) -> Result<T, LspError>;
}

impl<T> LockResultExt<T> for Result<T, PoisonError<T>> {
    fn recover_poison(self, context: &str) -> Result<T, LspError> {
        match self {
            Ok(guard) => Ok(guard),
            Err(poisoned) => {
                log::warn!(
                    target: "kakehashi::lock_recovery",
                    "Recovered from poisoned lock in {}",
                    context
                );
                Ok(poisoned.into_inner())
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
