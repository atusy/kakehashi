use std::path::PathBuf;
use std::sync::{Arc, Barrier};

#[cfg(feature = "e2e")]
use kakehashi::tree_worker::{
    ApplyDocumentEdits, ByteEdit, CloseDocument, ConfigureLanguages, DeriveDocumentSnapshot,
    DeriveNativeBindings, DeriveSemanticTokens, InspectDocumentMemory, NavigateNode,
    NodeNavigation, NodeScalarOperation, NodeScalarValue, OpaqueNodeId, ResolveNode, RunCaptures,
    RunNodeScalar, SyncDocument, WireByteRange, WirePosition, WorkerLanguageAsset,
    WorkerLanguageCatalog, WorkerQuerySources, WorkerTestMemoryBudgets,
};
use kakehashi::tree_worker::{Client, DeriveSnapshot, RequestContext, Response};

#[cfg(feature = "e2e")]
fn digest(path: &std::path::Path) -> String {
    kakehashi::tree_worker::artifact_digest(path).unwrap()
}

#[cfg(feature = "e2e")]
fn memory_test_worker(
    worker_generation: u64,
    derived_cache_soft_bytes: usize,
    non_evictable_estimate_hard_bytes: usize,
) -> (Client, PathBuf) {
    let data_dir = kakehashi::install::test_support::test_data_dir_path();
    std::fs::create_dir_all(&data_dir).unwrap();
    kakehashi::install::test_support::ensure_test_languages_installed(&data_dir).unwrap();
    let parser = data_dir
        .join("parser")
        .join(format!("rust.{}", std::env::consts::DLL_EXTENSION));
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_kakehashi"));
    let worker = Client::spawn_with_test_memory_budgets(
        &executable,
        1,
        worker_generation,
        WorkerTestMemoryBudgets::new(derived_cache_soft_bytes, non_evictable_estimate_hard_bytes)
            .unwrap(),
    )
    .unwrap();
    (worker, parser)
}

#[test]
fn real_worker_handshakes_and_contains_request_errors() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_kakehashi"));
    let worker = Client::spawn(&executable, 2, 41).unwrap();

    assert_eq!(worker.compute_threads(), 2);
    let response = worker
        .derive(DeriveSnapshot {
            context: RequestContext {
                request_id: 9,
                worker_generation: 41,
                uri: "file:///missing.rs".into(),
                incarnation: 1,
                content_version: 0,
                configuration_generation: 0,
            },
            language: "rust".into(),
            grammar_symbol: "rust".into(),
            parser_path: PathBuf::from("/missing/tree-sitter-rust"),
            artifact_digest: "sha256:missing-rust".into(),
            text: "fn main() {}".into(),
        })
        .unwrap();

    let Response::Error(error) = response else {
        panic!("missing grammar must be a contained request error");
    };
    assert_eq!(
        error.context.as_ref().map(|context| context.request_id),
        Some(9)
    );
    worker.shutdown().unwrap();
}

#[test]
fn concurrent_requests_are_routed_by_request_id() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_kakehashi"));
    let worker = Arc::new(Client::spawn(&executable, 2, 43).unwrap());
    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = [20_u64, 21]
        .into_iter()
        .map(|request_id| {
            let worker = Arc::clone(&worker);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                worker
                    .derive(DeriveSnapshot {
                        context: RequestContext {
                            request_id,
                            worker_generation: 43,
                            uri: format!("file:///{request_id}.rs"),
                            incarnation: 1,
                            content_version: 0,
                            configuration_generation: 0,
                        },
                        language: "rust".into(),
                        grammar_symbol: "rust".into(),
                        parser_path: PathBuf::from(format!("/missing/{request_id}")),
                        artifact_digest: format!("sha256:missing-{request_id}"),
                        text: "fn main() {}".into(),
                    })
                    .unwrap()
            })
        })
        .collect();
    barrier.wait();

    let mut ids = handles
        .into_iter()
        .map(|handle| match handle.join().unwrap() {
            Response::Error(error) => error.context.unwrap().request_id,
            response => panic!("missing grammars must fail independently: {response:?}"),
        })
        .collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(ids, [20, 21]);
    Arc::try_unwrap(worker)
        .ok()
        .expect("request threads released the worker")
        .shutdown()
        .unwrap();
}

