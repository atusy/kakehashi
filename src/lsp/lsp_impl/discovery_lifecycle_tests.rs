use super::*;
use crate::config::settings::{LanguageSettings, QueryItem, QueryKind};
use crate::document::snapshot::ParseSnapshot;
use crate::language::injection::{
    assert_discovery_matches_reference, collect_all_injections, effective_content_range,
};
use std::sync::Arc;
use tower_lsp_server::LspService;
use tower_lsp_server::ls_types::{TextDocumentIdentifier, TextDocumentItem};

async fn populated_snapshot(server: &Kakehashi, uri: &Url) -> Arc<ParseSnapshot> {
    let mut snapshots = server.documents.subscribe_snapshots(uri).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let snapshot = snapshots.borrow_and_update().snapshot.clone();
            if let Some(snapshot) = snapshot
                && snapshot
                    .injection_regions
                    .as_ref()
                    .is_some_and(|discovery| {
                        discovery.generation == server.cache.semantic_token_generation()
                    })
            {
                return snapshot;
            }
            snapshots.changed().await.unwrap();
        }
    })
    .await
    .expect("the lifecycle transition must publish current discovery")
}

fn assert_published_discovery(
    server: &Kakehashi,
    snapshot: &ParseSnapshot,
    expected_text: &str,
    expected_language: &str,
    expected_offset: i32,
) {
    assert_eq!(&*snapshot.text, expected_text);
    assert!(snapshot.text.len() >= 64 * 1024);
    let tree = snapshot.tree.as_ref().unwrap();
    let query = server.language.injection_query("rust").unwrap();
    assert!(tree.root_node().child_count() >= 2);
    assert!(
        (0..query.pattern_count())
            .all(|index| query.is_pattern_rooted(index) && !query.is_pattern_non_local(index))
    );
    // Compare all borrowed descriptors, including node identity, on the actual
    // tree/text/query obtained after the lifecycle transition.
    assert_discovery_matches_reference(tree, &snapshot.text, &query);
    let reference =
        collect_all_injections(&tree.root_node(), &snapshot.text, Some(&query)).unwrap();
    assert_eq!(reference.len(), 3000);
    for region in &reference {
        assert_eq!(region.language, expected_language);
        assert_eq!(region.offset.unwrap().start_column, expected_offset);
    }

    // Also compare the owned product published by the real populate path, so
    // rerunning two collectors against the same stale inputs cannot hide a
    // lifecycle wiring error.
    let discovery = snapshot.injection_regions.as_ref().unwrap();
    assert!(discovery.complete);
    assert_eq!(
        discovery.generation,
        server.cache.semantic_token_generation()
    );
    assert_eq!(discovery.regions.len(), reference.len());
    for (owned, region) in discovery.regions.iter().zip(&reference) {
        let range = effective_content_range(region, &snapshot.text);
        assert_eq!(owned.resolved_lang, expected_language);
        assert_eq!(owned.content_start_byte..owned.content_end_byte, range);
    }
}

#[rstest::rstest]
#[case::reopen(false)]
#[case::query_reload(true)]
#[tokio::test]
async fn published_parallel_discovery_matches_oracle_after_lifecycle_change(#[case] reload: bool) {
    let (service, mut socket) = LspService::new(|client| {
        let mut server = Kakehashi::new(client);
        // Exercise production parallel dispatch even on single-core CI hosts.
        server.compute_pool = Arc::new(crate::compute_pool::ComputePool::with_test_threads(2));
        server
    });
    let client = tokio::spawn(async move {
        use futures::StreamExt;
        while socket.next().await.is_some() {}
    });
    let server = service.inner();
    assert_eq!(server.compute_pool.thread_count(), 2);
    for name in ["rust", "before", "after"] {
        server
            .language
            .language_registry_for_parallel()
            .register(name.into(), tree_sitter_rust::LANGUAGE.into());
    }
    let directory = tempfile::tempdir().unwrap();
    let query_path = directory.path().join("injections.scm");
    let write_query = |language, offset| {
        std::fs::write(
            &query_path,
            format!(
                "((block_comment) @injection.content (#set! injection.language \"{language}\") \
                 (#offset! @injection.content 0 {offset} 0 -2))"
            ),
        )
        .unwrap();
    };
    write_query("before", 2);
    let settings = WorkspaceSettings {
        auto_install: false,
        search_paths: Vec::new(),
        languages: std::collections::HashMap::from([(
            "rust".into(),
            LanguageSettings {
                queries: Some(vec![QueryItem {
                    path: query_path.to_string_lossy().into_owned(),
                    kind: Some(QueryKind::Injections),
                }]),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    server
        .apply_raw_settings(RawWorkspaceSettings::default(), settings.clone())
        .await;
    let uri = Url::parse("file:///test/discovery-lifecycle.rs").unwrap();
    let lsp_uri = url_to_uri(&uri).unwrap();
    let open = |text| DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: lsp_uri.clone(),
            language_id: "rust".into(),
            version: 1,
            text,
        },
    };
    let original = "/* original payload */\nfn item() {}\n".repeat(3000);
    server.did_open_impl(open(original.clone())).await;
    let first = populated_snapshot(server, &uri).await;
    assert_published_discovery(server, &first, &original, "before", 2);
    let first_generation = first.injection_regions.as_ref().unwrap().generation;

    if reload {
        write_query("after", 3);
        // The real settings transaction reloads the file, bumps generations,
        // invalidates snapshots, and schedules the replacement parse.
        server
            .apply_raw_settings(RawWorkspaceSettings::default(), settings)
            .await;
        let current = populated_snapshot(server, &uri).await;
        assert_eq!(current.incarnation, first.incarnation);
        assert!(server.cache.semantic_token_generation() > first_generation);
        assert!(current.parsed_version > first.parsed_version);
        assert_published_discovery(server, &current, &original, "after", 3);
    } else {
        server
            .did_close_impl(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier {
                    uri: lsp_uri.clone(),
                },
            })
            .await;
        assert!(server.documents.get(&uri).is_none());
        let reopened = "\n/* reopened payload */\nfn item() {}\n".repeat(3000);
        server.did_open_impl(open(reopened.clone())).await;
        let current = populated_snapshot(server, &uri).await;
        assert_ne!(current.incarnation, first.incarnation);
        assert_eq!(server.cache.semantic_token_generation(), first_generation);
        assert_published_discovery(server, &current, &reopened, "before", 2);
    }
    server
        .did_close_impl(DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: url_to_uri(&uri).unwrap(),
            },
        })
        .await;
    client.abort();
}
