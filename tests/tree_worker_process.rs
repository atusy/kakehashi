use std::path::PathBuf;
use std::sync::{Arc, Barrier};

#[cfg(feature = "e2e")]
use kakehashi::tree_worker::{
    ApplyDocumentEdits, ByteEdit, CloseDocument, DeriveDocumentSnapshot, NavigateNode,
    NodeNavigation, OpaqueNodeId, ResolveNode, SyncDocument,
};
use kakehashi::tree_worker::{Client, DeriveSnapshot, RequestContext, Response};

#[cfg(feature = "e2e")]
fn digest(path: &std::path::Path) -> String {
    kakehashi::tree_worker::artifact_digest(path).unwrap()
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
fn real_worker_keeps_document_text_and_tree_across_incremental_edits() {
    let data_dir = kakehashi::install::test_support::test_data_dir_path();
    std::fs::create_dir_all(&data_dir).unwrap();
    kakehashi::install::test_support::ensure_test_languages_installed(&data_dir).unwrap();
    let parser = data_dir
        .join("parser")
        .join(format!("rust.{}", std::env::consts::DLL_EXTENSION));
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_kakehashi"));
    let worker = Client::spawn(&executable, 2, 44).unwrap();
    let context = RequestContext {
        request_id: 30,
        worker_generation: 44,
        uri: "file:///incremental.rs".into(),
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

    let mut node_context = snapshot.context.clone();
    node_context.request_id = 33;
    let response = worker
        .resolve_node(ResolveNode {
            context: node_context.clone(),
            byte_offset: 12,
            named: true,
        })
        .unwrap();
    let Response::Nodes(nodes) = response else {
        panic!("node resolve must return owned data: {response:?}");
    };
    assert_eq!(nodes.nodes[0].kind, "identifier");
    assert_eq!(nodes.nodes[0].id.worker_generation, 44);

    node_context.request_id = 34;
    let response = worker
        .navigate_node(NavigateNode {
            context: node_context,
            node_id: OpaqueNodeId {
                worker_generation: 43,
                local_id: nodes.nodes[0].id.local_id,
            },
            operation: NodeNavigation::Parent,
        })
        .unwrap();
    let Response::Nodes(nodes) = response else {
        panic!("stale node navigation must be contained: {response:?}");
    };
    assert!(nodes.nodes.is_empty());

    let mut closed = snapshot.context;
    closed.request_id = 35;
    closed.content_version = 3;
    closed.configuration_generation = 1;
    let response = worker
        .close_document(CloseDocument {
            context: closed.clone(),
        })
        .unwrap();
    assert!(matches!(response, Response::DocumentClosed(_)));

    closed.request_id = 36;
    let response = worker
        .sync_document(SyncDocument {
            context: closed.clone(),
            language: "rust".into(),
            grammar_symbol: "rust".into(),
            source_path: parser.clone(),
            parser_path: parser.clone(),
            artifact_digest: digest(&parser),
            text: "fn stale() {}".into(),
        })
        .unwrap();
    let Response::Error(error) = response else {
        panic!("stale full sync must not resurrect a closed document: {response:?}");
    };
    assert!(error.message.contains("closed document incarnation"));

    closed.request_id = 37;
    let response = worker
        .derive_document_snapshot(DeriveDocumentSnapshot { context: closed })
        .unwrap();
    let Response::Error(error) = response else {
        panic!("closed document must not retain its tree: {response:?}");
    };
    assert!(error.message.contains("missing"));
    worker.shutdown().unwrap();
}