#[cfg(feature = "e2e")]
#[test]
fn real_worker_derives_a_snapshot_from_an_installed_grammar() {
    let data_dir = kakehashi::install::test_support::test_data_dir_path();
    std::fs::create_dir_all(&data_dir).unwrap();
    kakehashi::install::test_support::ensure_test_languages_installed(&data_dir).unwrap();
    let parser = data_dir
        .join("parser")
        .join(format!("rust.{}", std::env::consts::DLL_EXTENSION));
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_kakehashi"));
    let worker = Client::spawn(&executable, 1, 42).unwrap();

    let rejected = worker
        .derive(DeriveSnapshot {
            context: RequestContext {
                request_id: 9,
                worker_generation: 42,
                uri: "file:///example.rs".into(),
                incarnation: 1,
                content_version: 0,
                configuration_generation: 0,
            },
            language: "rust".into(),
            grammar_symbol: "rust".into(),
            parser_path: parser.clone(),
            artifact_digest: "sha256:not-the-parser".into(),
            text: "fn main() {}".into(),
        })
        .unwrap();
    let Response::Error(error) = rejected else {
        panic!("digest mismatch must be rejected: {rejected:?}");
    };
    assert!(error.message.contains("digest mismatch"));

    let response = worker
        .derive(DeriveSnapshot {
            context: RequestContext {
                request_id: 10,
                worker_generation: 42,
                uri: "file:///example.rs".into(),
                incarnation: 1,
                content_version: 0,
                configuration_generation: 0,
            },
            language: "rust".into(),
            grammar_symbol: "rust".into(),
            parser_path: parser.clone(),
            artifact_digest: digest(&parser),
            text: "fn main() {}".into(),
        })
        .unwrap();

    let Response::Snapshot(snapshot) = response else {
        panic!("installed grammar must produce a snapshot: {response:?}");
    };
    assert_eq!(snapshot.root_kind, "source_file");
    assert_eq!(snapshot.context.worker_generation, 42);
    assert_eq!(snapshot.parser_cache_hit, Some(false));

    let response = worker
        .derive(DeriveSnapshot {
            context: RequestContext {
                request_id: 11,
                worker_generation: 42,
                uri: "file:///example.rs".into(),
                incarnation: 1,
                content_version: 1,
                configuration_generation: 0,
            },
            language: "rust".into(),
            grammar_symbol: "rust".into(),
            parser_path: parser.clone(),
            artifact_digest: digest(&parser),
            text: "fn main() { let x = 1; }".into(),
        })
        .unwrap();
    let Response::Snapshot(snapshot) = response else {
        panic!("second derive must produce a snapshot: {response:?}");
    };
    assert_eq!(snapshot.parser_cache_hit, Some(true));
    assert!(snapshot.compute_ns > 0);
    worker.shutdown().unwrap();
}

#[cfg(feature = "e2e")]
#[test]
fn real_worker_contains_full_sync_above_injected_non_evictable_budget() {
    let (worker, parser) = memory_test_worker(45, 1024, 1);
    let context = RequestContext {
        request_id: 1,
        worker_generation: 45,
        uri: "file:///hard-budget.rs".into(),
        incarnation: 1,
        content_version: 1,
        configuration_generation: 0,
    };

    let response = worker
        .sync_document(SyncDocument {
            context,
            language: "rust".into(),
            grammar_symbol: "rust".into(),
            source_path: parser.clone(),
            parser_path: parser.clone(),
            artifact_digest: digest(&parser),
            queries: WorkerQuerySources::default(),
            text: "fn main() {}".into(),
        })
        .unwrap();

    let Response::Error(error) = response else {
        panic!("hard admission must be a contained worker error: {response:?}");
    };
    assert!(
        error
            .message
            .contains("non-evictable budget 1 bytes exceeded")
    );
    assert_eq!(worker.worker_generation(), 45);
    worker.shutdown().unwrap();
}

