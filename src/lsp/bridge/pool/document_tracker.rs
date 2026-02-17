//! Document tracking for downstream language servers.
//!
//! This module provides the DocumentTracker which manages virtual document state
//! for downstream language servers. It tracks:
//! - Document versions (for didChange notifications)
//! - Host-to-virtual mappings (for didClose propagation)
//! - Opened state (for LSP spec compliance - ADR-0015)

use std::collections::HashMap;

use dashmap::DashMap;
use tokio::sync::Mutex;

use url::Url;

use crate::lsp::bridge::protocol::VirtualDocumentUri;

/// Represents an opened virtual document for tracking.
///
/// Used for didClose propagation when host document closes.
/// Each OpenedVirtualDoc represents a virtual document that was opened
/// via didOpen on a downstream language server.
#[derive(Debug, Clone)]
pub(crate) struct OpenedVirtualDoc {
    /// The virtual document URI (contains language and region_id)
    pub(crate) virtual_uri: VirtualDocumentUri,
    /// The server name this document was opened on.
    ///
    /// Used for reverse lookup when routing didClose notifications.
    /// Multiple languages may map to the same server (e.g., ts and tsx -> tsgo).
    pub(crate) server_name: String,
}

/// Tracks virtual document state for downstream language servers.
///
/// Manages three related concerns:
/// - Document versions (for didChange notifications)
/// - Host-to-virtual mappings (for didClose propagation)
/// - Opened state (for LSP spec compliance - ADR-0015)
///
/// # Server-Name-Based Keying
///
/// Document versions are keyed by `server_name`, not by language. This enables
/// process sharing for related languages (e.g., ts and tsx sharing one tsgo server).
/// VirtualDocumentUri still uses `injection_language` for URI construction (file extension).
///
/// # Lock Strategy
///
/// Each Mutex (`document_versions`, `host_to_virtual`) is acquired and released
/// independently — never held simultaneously. The DashMap `opened_documents`
/// provides lock-free concurrent reads with internal sharded locking.
pub(crate) struct DocumentTracker {
    /// Map of server_name -> (virtual document URI -> version).
    ///
    /// Keyed by server_name (not language) to enable process sharing.
    /// Multiple languages may map to the same server entry.
    document_versions: Mutex<HashMap<String, HashMap<String, i32>>>,
    /// Tracks which virtual documents were opened for each host document.
    ///
    /// Each OpenedVirtualDoc stores its server_name for reverse lookup during didClose.
    host_to_virtual: Mutex<HashMap<Url, Vec<OpenedVirtualDoc>>>,
    /// Tracks documents that have had didOpen ACTUALLY sent to downstream.
    /// Reference-counted: multiple servers may open the same virtual URI.
    /// Uses DashMap for lock-free concurrent reads with internal sharded locking (ADR-0015).
    opened_documents: DashMap<String, usize>,
    /// Reverse index: virtual URI string → server names that have this doc open.
    ///
    /// Enables O(1) lookup in `get_all_servers_for_virtual_uri()`, replacing the
    /// previous O(N×M) scan over `host_to_virtual`.
    virtual_to_servers: DashMap<String, Vec<String>>,
}

impl DocumentTracker {
    /// Create a new DocumentTracker with empty state.
    ///
    /// All tracking maps start empty. Documents are registered via
    /// `register_opened_document()` after a successful didOpen send.
    pub(crate) fn new() -> Self {
        Self {
            document_versions: Mutex::new(HashMap::new()),
            host_to_virtual: Mutex::new(HashMap::new()),
            opened_documents: DashMap::new(),
            virtual_to_servers: DashMap::new(),
        }
    }

    /// Check if a virtual document is claimed or opened on a downstream server.
    ///
    /// Fast, synchronous check used by request handlers and didChange
    /// forwarding to gate operations on documents not yet known downstream.
    pub(crate) fn is_document_opened(&self, virtual_uri: &VirtualDocumentUri) -> bool {
        self.opened_documents
            .get(&virtual_uri.to_uri_string())
            .is_some_and(|count| *count > 0)
    }

    /// Atomically claim a virtual document URI for opening.
    ///
    /// Returns `true` if this caller won the claim (URI was newly inserted).
    /// Returns `false` if another caller already claimed it.
    ///
    /// Initializes the document version BEFORE marking the document as opened
    /// in opened_documents. This ensures `increment_document_version` never returns
    /// `None` for a document where `is_document_opened()` returns `true`.
    ///
    /// On send failure, call `unclaim_document()` to roll back.
    pub(super) async fn try_claim_for_open(
        &self,
        virtual_uri: &VirtualDocumentUri,
        server_name: &str,
    ) -> bool {
        let uri_string = virtual_uri.to_uri_string();

        // Step 1: Check-and-initialize version under Mutex (serializes concurrent claims)
        {
            let mut versions = self.document_versions.lock().await;
            let docs = versions.entry(server_name.to_string()).or_default();
            if docs.contains_key(&uri_string) {
                return false; // Already claimed by another caller
            }
            docs.insert(uri_string.clone(), 1);
        }

        // Step 2: Mark as opened — version is already available for concurrent didChange
        *self.opened_documents.entry(uri_string.clone()).or_insert(0) += 1;

        // Step 3: Update reverse index so get_all_servers_for_virtual_uri works immediately.
        // This closes the TOCTOU gap between claim and register_opened_document.
        // register_opened_document will perform an idempotent duplicate-check insert.
        {
            let mut servers = self.virtual_to_servers.entry(uri_string).or_default();
            if !servers.contains(&server_name.to_string()) {
                servers.push(server_name.to_string());
            }
        }

        true
    }

