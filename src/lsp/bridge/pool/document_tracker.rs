//! Document tracking for downstream language servers.
//!
//! This module provides the DocumentTracker which manages virtual document state
//! for downstream language servers. It tracks:
//! - Document versions (for didChange notifications)
//! - Host-to-virtual mappings (for didClose propagation)
//! - Opened state (for LSP spec compliance - ADR-0015)

use std::collections::{HashMap, HashSet};

/// Decision result for document open handling.
///
/// This enum represents the two possible outcomes when determining
/// whether to send a didOpen notification for a virtual document.
///
/// The decision is based solely on `opened_documents` (a synchronous check).
/// No state is mutated during the decision — all mutations happen in
/// `register_opened_document()` after the send succeeds.
///
/// # State Machine
///
/// ```text
/// opened_documents contains URI? | Decision
/// -------------------------------|------------
/// No                             | SendDidOpen
/// Yes                            | AlreadyOpened
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DocumentOpenDecision {
    /// Document has not been opened yet - send didOpen notification.
    ///
    /// The caller should send didOpen and then call `register_opened_document()`
    /// to record the successful open.
    SendDidOpen,

    /// Document was already opened - skip didOpen (no-op).
    AlreadyOpened,
}

use log::warn;
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
/// # Lock Ordering Contract
///
/// When acquiring multiple locks, the order must be:
/// 1. `document_versions` first
/// 2. `host_to_virtual` second (while holding #1)
///
/// The `opened_documents` lock (std::sync::RwLock) can be acquired
/// independently of async locks for fast, synchronous read checks.
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
    /// Uses std::sync::RwLock for fast, synchronous read checks (ADR-0015).
    opened_documents: std::sync::RwLock<HashSet<String>>,
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
            opened_documents: std::sync::RwLock::new(HashSet::new()),
        }
    }

    /// Check if a document has had didOpen ACTUALLY sent to downstream (ADR-0015).
    ///
    /// This is a fast, synchronous check used by request handlers to ensure
    /// they don't send requests before didOpen has been sent.
    ///
    /// Returns true if `register_opened_document()` has been called for this document.
    /// Returns false if the document hasn't been opened yet.
    pub(crate) fn is_document_opened(&self, virtual_uri: &VirtualDocumentUri) -> bool {
        let uri_string = virtual_uri.to_uri_string();

        match self.opened_documents.read() {
            Ok(opened) => opened.contains(&uri_string),
            Err(poisoned) => {
                warn!(
                    target: "kakehashi::lock_recovery",
                    "Recovered from poisoned opened_documents lock in is_document_opened()"
                );
                poisoned.into_inner().contains(&uri_string)
            }
        }
    }

    /// Determine the action to take for document opening.
    ///
    /// This is a **pure, side-effect-free** check. No state is mutated.
    /// The caller should:
    /// 1. Check the decision
    /// 2. If `SendDidOpen`: send the notification, then call `register_opened_document()`
    /// 3. If `AlreadyOpened`: skip (no-op)
    ///
    /// Because no state is claimed/reserved, two concurrent calls for the same
    /// virtual document may both return `SendDidOpen`. This is acceptable — the
    /// downstream server handles duplicate didOpen gracefully, and
    /// `register_opened_document()` is idempotent.
    pub(super) fn document_open_decision(
        &self,
        virtual_uri: &VirtualDocumentUri,
    ) -> DocumentOpenDecision {
        if self.is_document_opened(virtual_uri) {
            DocumentOpenDecision::AlreadyOpened
        } else {
            DocumentOpenDecision::SendDidOpen
        }
    }

    /// Register a document as successfully opened.
    ///
    /// Called AFTER the didOpen notification has been successfully sent to the
    /// downstream server. Records all tracking state atomically:
    /// - Document version (set to 1, idempotent via `or_insert`)
    /// - Host-to-virtual mapping (with dedup check for idempotency)
    /// - Opened state (HashSet insert, naturally idempotent)
    ///
    /// # Idempotency
    ///
    /// This method is safe to call multiple times for the same document.
    /// This is important because concurrent requests may both get `SendDidOpen`
    /// from `document_open_decision()` and both call this method after sending.
    ///
    /// # Lock Ordering
    ///
    /// Acquires `document_versions` first, then `host_to_virtual`.
    /// This order must be consistent to prevent deadlocks.
    pub(super) async fn register_opened_document(
        &self,
        host_uri: &Url,
        virtual_uri: &VirtualDocumentUri,
        server_name: &str,
    ) {
        let uri_string = virtual_uri.to_uri_string();

        // Lock ordering: document_versions first, host_to_virtual second
        let mut versions = self.document_versions.lock().await;
        versions
            .entry(server_name.to_string())
            .or_default()
            .entry(uri_string.clone())
            .or_insert(1);

        let mut host_map = self.host_to_virtual.lock().await;
        let docs = host_map.entry(host_uri.clone()).or_default();
        if !docs
            .iter()
            .any(|d| d.virtual_uri.to_uri_string() == uri_string)
        {
            docs.push(OpenedVirtualDoc {
                virtual_uri: virtual_uri.clone(),
                server_name: server_name.to_string(),
            });
        }
        drop(host_map);
        drop(versions);

        // Mark as opened (uses std::sync::RwLock, independent of async locks)
        match self.opened_documents.write() {
            Ok(mut opened) => {
                opened.insert(virtual_uri.to_uri_string());
            }
            Err(poisoned) => {
                warn!(
                    target: "kakehashi::lock_recovery",
                    "Recovered from poisoned opened_documents lock in register_opened_document()"
                );
                poisoned.into_inner().insert(virtual_uri.to_uri_string());
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

        match self.opened_documents.write() {
            Ok(mut opened) => {
                opened.remove(&uri_string);
            }
            Err(poisoned) => {
                warn!(
                    target: "kakehashi::lock_recovery",
                    "Recovered from poisoned opened_documents lock in untrack_document()"
                );
                poisoned.into_inner().remove(&uri_string);
            }
        }
    }

    /// Remove and return all virtual documents for a host URI.
    ///
    /// Used by did_close module for cleanup.
    pub(super) async fn remove_host_virtual_docs(&self, host_uri: &Url) -> Vec<OpenedVirtualDoc> {
        let mut host_map = self.host_to_virtual.lock().await;
        host_map.remove(host_uri).unwrap_or_default()
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

        to_close
    }

    /// Find server_name for a virtual document URI (for didClose routing).
    ///
    /// Searches all host_to_virtual entries for a matching virtual URI.
    /// Returns the server_name stored in OpenedVirtualDoc.
    ///
    /// # Performance
    ///
    /// O(n) where n is total virtual documents. For typical document counts
    /// (<100), this is acceptable. Consider indexing if perf becomes an issue.
    ///
    /// # Arguments
    ///
    /// * `virtual_uri` - The virtual document URI to look up
    ///
    /// # Returns
    ///
    /// The server_name if found, None if the document is not tracked.
    pub(crate) async fn get_server_for_virtual_uri(
        &self,
        virtual_uri: &VirtualDocumentUri,
    ) -> Option<String> {
        let uri_string = virtual_uri.to_uri_string();
        let host_map = self.host_to_virtual.lock().await;

        for virtual_docs in host_map.values() {
            for doc in virtual_docs {
                if doc.virtual_uri.to_uri_string() == uri_string {
                    return Some(doc.server_name.clone());
                }
            }
        }
        None
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
    #[test]
    fn try_claim_for_open_returns_true_for_new_document() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        assert!(
            tracker.try_claim_for_open(&virtual_uri),
            "First claim should succeed"
        );
    }

    /// Test that try_claim_for_open returns false for an already claimed document.
    #[test]
    fn try_claim_for_open_returns_false_for_already_claimed() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // First claim succeeds
        assert!(tracker.try_claim_for_open(&virtual_uri));

        // Second claim for same URI fails
        assert!(
            !tracker.try_claim_for_open(&virtual_uri),
            "Second claim should fail — already claimed"
        );
    }

    /// Test that unclaim_document allows reclaim.
    #[test]
    fn unclaim_document_allows_reclaim() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Claim, unclaim, then claim again
        assert!(tracker.try_claim_for_open(&virtual_uri));
        tracker.unclaim_document(&virtual_uri);
        assert!(
            tracker.try_claim_for_open(&virtual_uri),
            "Should be able to reclaim after unclaim"
        );
    }

    // ========================================
    // DocumentOpenDecision unit tests
    // ========================================

    /// Test that document_open_decision returns SendDidOpen for new document.
    #[test]
    fn document_open_decision_returns_send_didopen_for_new_document() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        let decision = tracker.document_open_decision(&virtual_uri);

        assert_eq!(
            decision,
            DocumentOpenDecision::SendDidOpen,
            "New document should return SendDidOpen"
        );
    }

    /// Test that document_open_decision returns AlreadyOpened after registration.
    #[tokio::test]
    async fn document_open_decision_returns_already_opened_after_registration() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        tracker
            .register_opened_document(&host_uri, &virtual_uri, "lua")
            .await;

        let decision = tracker.document_open_decision(&virtual_uri);

        assert_eq!(
            decision,
            DocumentOpenDecision::AlreadyOpened,
            "Registered document should return AlreadyOpened"
        );
    }

    /// Test document_open_decision is side-effect-free.
    ///
    /// Calling it multiple times should always return SendDidOpen for
    /// a new document — no state is mutated by the decision itself.
    #[test]
    fn document_open_decision_is_side_effect_free() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Multiple calls should all return SendDidOpen (no state mutation)
        let decision1 = tracker.document_open_decision(&virtual_uri);
        let decision2 = tracker.document_open_decision(&virtual_uri);

        assert_eq!(decision1, DocumentOpenDecision::SendDidOpen);
        assert_eq!(decision2, DocumentOpenDecision::SendDidOpen);
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
    // get_server_for_virtual_uri tests
    // ========================================

    /// Test that get_server_for_virtual_uri returns the server_name.
    #[tokio::test]
    async fn get_server_for_virtual_uri_returns_server_name() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Open the document with server_name "lua"
        tracker
            .register_opened_document(&host_uri, &virtual_uri, "lua")
            .await;

        // Lookup should return the server_name
        let server_name = tracker.get_server_for_virtual_uri(&virtual_uri).await;
        assert_eq!(server_name, Some("lua".to_string()));
    }

    /// Test that get_server_for_virtual_uri returns None for unknown document.
    #[tokio::test]
    async fn get_server_for_virtual_uri_returns_none_for_unknown() {
        let tracker = DocumentTracker::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Without opening, lookup should return None
        let server_name = tracker.get_server_for_virtual_uri(&virtual_uri).await;
        assert_eq!(server_name, None);
    }

    /// Test that get_server_for_virtual_uri works with process sharing.
    ///
    /// When ts and tsx both use "tsgo" as server_name, the reverse lookup
    /// should return "tsgo" for both languages.
    #[tokio::test]
    async fn get_server_for_virtual_uri_with_process_sharing() {
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

        // Both should return "tsgo" as server_name
        assert_eq!(
            tracker.get_server_for_virtual_uri(&virtual_uri_ts).await,
            Some("tsgo".to_string())
        );
        assert_eq!(
            tracker.get_server_for_virtual_uri(&virtual_uri_tsx).await,
            Some("tsgo".to_string())
        );
    }
}