#[cfg(feature = "e2e")]
#[test]
fn rejected_full_sync_preserves_replica_and_admission_capacity() {
    let (worker, parser) = memory_test_worker(46, 1024, 1024);
    let original = "fn main() {}";
    let context = RequestContext {
        request_id: 1,
        worker_generation: 46,
        uri: "file:///full-sync-transaction.rs".into(),
        incarnation: 1,
        content_version: 1,
        configuration_generation: 0,
    };
    let sync = |context: RequestContext, text: String| SyncDocument {
        context,
        language: "rust".into(),
        grammar_symbol: "rust".into(),
        source_path: parser.clone(),
        parser_path: parser.clone(),
        artifact_digest: digest(&parser),
        queries: WorkerQuerySources::default(),
        text,
    };

    assert!(matches!(
        worker
            .sync_document(sync(context.clone(), original.into()))
            .unwrap(),
        Response::DocumentAck(_)
    ));
    let response = worker
        .resolve_node(ResolveNode {
            context: RequestContext {
                request_id: 11,
                ..context.clone()
            },
            byte_offset: 3,
            named: true,
            layer: kakehashi::tree_worker::NodeLayerSelector::Host,
        })
        .unwrap();
    let Response::Nodes(nodes) = response else {
        panic!("original node identity must be minted: {response:?}");
    };
    let original_node = nodes.nodes[0].id.clone();

    let oversized = format!(
        "fn main() {{ {} }}",
        (0..200)
            .map(|index| format!("let value_{index} = {index};"))
            .collect::<String>()
    );
    let rejected_context = RequestContext {
        request_id: 2,
        content_version: 2,
        ..context.clone()
    };
    let rejected = worker
        .sync_document(sync(rejected_context, oversized))
        .unwrap();
    let Response::Error(error) = rejected else {
        panic!("oversized replacement must be contained: {rejected:?}");
    };
    assert!(
        error
            .message
            .contains("non-evictable budget 1024 bytes exceeded")
    );

    let response = worker
        .derive_document_snapshot(DeriveDocumentSnapshot {
            context: RequestContext {
                request_id: 3,
                ..context.clone()
            },
        })
        .unwrap();
    let Response::Snapshot(snapshot) = response else {
        panic!("rejected replacement must preserve version 1: {response:?}");
    };
    assert_eq!(snapshot.root_end_byte, original.len());
    let response = worker
        .run_node_scalar(RunNodeScalar {
            context: RequestContext {
                request_id: 31,
                ..context.clone()
            },
            node_id: original_node,
            operation: NodeScalarOperation::Text,
        })
        .unwrap();
    let Response::NodeScalar(node) = response else {
        panic!("rejected replacement must preserve node identity: {response:?}");
    };
    assert_eq!(node.value, Some(NodeScalarValue::String("main".into())));

    let replacement = "fn next() {}";
    let response = worker
        .sync_document(sync(
            RequestContext {
                request_id: 4,
                content_version: 2,
                ..context
            },
            replacement.into(),
        ))
        .unwrap();
    assert!(matches!(response, Response::DocumentAck(_)));
    worker.shutdown().unwrap();
}