    /// Roll back a claim made by `try_claim_for_open()`.
    ///
    /// Called when the didOpen send fails, so the document can be
    /// claimed again on a future attempt. Removes both the version
    /// entry and the opened_documents entry initialized by `try_claim_for_open()`.
    pub(super) async fn unclaim_document(
        &self,
        virtual_uri: &VirtualDocumentUri,
        server_name: &str,
    ) {
        let uri_string = virtual_uri.to_uri_string();

        // Remove version first (mirrors claim order)
        {
            let mut versions = self.document_versions.lock().await;
            if let Some(docs) = versions.get_mut(server_name) {
                docs.remove(&uri_string);
            }
        }

        // Then decrement the opened refcount and clean reverse index
        self.decrement_opened(&uri_string);
        self.remove_from_reverse_index(&uri_string, server_name);
    }

    /// Register a document's host_to_virtual mapping.
    ///
    /// Called BEFORE the didOpen send in `ensure_document_opened`, so that
    /// `close_host_document` can find the document even if the task is
    /// aborted after registration. On send failure, the caller rolls back
    /// via `unregister_virtual_doc()`.
    ///
    /// Records tracking state:
    /// - Document version (safety net via `or_insert` — primary initialization
    ///   happens in `try_claim_for_open()` to close the race window)
    /// - Host-to-virtual mapping (with dedup check for idempotency)
    /// - Opened state (reference count increment)
    ///
    /// Note: Both `opened_documents` `or_insert(1)` and version `or_insert(1)` are
    /// safety nets. `try_claim_for_open()` already performs both operations.
    /// They are kept here for test helpers that call `register_opened_document`
    /// directly without going through the claim path.
    pub(super) async fn register_opened_document(
        &self,
        host_uri: &Url,
        virtual_uri: &VirtualDocumentUri,
        server_name: &str,
    ) {
        let uri_string = virtual_uri.to_uri_string();

        // Step 1: Update versions (release lock after block)
        {
            let mut versions = self.document_versions.lock().await;
            versions
                .entry(server_name.to_string())
                .or_default()
                .entry(uri_string.clone())
                .or_insert(1);
        }

        // Step 2: Update host_to_virtual (separate lock scope)
        {
            let mut host_map = self.host_to_virtual.lock().await;
            let docs = host_map.entry(host_uri.clone()).or_default();
            if !docs.iter().any(|d| {
                d.virtual_uri.to_uri_string() == uri_string && d.server_name == server_name
            }) {
                docs.push(OpenedVirtualDoc {
                    virtual_uri: virtual_uri.clone(),
                    server_name: server_name.to_string(),
                });
            }
        }

        // Safety-net insert (already incremented by try_claim_for_open in production;
        // needed for test helpers that call this directly)
        self.opened_documents.entry(uri_string.clone()).or_insert(1);

        // Update the reverse index for O(1) get_all_servers_for_virtual_uri lookups
        {
            let mut servers = self.virtual_to_servers.entry(uri_string).or_default();
            if !servers.contains(&server_name.to_string()) {
                servers.push(server_name.to_string());
            }
        }
    }

    /// Increment the version of a virtual document and return the new version.
    ///
    /// Returns None if the document has not been opened.
    ///
    /// # Arguments
    ///
    /// * `virtual_uri` - The virtual document URI
    /// * `server_name` - The server name for HashMap lookup
    pub(super) async fn increment_document_version(
        &self,
        virtual_uri: &VirtualDocumentUri,
        server_name: &str,
    ) -> Option<i32> {
        let uri_string = virtual_uri.to_uri_string();

        let mut versions = self.document_versions.lock().await;
        if let Some(docs) = versions.get_mut(server_name)
            && let Some(version) = docs.get_mut(&uri_string)
        {
            *version += 1;
            return Some(*version);
        }
        None
    }

