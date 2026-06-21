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

/// How a [`ConnectionKey`] is rooted — the discriminator that decides which
/// downstream process a document routes to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ConnectionRoot {
    /// Resolved marker workspace root URI string: documents under distinct
    /// marker roots get distinct connections (per-root pooling, #382).
    Marker(String),
    /// No marker root applied (no document hint, non-file URI, no marker
    /// found, or the `[]` kill switch). Every such document collapses onto the
    /// single client-rooted fallback connection — the pre-#382 behavior.
    ClientFallback,
    /// One connection shared across *every* root for a `preferSharedInstance`
    /// server (#391). Kept distinct from `ClientFallback` so a marker-less
    /// document on the same server does not collide with the shared instance.
    Shared,
}

/// Identity of one pooled downstream connection: `(server_name, root)`.
///
/// `Eq`/`Hash` over both fields so the same `server_name` rooted two different
/// ways maps to two distinct connections. Used as the key for every
/// per-connection map in the pool (connections, panic counts, document
/// versions, host-document sync state, the cancel registry) and stored on each
/// [`ConnectionHandle`](super::ConnectionHandle) so downstream request,
/// `didChange`, and cancel paths can recover it via `handle.key()` without
/// re-resolving the root.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ConnectionKey {
    /// Config server name (the `language_servers` map key), e.g. `tsgo`.
    server: String,
    /// How this connection is rooted.
    root: ConnectionRoot,
}

impl ConnectionKey {
    /// Build a key for `server` rooted at `root` (the resolved marker root URI
    /// string, or `None` for the client-root fallback).
    pub(crate) fn new(server: impl Into<String>, root: Option<String>) -> Self {
        Self {
            server: server.into(),
            root: match root {
                Some(root) => ConnectionRoot::Marker(root),
                None => ConnectionRoot::ClientFallback,
            },
        }
    }

    /// Build the shared-instance key for `server` (#391): one connection that
    /// serves every workspace root via `workspace/didChangeWorkspaceFolders`.
    pub(crate) fn shared(server: impl Into<String>) -> Self {
        Self {
            server: server.into(),
            root: ConnectionRoot::Shared,
        }
    }

    /// Whether this is a shared-instance key (#391).
    pub(crate) fn is_shared(&self) -> bool {
        matches!(self.root, ConnectionRoot::Shared)
    }

    /// Build a key with no resolved root (client-root fallback).
    ///
    /// Test-only convenience: production builds keys via [`new`](Self::new) with
    /// the resolved marker root. Tests use this where the root is irrelevant.
    #[cfg(test)]
    pub(crate) fn for_server(server: impl Into<String>) -> Self {
        Self::new(server, None)
    }
}

impl fmt::Display for ConnectionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.root {
            ConnectionRoot::Marker(root) => write!(f, "{}@{}", self.server, root),
            ConnectionRoot::ClientFallback => write!(f, "{}", self.server),
            ConnectionRoot::Shared => write!(f, "{}#shared", self.server),
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

    #[test]
    fn shared_key_is_distinct_from_marker_and_fallback() {
        let shared = ConnectionKey::shared("tsgo");
        assert!(shared.is_shared());
        // The shared key must not collide with the client-root fallback for the
        // same server, nor with any marker root — else a marker-less document
        // (or a per-root fallback after a capability miss) would be routed to
        // the shared instance by accident.
        assert_ne!(shared, ConnectionKey::for_server("tsgo"));
        assert_ne!(
            shared,
            ConnectionKey::new("tsgo", Some("file:///repo".to_string()))
        );
        assert_eq!(shared, ConnectionKey::shared("tsgo"));
        assert_ne!(shared, ConnectionKey::shared("pyright"));
        assert!(!ConnectionKey::for_server("tsgo").is_shared());
    }

    #[test]
    fn shared_key_display_is_unambiguous() {
        assert_eq!(ConnectionKey::shared("tsgo").to_string(), "tsgo#shared");
    }
}