#[cfg(feature = "e2e")]
#[test]
fn rejected_incremental_edit_preserves_replica_and_admission_capacity() {
    let (worker, parser) = memory_test_worker(47, 1024, 1024);
    let original = "fn main() {}";
    let context = RequestContext {
        request_id: 1,
        worker_generation: 47,
        uri: "file:///incremental-transaction.rs".into(),
        incarnation: 1,
        content_version: 1,
        configuration_generation: 0,
    };
    let response = worker
        .sync_document(SyncDocument {
            context: context.clone(),
            language: "rust".into(),
            grammar_symbol: "rust".into(),
            source_path: parser.clone(),
            parser_path: parser.clone(),
            artifact_digest: digest(&parser),
            queries: WorkerQuerySources::default(),
            text: original.into(),
        })
        .unwrap();
    assert!(matches!(response, Response::DocumentAck(_)));
    let response = worker
        .resolve_node(ResolveNode {
            context: RequestContext {
                request_id: 11,
                ..context.clone()
            },
            byte_offset: 3,
            named: true,
            layer: kakehashi::tree_worker::NodeLayerSelector::Host,
        })
        .unwrap();
    let Response::Nodes(nodes) = response else {
        panic!("original node identity must be minted: {response:?}");
    };
    let original_node = nodes.nodes[0].id.clone();

    let oversized = (0..200)
        .map(|index| format!("let value_{index} = {index};"))
        .collect::<String>();
    let rejected = worker
        .apply_document_edits(ApplyDocumentEdits {
            context: RequestContext {
                request_id: 2,
                content_version: 2,
                ..context.clone()
            },
            base_version: 1,
            edits: vec![ByteEdit {
                start_byte: original.len() - 1,
                old_end_byte: original.len() - 1,
                new_text: oversized,
            }],
        })
        .unwrap();
    let Response::Error(error) = rejected else {
        panic!("oversized incremental edit must be contained: {rejected:?}");
    };
    assert!(
        error
            .message
            .contains("non-evictable budget 1024 bytes exceeded")
    );

    let response = worker
        .derive_document_snapshot(DeriveDocumentSnapshot {
            context: RequestContext {
                request_id: 3,
                ..context.clone()
            },
        })
        .unwrap();
    let Response::Snapshot(snapshot) = response else {
        panic!("rejected edit must preserve version 1: {response:?}");
    };
    assert_eq!(snapshot.root_end_byte, original.len());
    let response = worker
        .run_node_scalar(RunNodeScalar {
            context: RequestContext {
                request_id: 31,
                ..context.clone()
            },
            node_id: original_node,
            operation: NodeScalarOperation::Text,
        })
        .unwrap();
    let Response::NodeScalar(node) = response else {
        panic!("rejected edit must preserve node identity: {response:?}");
    };
    assert_eq!(node.value, Some(NodeScalarValue::String("main".into())));

    let response = worker
        .apply_document_edits(ApplyDocumentEdits {
            context: RequestContext {
                request_id: 4,
                content_version: 2,
                ..context
            },
            base_version: 1,
            edits: vec![ByteEdit {
                start_byte: 3,
                old_end_byte: 7,
                new_text: "next".into(),
            }],
        })
        .unwrap();
    assert!(matches!(response, Response::DocumentAck(_)));
    worker.shutdown().unwrap();
}