    /// Remove a document from all tracking state.
    ///
    /// Removes the document from:
    /// - `document_versions` (version tracking for didChange)
    /// - `opened_documents` (opened state for LSP compliance)
    ///
    /// Note: Does NOT remove from `host_to_virtual`. That cleanup is handled
    /// separately by `remove_host_virtual_docs()` or `remove_matching_virtual_docs()`,
    /// which are called before this method in the close flow.
    ///
    /// # Arguments
    ///
    /// * `virtual_uri` - The virtual document URI
    /// * `server_name` - The server name for HashMap lookup
    pub(crate) async fn untrack_document(
        &self,
        virtual_uri: &VirtualDocumentUri,
        server_name: &str,
    ) {
        let uri_string = virtual_uri.to_uri_string();

        let mut versions = self.document_versions.lock().await;
        if let Some(docs) = versions.get_mut(server_name) {
            docs.remove(&uri_string);
        }

        self.decrement_opened(&uri_string);
        self.remove_from_reverse_index(&uri_string, server_name);
    }

    /// Remove a single virtual document from host_to_virtual tracking.
    ///
    /// Used to roll back registration when didOpen send fails after
    /// register-before-send. Only removes from `host_to_virtual`; the
    /// caller must also call `unclaim_document()` to roll back the claim.
    pub(super) async fn unregister_virtual_doc(
        &self,
        host_uri: &Url,
        virtual_uri: &VirtualDocumentUri,
    ) {
        let uri_string = virtual_uri.to_uri_string();
        let mut host_map = self.host_to_virtual.lock().await;
        if let Some(docs) = host_map.get_mut(host_uri) {
            // Collect server names being removed for reverse index cleanup
            let removed_servers: Vec<String> = docs
                .iter()
                .filter(|d| d.virtual_uri.to_uri_string() == uri_string)
                .map(|d| d.server_name.clone())
                .collect();
            docs.retain(|d| d.virtual_uri.to_uri_string() != uri_string);
            drop(host_map);
            for server_name in &removed_servers {
                self.remove_from_reverse_index(&uri_string, server_name);
            }
        }
    }

    /// Remove and return all virtual documents for a host URI.
    ///
    /// Used by did_close module for cleanup.
    pub(super) async fn remove_host_virtual_docs(&self, host_uri: &Url) -> Vec<OpenedVirtualDoc> {
        let mut host_map = self.host_to_virtual.lock().await;
        let docs = host_map.remove(host_uri).unwrap_or_default();
        drop(host_map);
        for doc in &docs {
            self.remove_from_reverse_index(&doc.virtual_uri.to_uri_string(), &doc.server_name);
        }
        docs
    }

    /// Take virtual documents matching the given ULIDs, removing them from tracking.
    ///
    /// This is atomic: lookup and removal happen in a single lock acquisition,
    /// preventing race conditions with concurrent didOpen requests.
    ///
    /// Returns the removed documents (for sending didClose). Documents that
    /// were never opened (not in host_to_virtual) are not returned.
    ///
    /// # Arguments
    /// * `host_uri` - The host document URI
    /// * `invalidated_ulids` - ULIDs to match against virtual document URIs
    pub(crate) async fn remove_matching_virtual_docs(
        &self,
        host_uri: &Url,
        invalidated_ulids: &[ulid::Ulid],
    ) -> Vec<OpenedVirtualDoc> {
        if invalidated_ulids.is_empty() {
            return Vec::new();
        }

        // Convert ULIDs to strings for matching
        let ulid_strs: std::collections::HashSet<String> =
            invalidated_ulids.iter().map(|u| u.to_string()).collect();

        let mut host_map = self.host_to_virtual.lock().await;
        let Some(docs) = host_map.get_mut(host_uri) else {
            return Vec::new();
        };

        // Partition: matching docs to return, non-matching to keep
        let mut to_close = Vec::new();
        docs.retain(|doc| {
            // Match region_id directly from VirtualDocumentUri
            let should_close = ulid_strs.contains(doc.virtual_uri.region_id());
            if should_close {
                to_close.push(doc.clone());
                false // Remove from host_to_virtual
            } else {
                true // Keep in host_to_virtual
            }
        });
        drop(host_map);

        // Clean reverse index for each closed doc
        for doc in &to_close {
            self.remove_from_reverse_index(&doc.virtual_uri.to_uri_string(), &doc.server_name);
        }

        to_close
    }

    /// Decrement the reference count for an opened document URI.
    ///
    /// Removes the entry entirely when the count reaches zero.
    fn decrement_opened(&self, uri_string: &str) {
        self.opened_documents.remove_if_mut(uri_string, |_, count| {
            debug_assert!(*count > 0, "double-decrement on opened_documents");
            *count = count.saturating_sub(1);
            *count == 0
        });
    }

    /// Remove a server from the reverse index for a given virtual URI.
    ///
    /// Removes the entry entirely when no servers remain.
    fn remove_from_reverse_index(&self, uri_string: &str, server_name: &str) {
        self.virtual_to_servers
            .remove_if_mut(uri_string, |_, servers| {
                servers.retain(|s| s != server_name);
                servers.is_empty()
            });
    }

