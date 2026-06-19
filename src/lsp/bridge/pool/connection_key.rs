//! Pool key identifying a single downstream connection.
//!
//! Historically the connection pool keyed on `server_name` alone, so the first
//! document to trigger a spawn decided the workspace root for the server's
//! whole lifetime — wrong for multi-root monorepos (issue #382). The key now
//! pairs the server name with the resolved workspace **root**, so documents
//! under different marker roots get their own downstream process while
//! documents sharing a root (or sharing the client-root fallback) still share
//! one.
//!
//! `root` is the resolved root URI string from the `root_markers` walk, or
//! `None` when no marker root applies (no document hint, non-file URI, no
//! marker found, or the `[]` kill switch) — every such document falls back to
//! the single client-rooted connection, matching the pre-#382 behavior.

use std::fmt;

/// Identity of one pooled downstream connection: `(server_name, root)`.
///
/// `Eq`/`Hash` over both fields so the same `server_name` rooted at two
/// different workspace roots maps to two distinct connections. Used as the key
/// for every per-connection map in the pool (connections, panic counts,
/// document versions, host-document sync state, the cancel registry) and stored
/// on each [`ConnectionHandle`](super::ConnectionHandle) so downstream request,
/// `didChange`, and cancel paths can recover it via `handle.key()` without
/// re-resolving the root.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ConnectionKey {
    /// Config server name (the `language_servers` map key), e.g. `tsgo`.
    server: String,
    /// Resolved workspace root URI string, or `None` for the client-root
    /// fallback. `None` keys collapse onto one shared connection.
    root: Option<String>,
}

impl ConnectionKey {
    /// Build a key for `server` rooted at `root` (the resolved root URI string,
    /// or `None` for the client-root fallback).
    pub(crate) fn new(server: impl Into<String>, root: Option<String>) -> Self {
        Self {
            server: server.into(),
            root,
        }
    }

    /// Build a key with no resolved root (client-root fallback).
    ///
    /// Used where the root is irrelevant or unresolved — test fixtures and the
    /// fallback path when no marker root applies.
    pub(crate) fn for_server(server: impl Into<String>) -> Self {
        Self::new(server, None)
    }
}

impl fmt::Display for ConnectionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.root {
            Some(root) => write!(f, "{}@{}", self.server, root),
            None => write!(f, "{}", self.server),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_server_different_root_are_distinct() {
        let a = ConnectionKey::new("tsgo", Some("file:///repo/a".to_string()));
        let b = ConnectionKey::new("tsgo", Some("file:///repo/b".to_string()));
        assert_ne!(a, b, "same server under different roots must not collide");
    }

    #[test]
    fn same_server_same_root_are_equal() {
        let a = ConnectionKey::new("tsgo", Some("file:///repo/a".to_string()));
        let b = ConnectionKey::new("tsgo", Some("file:///repo/a".to_string()));
        assert_eq!(a, b);
    }

    #[test]
    fn fallback_keys_collapse_by_server() {
        assert_eq!(
            ConnectionKey::for_server("lua"),
            ConnectionKey::for_server("lua")
        );
        assert_ne!(
            ConnectionKey::for_server("lua"),
            ConnectionKey::for_server("pyright")
        );
    }

    #[test]
    fn display_includes_root_when_present() {
        assert_eq!(ConnectionKey::for_server("lua").to_string(), "lua");
        assert_eq!(
            ConnectionKey::new("lua", Some("file:///repo".to_string())).to_string(),
            "lua@file:///repo"
        );
    }
}