#[cfg(feature = "e2e")]
#[test]
fn soft_budget_evicts_non_current_result_without_invalidating_identity() {
    let (worker, parser) = memory_test_worker(48, 20, 4096);
    let catalog_context = RequestContext {
        request_id: 1,
        worker_generation: 48,
        uri: "kakehashi://memory-stress-catalog".into(),
        incarnation: 0,
        content_version: 0,
        configuration_generation: 0,
    };
    let response = worker
        .configure_languages(ConfigureLanguages {
            context: catalog_context,
            catalog: WorkerLanguageCatalog {
                assets: vec![WorkerLanguageAsset {
                    language: "rust".into(),
                    grammar_symbol: "rust".into(),
                    source_path: parser.clone(),
                    parser_path: parser.clone(),
                    artifact_digest: digest(&parser),
                    queries: WorkerQuerySources::default(),
                }],
                capture_mappings: serde_json::from_value(serde_json::json!({
                    "rust": { "highlights": { "variable": "keyword" } }
                }))
                .unwrap(),
                search_paths: Vec::new(),
            },
        })
        .unwrap();
    assert!(matches!(response, Response::LanguageCatalogAck(_)));

    let context = |request_id, uri: &str| RequestContext {
        request_id,
        worker_generation: 48,
        uri: uri.into(),
        incarnation: 1,
        content_version: 1,
        configuration_generation: 0,
    };
    let a = context(2, "file:///soft-a.rs");
    let b = context(3, "file:///soft-b.rs");
    for document in [&a, &b] {
        let response = worker
            .sync_document(SyncDocument {
                context: document.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: parser.clone(),
                parser_path: parser.clone(),
                artifact_digest: digest(&parser),
                queries: WorkerQuerySources {
                    highlights: Some("(identifier) @variable".into()),
                    ..Default::default()
                },
                text: "fn main() {}".into(),
            })
            .unwrap();
        assert!(matches!(response, Response::DocumentAck(_)));
    }
    let response = worker
        .resolve_node(ResolveNode {
            context: RequestContext {
                request_id: 4,
                ..a.clone()
            },
            byte_offset: 3,
            named: true,
            layer: kakehashi::tree_worker::NodeLayerSelector::Host,
        })
        .unwrap();
    let Response::Nodes(nodes) = response else {
        panic!("document A node identity must be minted: {response:?}");
    };
    let a_node = nodes.nodes[0].id.clone();

    let derive = |request_id, context: &RequestContext| {
        worker
            .derive_semantic_tokens(DeriveSemanticTokens {
                context: RequestContext {
                    request_id,
                    ..context.clone()
                },
                supports_multiline: false,
            })
            .unwrap()
    };
    let Response::SemanticTokens(a_first) = derive(5, &a) else {
        panic!("document A semantic derivation must succeed");
    };
    assert!(!a_first.cache_hit);
    let Response::SemanticTokens(b_first) = derive(6, &b) else {
        panic!("document B semantic derivation must succeed");
    };
    assert!(!b_first.cache_hit);

    let inspect = |request_id, context: &RequestContext| {
        worker
            .inspect_document_memory(InspectDocumentMemory {
                context: RequestContext {
                    request_id,
                    ..context.clone()
                },
            })
            .unwrap()
    };
    let Response::DocumentMemory(a_memory) = inspect(7, &a) else {
        panic!("document A memory must be inspectable");
    };
    let Response::DocumentMemory(b_memory) = inspect(8, &b) else {
        panic!("document B memory must be inspectable");
    };
    assert_eq!(a_memory.result_cache_bytes, 0);
    assert_eq!(a_memory.auxiliary_cache_bytes, 0);
    assert_eq!(b_memory.result_cache_bytes, 20);
    assert_eq!(b_memory.auxiliary_cache_bytes, 0);

    let Response::SemanticTokens(a_recomputed) = derive(9, &a) else {
        panic!("evicted document A must recompute");
    };
    assert!(!a_recomputed.cache_hit);
    assert_eq!(a_recomputed.tokens, a_first.tokens);
    let response = worker
        .run_node_scalar(RunNodeScalar {
            context: RequestContext {
                request_id: 10,
                ..a
            },
            node_id: a_node,
            operation: NodeScalarOperation::Text,
        })
        .unwrap();
    let Response::NodeScalar(node) = response else {
        panic!("cache eviction must preserve node identity: {response:?}");
    };
    assert_eq!(node.value, Some(NodeScalarValue::String("main".into())));
    worker.shutdown().unwrap();
}