    /// Find ALL server names that have opened a given virtual document URI.
    ///
    /// When multiple servers handle the same language (e.g., emmylua and lua_ls
    /// both handling Lua), each server opens its own copy of the virtual document.
    /// This method collects ALL matching server names so that didChange can be
    /// forwarded to every server, not just the first one found.
    ///
    /// O(1) lookup via the `virtual_to_servers` reverse index.
    pub(super) fn get_all_servers_for_virtual_uri(
        &self,
        virtual_uri: &VirtualDocumentUri,
    ) -> Vec<String> {
        self.virtual_to_servers
            .get(&virtual_uri.to_uri_string())
            .map(|entry| entry.value().clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::pool::test_helpers::*;

    // ========================================
    // OpenedVirtualDoc tests
    // ========================================

    /// Test that OpenedVirtualDoc struct has required fields.
    ///
    /// The struct should have:
    /// - virtual_uri: VirtualDocumentUri (typed URI with language and region_id)
    /// - server_name: String (for reverse lookup during didClose)
    #[tokio::test]
    async fn opened_virtual_doc_struct_has_required_fields() {
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "region-0");
        let doc = OpenedVirtualDoc {
            virtual_uri: virtual_uri.clone(),
            server_name: "lua".to_string(),
        };

        assert_eq!(doc.virtual_uri.language(), "lua");
        assert_eq!(doc.virtual_uri.region_id(), "region-0");
        assert_eq!(doc.server_name, "lua");
    }

    // ========================================
    // register_opened_document tests
    // ========================================

    /// Test that register_opened_document records host to virtual mapping.
    #[tokio::test]
    async fn register_opened_document_records_host_to_virtual_mapping() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "lua-0");

        tracker
            .register_opened_document(&host_uri, &virtual_uri, "lua")
            .await;

        // Verify the host_to_virtual mapping was recorded
        let host_map = tracker.host_to_virtual.lock().await;
        let virtual_docs = host_map
            .get(&host_uri)
            .expect("host_uri should have entry in host_to_virtual");
        assert_eq!(virtual_docs.len(), 1);
        assert_eq!(virtual_docs[0].virtual_uri.language(), "lua");
        assert_eq!(virtual_docs[0].virtual_uri.region_id(), "lua-0");
        assert_eq!(virtual_docs[0].server_name, "lua");
    }

    /// Test that register_opened_document records multiple virtual docs for same host.
    #[tokio::test]
    async fn register_opened_document_records_multiple_virtual_docs() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///project/doc.md").unwrap();

        let virtual_uri_0 = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "lua-0");
        tracker
            .register_opened_document(&host_uri, &virtual_uri_0, "lua")
            .await;

        let virtual_uri_1 = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "lua-1");
        tracker
            .register_opened_document(&host_uri, &virtual_uri_1, "lua")
            .await;

        let host_map = tracker.host_to_virtual.lock().await;
        let virtual_docs = host_map
            .get(&host_uri)
            .expect("host_uri should have entry in host_to_virtual");
        assert_eq!(virtual_docs.len(), 2);
        assert_eq!(virtual_docs[0].virtual_uri.region_id(), "lua-0");
        assert_eq!(virtual_docs[1].virtual_uri.region_id(), "lua-1");
    }

    /// Test that register_opened_document is idempotent.
    ///
    /// Calling it twice for the same document should not create duplicate
    /// entries in host_to_virtual or reset the version counter.
    #[tokio::test]
    async fn register_opened_document_is_idempotent() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "lua-0");

        // Register twice
        tracker
            .register_opened_document(&host_uri, &virtual_uri, "lua")
            .await;
        tracker
            .register_opened_document(&host_uri, &virtual_uri, "lua")
            .await;

        // Verify only one entry exists (no duplicate)
        let host_map = tracker.host_to_virtual.lock().await;
        let virtual_docs = host_map
            .get(&host_uri)
            .expect("host_uri should have entry in host_to_virtual");
        assert_eq!(
            virtual_docs.len(),
            1,
            "Should only have one entry, not duplicates"
        );
    }

    /// Test that register_opened_document marks the document as opened.
    #[tokio::test]
    async fn register_opened_document_marks_as_opened() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        assert!(
            !tracker.is_document_opened(&virtual_uri),
            "Should not be opened before registration"
        );

        tracker
            .register_opened_document(&host_uri, &virtual_uri, "lua")
            .await;

        assert!(
            tracker.is_document_opened(&virtual_uri),
            "Should be opened after registration"
        );
    }

    // ========================================
    // is_document_opened tests
    // ========================================

    /// Test that is_document_opened returns false before registration.
    #[test]
    fn is_document_opened_returns_false_before_registered() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        assert!(
            !tracker.is_document_opened(&virtual_uri),
            "is_document_opened should return false before registration"
        );
    }

    // ========================================
    // try_claim_for_open / unclaim_document tests
    // ========================================

    /// Test that try_claim_for_open returns true for a new document.
    #[tokio::test]
    async fn try_claim_for_open_returns_true_for_new_document() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        assert!(
            tracker.try_claim_for_open(&virtual_uri, "lua").await,
            "First claim should succeed"
        );
    }

    /// Test that try_claim_for_open returns false for an already claimed document.
    #[tokio::test]
    async fn try_claim_for_open_returns_false_for_already_claimed() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // First claim succeeds
        assert!(tracker.try_claim_for_open(&virtual_uri, "lua").await);

        // Second claim for same URI fails
        assert!(
            !tracker.try_claim_for_open(&virtual_uri, "lua").await,
            "Second claim should fail — already claimed"
        );
    }

    /// Test that unclaim_document allows reclaim.
    #[tokio::test]
    async fn unclaim_document_allows_reclaim() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Claim, unclaim, then claim again
        assert!(tracker.try_claim_for_open(&virtual_uri, "lua").await);
        tracker.unclaim_document(&virtual_uri, "lua").await;
        assert!(
            tracker.try_claim_for_open(&virtual_uri, "lua").await,
            "Should be able to reclaim after unclaim"
        );
    }

    /// Test that get_all_servers_for_virtual_uri works immediately after try_claim_for_open.
    ///
    /// This verifies the reverse index is updated at claim time, closing the
    /// TOCTOU gap between try_claim_for_open and register_opened_document.
    #[tokio::test]
    async fn reverse_index_available_immediately_after_claim() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Before claim: no servers
        assert!(
            tracker
                .get_all_servers_for_virtual_uri(&virtual_uri)
                .is_empty(),
            "Should return empty before claim"
        );

        // Claim the document (should update reverse index)
        tracker.try_claim_for_open(&virtual_uri, "lua").await;

        // Reverse index should be available immediately — no need for register_opened_document
        let servers = tracker.get_all_servers_for_virtual_uri(&virtual_uri);
        assert_eq!(
            servers,
            vec!["lua".to_string()],
            "Reverse index should be available immediately after claim"
        );
    }

    /// Test that version is available immediately after try_claim_for_open.
    ///
    /// This prevents the race condition where a concurrent didChange arrives
    /// between claim and register_opened_document — the version must be
    /// initialized at claim time so increment_document_version returns Some.
    #[tokio::test]
    async fn version_available_immediately_after_claim() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Claim the document (should initialize version to 1)
        tracker.try_claim_for_open(&virtual_uri, "lua").await;

        // Version should be available immediately — no need for register_opened_document
        let version = tracker
            .increment_document_version(&virtual_uri, "lua")
            .await;
        assert_eq!(
            version,
            Some(2),
            "Version should be available immediately after claim (1 → 2)"
        );
    }

    /// Test that unclaim_document removes the version entry.
    ///
    /// When a didOpen send fails and we unclaim, the version entry must also
    /// be cleaned up so that increment_document_version returns None.
    #[tokio::test]
    async fn unclaim_removes_version_entry() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Claim (initializes version) then unclaim (should remove version)
        tracker.try_claim_for_open(&virtual_uri, "lua").await;
        tracker.unclaim_document(&virtual_uri, "lua").await;

        // Version should no longer exist
        let version = tracker
            .increment_document_version(&virtual_uri, "lua")
            .await;
        assert!(version.is_none(), "Version should be removed after unclaim");
    }

    // ========================================
    // increment_document_version tests
    // ========================================

    /// Test that increment_document_version returns None for unopened document.
    #[tokio::test]
    async fn increment_document_version_returns_none_for_unopened() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Document was never registered via register_opened_document
        let version = tracker
            .increment_document_version(&virtual_uri, "lua")
            .await;
        assert!(
            version.is_none(),
            "increment_document_version should return None for unopened document"
        );
    }

    /// Test that increment_document_version increments and returns new version.
    #[tokio::test]
    async fn increment_document_version_increments_after_open() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Open the document (sets version to 1)
        tracker
            .register_opened_document(&host_uri, &virtual_uri, "lua")
            .await;

        // First increment: 1 -> 2
        let version = tracker
            .increment_document_version(&virtual_uri, "lua")
            .await;
        assert_eq!(version, Some(2), "First increment should return 2");

        // Second increment: 2 -> 3
        let version = tracker
            .increment_document_version(&virtual_uri, "lua")
            .await;
        assert_eq!(version, Some(3), "Second increment should return 3");
    }

    // ========================================
    // untrack_document tests
    // ========================================

    /// Test that untrack_document removes from document_versions.
    #[tokio::test]
    async fn untrack_document_removes_from_versions() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Open the document
        tracker
            .register_opened_document(&host_uri, &virtual_uri, "lua")
            .await;

        // Verify version exists
        let version = tracker
            .increment_document_version(&virtual_uri, "lua")
            .await;
        assert!(
            version.is_some(),
            "Document should have version before untrack"
        );

        // Untrack the document
        tracker.untrack_document(&virtual_uri, "lua").await;

        // Version should no longer exist
        let version = tracker
            .increment_document_version(&virtual_uri, "lua")
            .await;
        assert!(
            version.is_none(),
            "Document should not have version after untrack"
        );
    }

    /// Test that untrack_document removes from opened_documents.
    #[tokio::test]
    async fn untrack_document_removes_from_opened() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Register as opened (sets version + host mapping + opened state)
        tracker
            .register_opened_document(&host_uri, &virtual_uri, "lua")
            .await;
        assert!(
            tracker.is_document_opened(&virtual_uri),
            "Document should be opened before untrack"
        );

        // Untrack the document
        tracker.untrack_document(&virtual_uri, "lua").await;

        // Should no longer be marked as opened
        assert!(
            !tracker.is_document_opened(&virtual_uri),
            "Document should not be opened after untrack"
        );
    }

    /// Test that untrack_document does NOT remove from host_to_virtual.
    ///
    /// The host_to_virtual cleanup is handled separately by remove_host_virtual_docs
    /// or remove_matching_virtual_docs, which are called before untrack_document.
    #[tokio::test]
    async fn untrack_document_does_not_remove_from_host_to_virtual() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Open the document (adds to host_to_virtual)
        tracker
            .register_opened_document(&host_uri, &virtual_uri, "lua")
            .await;

        // Untrack the document
        tracker.untrack_document(&virtual_uri, "lua").await;

        // host_to_virtual should still have the entry
        let host_map = tracker.host_to_virtual.lock().await;
        let docs = host_map.get(&host_uri);
        assert!(
            docs.is_some() && !docs.unwrap().is_empty(),
            "untrack_document should NOT remove from host_to_virtual"
        );
    }

    // ========================================
    // remove_matching_virtual_docs tests
    // ========================================

    #[tokio::test]
    async fn remove_matching_virtual_docs_removes_matching_docs() {
        let tracker = DocumentTracker::new();
        let host_uri = test_host_uri("phase3_take");

        // Register some virtual docs
        let virtual_uri_1 = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        let virtual_uri_2 =
            VirtualDocumentUri::new(&url_to_uri(&host_uri), "python", TEST_ULID_PYTHON_0);

        tracker
            .register_opened_document(&host_uri, &virtual_uri_1, "lua")
            .await;
        tracker
            .register_opened_document(&host_uri, &virtual_uri_2, "python")
            .await;

        // Parse the ULIDs for matching
        let ulid_lua: ulid::Ulid = TEST_ULID_LUA_0.parse().unwrap();

        // Take only the Lua ULID
        let taken = tracker
            .remove_matching_virtual_docs(&host_uri, &[ulid_lua])
            .await;

        // Should return the Lua doc
        assert_eq!(taken.len(), 1, "Should take exactly one doc");
        assert_eq!(
            taken[0].virtual_uri.language(),
            "lua",
            "Should be the Lua doc"
        );
        assert_eq!(
            taken[0].virtual_uri.region_id(),
            TEST_ULID_LUA_0,
            "Should have the Lua ULID"
        );

        // Verify remaining docs in host_to_virtual
        let host_map = tracker.host_to_virtual.lock().await;
        let remaining = host_map.get(&host_uri).unwrap();
        assert_eq!(remaining.len(), 1, "Should have one remaining doc");
        assert_eq!(
            remaining[0].virtual_uri.language(),
            "python",
            "Python doc should remain"
        );
    }

    #[tokio::test]
    async fn remove_matching_virtual_docs_returns_empty_for_no_match() {
        let tracker = DocumentTracker::new();
        let host_uri = test_host_uri("phase3_no_match");

        // Register a virtual doc
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        tracker
            .register_opened_document(&host_uri, &virtual_uri, "lua")
            .await;

        // Try to take a different ULID
        let other_ulid: ulid::Ulid = TEST_ULID_LUA_1.parse().unwrap();
        let taken = tracker
            .remove_matching_virtual_docs(&host_uri, &[other_ulid])
            .await;

        assert!(taken.is_empty(), "Should return empty when no ULIDs match");

        // Original doc should still be there
        let host_map = tracker.host_to_virtual.lock().await;
        let remaining = host_map.get(&host_uri).unwrap();
        assert_eq!(remaining.len(), 1, "Original doc should remain");
    }

    #[tokio::test]
    async fn remove_matching_virtual_docs_returns_empty_for_unknown_host() {
        let tracker = DocumentTracker::new();
        let host_uri = test_host_uri("phase3_unknown_host");

        let ulid: ulid::Ulid = TEST_ULID_LUA_0.parse().unwrap();
        let taken = tracker
            .remove_matching_virtual_docs(&host_uri, &[ulid])
            .await;

        assert!(taken.is_empty(), "Should return empty for unknown host URI");
    }

    #[tokio::test]
    async fn remove_matching_virtual_docs_returns_empty_for_empty_ulids() {
        let tracker = DocumentTracker::new();
        let host_uri = test_host_uri("phase3_empty_ulids");

        // Register a virtual doc
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        tracker
            .register_opened_document(&host_uri, &virtual_uri, "lua")
            .await;

        // Take with empty ULID list (fast path)
        let taken = tracker.remove_matching_virtual_docs(&host_uri, &[]).await;

        assert!(taken.is_empty(), "Should return empty for empty ULID list");

        // Original doc should still be there
        let host_map = tracker.host_to_virtual.lock().await;
        let remaining = host_map.get(&host_uri).unwrap();
        assert_eq!(remaining.len(), 1, "Original doc should remain");
    }

    #[tokio::test]
    async fn remove_matching_virtual_docs_takes_multiple_docs() {
        let tracker = DocumentTracker::new();
        let host_uri = test_host_uri("phase3_multiple");

        // Register multiple virtual docs using VirtualDocumentUri for proper type safety
        let virtual_uri_1 = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        let virtual_uri_2 = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_1);
        let virtual_uri_3 =
            VirtualDocumentUri::new(&url_to_uri(&host_uri), "python", TEST_ULID_PYTHON_0);

        tracker
            .register_opened_document(&host_uri, &virtual_uri_1, "lua")
            .await;
        tracker
            .register_opened_document(&host_uri, &virtual_uri_2, "lua")
            .await;
        tracker
            .register_opened_document(&host_uri, &virtual_uri_3, "python")
            .await;

        // Take both Lua ULIDs
        let ulid_1: ulid::Ulid = TEST_ULID_LUA_0.parse().unwrap();
        let ulid_2: ulid::Ulid = TEST_ULID_LUA_1.parse().unwrap();

        let taken = tracker
            .remove_matching_virtual_docs(&host_uri, &[ulid_1, ulid_2])
            .await;

        assert_eq!(taken.len(), 2, "Should take both Lua docs");

        // Verify Python doc remains
        let host_map = tracker.host_to_virtual.lock().await;
        let remaining = host_map.get(&host_uri).unwrap();
        assert_eq!(remaining.len(), 1, "Python doc should remain");
        assert_eq!(
            remaining[0].virtual_uri.language(),
            "python",
            "Remaining doc should be Python"
        );
    }

    // ========================================
    // unregister_virtual_doc tests
    // ========================================

    /// Test that unregister_virtual_doc removes the entry from host_to_virtual.
    ///
    /// This is the rollback path when register-before-send is used and
    /// the didOpen send fails. The host_to_virtual entry must be cleaned up.
    #[tokio::test]
    async fn unregister_virtual_doc_removes_entry() {
        let tracker = DocumentTracker::new();
        let host_uri = test_host_uri("unregister");
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Register then unregister
        tracker
            .register_opened_document(&host_uri, &virtual_uri, "lua")
            .await;
        tracker
            .unregister_virtual_doc(&host_uri, &virtual_uri)
            .await;

        // host_to_virtual should be empty for this host
        let host_map = tracker.host_to_virtual.lock().await;
        let docs = host_map.get(&host_uri);
        assert!(
            docs.is_none() || docs.unwrap().is_empty(),
            "unregister_virtual_doc should remove the entry from host_to_virtual"
        );
    }

    /// Test that unregister_virtual_doc only removes the targeted entry.
    ///
    /// Other virtual documents for the same host should remain.
    #[tokio::test]
    async fn unregister_virtual_doc_preserves_other_entries() {
        let tracker = DocumentTracker::new();
        let host_uri = test_host_uri("unregister_partial");
        let vuri_0 = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        let vuri_1 = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_1);

        // Register two documents
        tracker
            .register_opened_document(&host_uri, &vuri_0, "lua")
            .await;
        tracker
            .register_opened_document(&host_uri, &vuri_1, "lua")
            .await;

        // Unregister only the first
        tracker.unregister_virtual_doc(&host_uri, &vuri_0).await;

        // Second should remain
        let host_map = tracker.host_to_virtual.lock().await;
        let docs = host_map.get(&host_uri).unwrap();
        assert_eq!(docs.len(), 1, "Should have exactly one remaining entry");
        assert_eq!(
            docs[0].virtual_uri.region_id(),
            TEST_ULID_LUA_1,
            "The remaining entry should be the second document"
        );
    }

    /// Test that unregister_virtual_doc is a no-op for unknown host.
    #[tokio::test]
    async fn unregister_virtual_doc_noop_for_unknown_host() {
        let tracker = DocumentTracker::new();
        let host_uri = test_host_uri("unregister_unknown");
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Should not panic
        tracker
            .unregister_virtual_doc(&host_uri, &virtual_uri)
            .await;
    }

    // ========================================
    // get_all_servers_for_virtual_uri tests
    // ========================================

    /// Test that get_all_servers_for_virtual_uri returns multiple servers for the same URI.
    ///
    /// When two servers (e.g., emmylua and lua_ls) both open the same virtual doc,
    /// both server names should be returned.
    #[tokio::test]
    async fn get_all_servers_returns_multiple_servers_for_same_uri() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Open the same virtual doc on two different servers
        tracker
            .register_opened_document(&host_uri, &virtual_uri, "emmylua")
            .await;
        // Second server gets a different key in document_versions but same virtual_uri.
        // Two different servers opening the same URI is achieved by calling
        // register_opened_document with different server_names.
        tracker
            .register_opened_document(&host_uri, &virtual_uri, "lua_ls")
            .await;

        let servers = tracker.get_all_servers_for_virtual_uri(&virtual_uri);
        assert_eq!(servers.len(), 2, "Should return both servers");
        assert!(servers.contains(&"emmylua".to_string()));
        assert!(servers.contains(&"lua_ls".to_string()));
    }

    /// Test that get_all_servers_for_virtual_uri returns a single server when only one matches.
    #[tokio::test]
    async fn get_all_servers_returns_single_server() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        tracker
            .register_opened_document(&host_uri, &virtual_uri, "lua")
            .await;

        let servers = tracker.get_all_servers_for_virtual_uri(&virtual_uri);
        assert_eq!(servers, vec!["lua".to_string()]);
    }

    /// Test that get_all_servers_for_virtual_uri returns empty vec for unknown URI.
    #[tokio::test]
    async fn get_all_servers_returns_empty_for_unknown() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        let servers = tracker.get_all_servers_for_virtual_uri(&virtual_uri);
        assert!(servers.is_empty(), "Should return empty for unknown URI");
    }

    /// Test that get_all_servers_for_virtual_uri does not cross-contaminate.
    ///
    /// Different virtual URIs should not leak servers from unrelated documents.
    #[tokio::test]
    async fn get_all_servers_does_not_cross_contaminate() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();

        let virtual_uri_lua =
            VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        let virtual_uri_python =
            VirtualDocumentUri::new(&url_to_uri(&host_uri), "python", TEST_ULID_PYTHON_0);

        tracker
            .register_opened_document(&host_uri, &virtual_uri_lua, "lua")
            .await;
        tracker
            .register_opened_document(&host_uri, &virtual_uri_python, "python")
            .await;

        let lua_servers = tracker.get_all_servers_for_virtual_uri(&virtual_uri_lua);
        let python_servers = tracker.get_all_servers_for_virtual_uri(&virtual_uri_python);

        assert_eq!(lua_servers, vec!["lua".to_string()]);
        assert_eq!(python_servers, vec!["python".to_string()]);
    }

    /// Test that untracking one server preserves opened state for another server.
    ///
    /// When two servers (e.g., emmylua and lua_ls) both claim the same virtual URI,
    /// untracking one should NOT remove the document from opened_documents because
    /// the other server still has it open. This is the core W1 reference-counting fix.
    #[tokio::test]
    async fn untrack_one_server_preserves_opened_for_other_server() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Two servers claim the same virtual URI
        assert!(tracker.try_claim_for_open(&virtual_uri, "emmylua").await);
        assert!(tracker.try_claim_for_open(&virtual_uri, "lua_ls").await);
        assert!(
            tracker.is_document_opened(&virtual_uri),
            "Document should be opened after two claims"
        );

        // Untrack one server
        tracker.untrack_document(&virtual_uri, "emmylua").await;

        // Document should still be opened because lua_ls still has it
        assert!(
            tracker.is_document_opened(&virtual_uri),
            "Document should remain opened while another server still has it"
        );

        // Untrack the second server
        tracker.untrack_document(&virtual_uri, "lua_ls").await;

        // Now it should be gone
        assert!(
            !tracker.is_document_opened(&virtual_uri),
            "Document should not be opened after all servers untrack"
        );
    }

    /// Test that get_all_servers_for_virtual_uri works with process sharing.
    ///
    /// When ts and tsx both use "tsgo" as server_name, the lookup
    /// should return "tsgo" for each language's virtual URI independently.
    #[tokio::test]
    async fn get_all_servers_with_process_sharing() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();

        // Open a ts document with server_name "tsgo"
        let virtual_uri_ts =
            VirtualDocumentUri::new(&url_to_uri(&host_uri), "typescript", TEST_ULID_LUA_0);
        tracker
            .register_opened_document(&host_uri, &virtual_uri_ts, "tsgo")
            .await;

        // Open a tsx document with server_name "tsgo"
        let virtual_uri_tsx =
            VirtualDocumentUri::new(&url_to_uri(&host_uri), "typescriptreact", TEST_ULID_LUA_1);
        tracker
            .register_opened_document(&host_uri, &virtual_uri_tsx, "tsgo")
            .await;

        // Both should return vec!["tsgo"] — same server, different virtual URIs
        assert_eq!(
            tracker.get_all_servers_for_virtual_uri(&virtual_uri_ts),
            vec!["tsgo".to_string()]
        );
        assert_eq!(
            tracker.get_all_servers_for_virtual_uri(&virtual_uri_tsx),
            vec!["tsgo".to_string()]
        );
    }
}