#[cfg(feature = "e2e")]
#[test]
fn real_worker_keeps_document_text_and_tree_across_incremental_edits() {
    let data_dir = kakehashi::install::test_support::test_data_dir_path();
    std::fs::create_dir_all(&data_dir).unwrap();
    kakehashi::install::test_support::ensure_test_languages_installed(&data_dir).unwrap();
    let parser = data_dir
        .join("parser")
        .join(format!("rust.{}", std::env::consts::DLL_EXTENSION));
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_kakehashi"));
    let worker = Client::spawn(&executable, 2, 44).unwrap();
    let capture_queries = tempfile::tempdir().unwrap();
    let capture_query_dir = capture_queries.path().join("queries/rust");
    std::fs::create_dir_all(&capture_query_dir).unwrap();
    std::fs::write(capture_query_dir.join("context.scm"), "(identifier) @name").unwrap();
    let context = RequestContext {
        request_id: 30,
        worker_generation: 44,
        uri: "file:///incremental.rs".into(),
        incarnation: 1,
        content_version: 1,
        configuration_generation: 0,
    };

    let response = worker
        .configure_languages(ConfigureLanguages {
            context: RequestContext {
                request_id: 29,
                worker_generation: 44,
                uri: "kakehashi://language-catalog".into(),
                incarnation: 0,
                content_version: 0,
                configuration_generation: 0,
            },
            catalog: WorkerLanguageCatalog {
                assets: vec![WorkerLanguageAsset {
                    language: "rust".into(),
                    grammar_symbol: "rust".into(),
                    source_path: parser.clone(),
                    parser_path: parser.clone(),
                    artifact_digest: digest(&parser),
                    queries: WorkerQuerySources {
                        injections: Some("(identifier) @injection.content".into()),
                        ..Default::default()
                    },
                }],
                capture_mappings: serde_json::from_value(serde_json::json!({
                    "rust": { "highlights": { "variable": "keyword" } }
                }))
                .unwrap(),
                search_paths: vec![capture_queries.path().into()],
            },
        })
        .unwrap();
    let Response::LanguageCatalogAck(ack) = response else {
        panic!("language catalog must be acknowledged: {response:?}");
    };
    assert_eq!(ack.languages, 1);

    let response = worker
        .sync_document(SyncDocument {
            context: context.clone(),
            language: "rust".into(),
            grammar_symbol: "rust".into(),
            source_path: parser.clone(),
            parser_path: parser.clone(),
            artifact_digest: digest(&parser),
            queries: WorkerQuerySources {
                highlights: Some("(identifier) @variable".into()),
                ..Default::default()
            },
            text: "fn main() { 1 }".into(),
        })
        .unwrap();
    let Response::DocumentAck(ack) = response else {
        panic!("full sync must be acknowledged: {response:?}");
    };
    assert!(!ack.incremental);

    let mut edited = context;
    edited.request_id = 31;
    edited.content_version = 2;
    let response = worker
        .apply_document_edits(ApplyDocumentEdits {
            context: edited.clone(),
            base_version: 1,
            edits: vec![ByteEdit {
                start_byte: 12,
                old_end_byte: 13,
                new_text: "value + 2".into(),
            }],
        })
        .unwrap();
    let Response::DocumentAck(ack) = response else {
        panic!("incremental edit must be acknowledged: {response:?}");
    };
    assert!(ack.incremental);

    edited.request_id = 32;
    let response = worker
        .derive_document_snapshot(DeriveDocumentSnapshot { context: edited })
        .unwrap();
    let Response::Snapshot(snapshot) = response else {
        panic!("derive must read worker-owned state: {response:?}");
    };
    assert_eq!(snapshot.root_end_byte, "fn main() { value + 2 }".len());
    assert_eq!(snapshot.parser_cache_hit, None);
    assert!(snapshot.compute_ns > 0);

    let mut semantic_context = snapshot.context.clone();
    semantic_context.request_id = 321;
    let response = worker
        .derive_semantic_tokens(DeriveSemanticTokens {
            context: semantic_context,
            supports_multiline: false,
        })
        .unwrap();
    let Response::SemanticTokens(tokens) = response else {
        panic!("semantic tokens must be worker-owned: {response:?}");
    };
    assert!(!tokens.tokens.is_empty());
    assert!(tokens.tokens.iter().all(|token| token.token_type == 1));

    let mut captures_context = snapshot.context.clone();
    captures_context.request_id = 322;
    let response = worker
        .run_captures(RunCaptures {
            context: captures_context,
            kind: "context".into(),
            range: None,
            injection: false,
        })
        .unwrap();
    let Response::Captures(captures) = response else {
        panic!("captures must be worker-owned: {response:?}");
    };
    assert!(captures.available);
    assert_eq!(captures.matches.len(), 2);

    let mut node_context = snapshot.context.clone();
    node_context.request_id = 33;
    let response = worker
        .resolve_node(ResolveNode {
            context: node_context.clone(),
            byte_offset: 12,
            named: true,
            layer: kakehashi::tree_worker::NodeLayerSelector::Host,
        })
        .unwrap();
    let Response::Nodes(nodes) = response else {
        panic!("node resolve must return owned data: {response:?}");
    };
    assert_eq!(nodes.nodes[0].kind, "identifier");
    assert_eq!(nodes.nodes[0].id.worker_generation, 44);

    node_context.request_id = 34;
    let response = worker
        .run_node_scalar(RunNodeScalar {
            context: node_context.clone(),
            node_id: nodes.nodes[0].id.clone(),
            operation: NodeScalarOperation::Text,
        })
        .unwrap();
    let Response::NodeScalar(scalar) = response else {
        panic!("node scalar must return typed owned data: {response:?}");
    };
    assert_eq!(scalar.value, Some(NodeScalarValue::String("value".into())));

    node_context.request_id = 35;
    let response = worker
        .navigate_node(NavigateNode {
            context: node_context,
            node_id: OpaqueNodeId {
                worker_generation: 43,
                local_id: nodes.nodes[0].id.local_id.clone(),
            },
            operation: NodeNavigation::Parent,
        })
        .unwrap();
    let Response::Nodes(nodes) = response else {
        panic!("stale node navigation must be contained: {response:?}");
    };
    assert!(nodes.nodes.is_empty());

    let bindings_context = RequestContext {
        request_id: 351,
        worker_generation: 44,
        uri: "file:///bindings-process.rs".into(),
        incarnation: 1,
        content_version: 1,
        configuration_generation: 0,
    };
    let response = worker
        .sync_document(SyncDocument {
            context: bindings_context.clone(),
            language: "rust".into(),
            grammar_symbol: "rust".into(),
            source_path: parser.clone(),
            parser_path: parser.clone(),
            artifact_digest: digest(&parser),
            queries: WorkerQuerySources {
                bindings: Some(
                    r#"
                    (block) @scope
                    (let_declaration pattern: (identifier) @definition)
                    (identifier) @reference
                    "#
                    .into(),
                ),
                ..Default::default()
            },
            text: "fn main() { let target = 1; target; }".into(),
        })
        .unwrap();
    assert!(matches!(response, Response::DocumentAck(_)));
    let response = worker
        .derive_native_bindings(DeriveNativeBindings {
            context: RequestContext {
                request_id: 352,
                ..bindings_context
            },
            position: WirePosition {
                line: 0,
                character: 28,
            },
        })
        .unwrap();
    let Response::NativeBindings(bindings) = response else {
        panic!("native bindings must be worker-owned: {response:?}");
    };
    let facts = bindings.facts.expect("reference must resolve");
    assert_eq!(facts.definition, Some(WireByteRange { start: 16, end: 22 }));
    assert_eq!(facts.references, vec![WireByteRange { start: 28, end: 34 }]);

    let mut closed = snapshot.context;
    closed.request_id = 36;
    closed.content_version = 3;
    closed.configuration_generation = 1;
    let response = worker
        .close_document(CloseDocument {
            context: closed.clone(),
        })
        .unwrap();
    assert!(matches!(response, Response::DocumentClosed(_)));

    closed.request_id = 37;
    let response = worker
        .sync_document(SyncDocument {
            context: closed.clone(),
            language: "rust".into(),
            grammar_symbol: "rust".into(),
            source_path: parser.clone(),
            parser_path: parser.clone(),
            artifact_digest: digest(&parser),
            queries: Default::default(),
            text: "fn stale() {}".into(),
        })
        .unwrap();
    let Response::Error(error) = response else {
        panic!("stale full sync must not resurrect a closed document: {response:?}");
    };
    assert!(error.message.contains("closed document incarnation"));

    closed.request_id = 38;
    let response = worker
        .derive_document_snapshot(DeriveDocumentSnapshot { context: closed })
        .unwrap();
    let Response::Error(error) = response else {
        panic!("closed document must not retain its tree: {response:?}");
    };
    assert!(error.message.contains("missing"));
    worker.shutdown().unwrap();
}
