//! What a `workspace/didChangeConfiguration` reload asks of the open documents
//! and the client: a full reparse (which also re-drives injection processing,
//! eager bridge opens and diagnostics) and a `semanticTokens/refresh` are paid
//! only when something they depend on changed — a parse-relevant setting, or
//! the parser/query files a reload actually read.

use super::*;
use crate::config::settings::{LanguageSettings, LogMessageLevel, QueryTypeMappings};
use std::path::{Path, PathBuf};
use tower_lsp_server::LspService;
use tower_lsp_server::ls_types::TextDocumentItem;

/// A server with no language configuration of its own: `rust` is registered
/// the way a built-in grammar is, so documents parse without any search path.
fn server_with_builtin_rust() -> (LspService<Kakehashi>, tokio::task::JoinHandle<()>) {
    let (service, mut socket) = LspService::new(Kakehashi::new);
    let client = tokio::spawn(async move {
        use futures::StreamExt;
        while socket.next().await.is_some() {}
    });
    service
        .inner()
        .language
        .language_registry_for_parallel()
        .register("rust".into(), tree_sitter_rust::LANGUAGE.into());
    (service, client)
}

fn baseline_settings() -> WorkspaceSettings {
    WorkspaceSettings {
        auto_install: false,
        ..Default::default()
    }
}

async fn open_and_wait_for_tree(server: &Kakehashi, name: &str, language_id: &str) -> Url {
    let uri = Url::parse(&format!("file:///settings-reload/{name}")).unwrap();
    server
        .did_open_impl(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: url_to_uri(&uri).unwrap(),
                language_id: language_id.into(),
                version: 1,
                text: "local x = 1\nfn main() {}\n".into(),
            },
        })
        .await;
    wait_for_tree(server, &uri).await;
    uri
}

async fn wait_for_tree(server: &Kakehashi, uri: &Url) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !server
            .documents
            .get(uri)
            .is_some_and(|document| document.has_current_tree())
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the opened document must get a current tree");
}

/// Nothing observable happened to the documents: no reparse, no refresh,
/// and — because the reload itself is what disturbs them — no token
/// generation bump (it would null in-flight token requests and stale every
/// stored injection region) and no loss of the document's language.
fn assert_no_reload_work(
    outcome: &SettingsReloadOutcome,
    server: &Kakehashi,
    uri: &Url,
    token_generation_before: u64,
) {
    assert_eq!(
        server.cache.semantic_token_generation(),
        token_generation_before,
        "a reload that changes nothing must not invalidate generation-stamped products"
    );
    assert!(
        outcome.reparse_uris.is_empty(),
        "nothing parse-relevant changed, yet the reload invalidated {:?}",
        outcome.reparse_uris
    );
    assert!(
        !outcome.semantic_refresh_requested,
        "nothing token-relevant changed, yet the reload requested a semantic tokens refresh"
    );
    assert!(
        server
            .documents
            .get(uri)
            .is_some_and(|document| document.has_current_tree()),
        "the open document must keep its tree"
    );
}

fn assert_full_reload_work(outcome: &SettingsReloadOutcome, uri: &Url) {
    assert!(
        outcome.reparse_uris.contains(uri),
        "the reload must reparse the open document, got {:?}",
        outcome.reparse_uris
    );
    assert!(
        outcome.semantic_refresh_requested,
        "the reload must request a semantic tokens refresh"
    );
}

#[tokio::test]
async fn reload_changing_only_diagnostic_and_log_policy_neither_reparses_nor_refreshes() {
    let (service, client) = server_with_builtin_rust();
    let server = service.inner();
    server
        .apply_raw_settings(RawWorkspaceSettings::default(), baseline_settings())
        .await;
    let uri = open_and_wait_for_tree(server, "irrelevant.rs", "rust").await;

    let generation = server.cache.semantic_token_generation();
    let mut next = baseline_settings();
    next.diagnostics_debounce_ms += 250;
    next.features.window_log_message = LogMessageLevel::Warning;
    next.features.text_document_publish_diagnostics.debounce_ms += 1;
    next.features.workspace_diagnostic_refresh.max_wait_ms += 1;
    let outcome = server
        .apply_raw_settings(RawWorkspaceSettings::default(), next)
        .await;

    assert_no_reload_work(&outcome, server, &uri, generation);
    // Sanity: the settings themselves were applied.
    assert_eq!(
        server
            .settings_manager
            .load_settings()
            .features
            .window_log_message,
        LogMessageLevel::Warning
    );
    client.abort();
}

#[tokio::test]
async fn identical_reload_neither_reparses_nor_refreshes() {
    let (service, client) = server_with_builtin_rust();
    let server = service.inner();
    server
        .apply_raw_settings(RawWorkspaceSettings::default(), baseline_settings())
        .await;
    let uri = open_and_wait_for_tree(server, "identical.rs", "rust").await;
    let generation = server.cache.semantic_token_generation();

    let outcome = server
        .apply_raw_settings(RawWorkspaceSettings::default(), baseline_settings())
        .await;

    assert_no_reload_work(&outcome, server, &uri, generation);
    client.abort();
}

#[tokio::test]
async fn reload_changing_language_config_reparses_and_refreshes() {
    let (service, client) = server_with_builtin_rust();
    let server = service.inner();
    server
        .apply_raw_settings(RawWorkspaceSettings::default(), baseline_settings())
        .await;
    let uri = open_and_wait_for_tree(server, "relevant.rs", "rust").await;

    // A bridge filter changes nothing about the tree, but the reparse loop is
    // what re-drives injection processing and eager bridge opens.
    let mut next = baseline_settings();
    next.languages.insert(
        "rust".into(),
        LanguageSettings {
            bridge: Some(Default::default()),
            ..Default::default()
        },
    );
    let outcome = server
        .apply_raw_settings(RawWorkspaceSettings::default(), next)
        .await;

    assert_full_reload_work(&outcome, &uri);
    client.abort();
}

#[tokio::test]
async fn reload_changing_language_servers_reparses() {
    let (service, client) = server_with_builtin_rust();
    let server = service.inner();
    server
        .apply_raw_settings(RawWorkspaceSettings::default(), baseline_settings())
        .await;
    let uri = open_and_wait_for_tree(server, "servers.rs", "rust").await;

    let mut next = baseline_settings();
    next.language_servers.insert(
        "not-started".into(),
        crate::config::settings::BridgeServerConfig {
            cmd: Some(vec!["true".into()]),
            languages: Some(vec!["python".into()]),
            ..Default::default()
        },
    );
    let outcome = server
        .apply_raw_settings(RawWorkspaceSettings::default(), next)
        .await;

    // A new server must get the eager opens the reparse loop drives.
    assert_full_reload_work(&outcome, &uri);
    client.abort();
}

/// Capture mappings change no tree, but every semantic-tokens computation
/// reads them, and only a language reload's generation bump makes cached
/// tokens stale.
#[tokio::test]
async fn reload_changing_capture_mappings_reparses_and_refreshes() {
    let (service, client) = server_with_builtin_rust();
    let server = service.inner();
    server
        .apply_raw_settings(RawWorkspaceSettings::default(), baseline_settings())
        .await;
    let uri = open_and_wait_for_tree(server, "mappings.rs", "rust").await;

    let mut next = baseline_settings();
    next.capture_mappings.insert(
        "rust".into(),
        QueryTypeMappings {
            highlights: Some([("variable".to_string(), "".to_string())].into()),
        },
    );
    let outcome = server
        .apply_raw_settings(RawWorkspaceSettings::default(), next)
        .await;

    assert_full_reload_work(&outcome, &uri);
    client.abort();
}

/// The Lua grammar from the test grammar directory, or `None` (the test
/// skips) when it has not been built.
fn lua_parser() -> Option<PathBuf> {
    let grammars = std::env::var("TREE_SITTER_GRAMMARS").unwrap_or_else(|_| {
        std::env::current_dir()
            .unwrap()
            .join("deps/tree-sitter")
            .to_string_lossy()
            .into_owned()
    });
    let parser = Path::new(&grammars)
        .join("parser")
        .join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
    parser.exists().then_some(parser)
}

fn install_lua_parser(search_path: &Path, parser: &Path) {
    std::fs::create_dir_all(search_path.join("parser")).unwrap();
    std::fs::copy(
        parser,
        search_path
            .join("parser")
            .join(format!("lua.{}", std::env::consts::DLL_EXTENSION)),
    )
    .unwrap();
}

fn write_lua_highlights(search_path: &Path, query: &str) {
    let dir = search_path.join("queries").join("lua");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("highlights.scm"), query).unwrap();
}

fn search_path_settings(search_path: &Path) -> WorkspaceSettings {
    WorkspaceSettings {
        search_paths: vec![search_path.to_string_lossy().into_owned()],
        ..baseline_settings()
    }
}

/// The common zero-config shape: a language nobody configured, discovered on
/// the search paths when a document needed it. An identical reload re-reads
/// its files and, finding them unchanged, leaves the documents alone; an
/// identical reload after the query file was edited is how that edit reaches
/// the editor, so it must still reparse and refresh.
#[tokio::test]
async fn identical_reload_tracks_edits_to_a_discovered_language_query() {
    let Some(parser) = lua_parser() else {
        eprintln!("skipping: lua parser not built");
        return;
    };
    let search_path = tempfile::tempdir().unwrap();
    install_lua_parser(search_path.path(), &parser);
    write_lua_highlights(search_path.path(), "(identifier) @variable\n");
    let (service, client) = server_with_builtin_rust();
    let server = service.inner();
    let settings = search_path_settings(search_path.path());
    server
        .apply_raw_settings(RawWorkspaceSettings::default(), settings.clone())
        .await;
    let uri = open_and_wait_for_tree(server, "discovered.lua", "lua").await;
    assert!(server.language.has_queries("lua"));
    let generation = server.cache.semantic_token_generation();

    let unchanged = server
        .apply_raw_settings(RawWorkspaceSettings::default(), settings.clone())
        .await;
    assert_no_reload_work(&unchanged, server, &uri, generation);

    write_lua_highlights(
        search_path.path(),
        "(identifier) @variable\n(string) @string\n",
    );
    let edited = server
        .apply_raw_settings(RawWorkspaceSettings::default(), settings)
        .await;
    assert_full_reload_work(&edited, &uri);
    client.abort();
}

/// A parser dropped into a search path after a document failed to load its
/// language: an identical reload is what clears the cached failure, so it
/// must reparse the document that was waiting for that parser.
#[tokio::test]
async fn identical_reload_reparses_when_a_missing_parser_appeared() {
    let Some(parser) = lua_parser() else {
        eprintln!("skipping: lua parser not built");
        return;
    };
    let search_path = tempfile::tempdir().unwrap();
    let (service, client) = server_with_builtin_rust();
    let server = service.inner();
    let settings = search_path_settings(search_path.path());
    server
        .apply_raw_settings(RawWorkspaceSettings::default(), settings.clone())
        .await;
    let uri = Url::parse("file:///settings-reload/missing.lua").unwrap();
    server
        .did_open_impl(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: url_to_uri(&uri).unwrap(),
                language_id: "lua".into(),
                version: 1,
                text: "local x = 1\n".into(),
            },
        })
        .await;
    assert!(!server.language.has_parser_available("lua"));

    let generation = server.cache.semantic_token_generation();
    let still_missing = server
        .apply_raw_settings(RawWorkspaceSettings::default(), settings.clone())
        .await;
    assert!(still_missing.reparse_uris.is_empty());
    assert!(!still_missing.semantic_refresh_requested);
    assert_eq!(server.cache.semantic_token_generation(), generation);

    install_lua_parser(search_path.path(), &parser);
    let outcome = server
        .apply_raw_settings(RawWorkspaceSettings::default(), settings)
        .await;

    assert_full_reload_work(&outcome, &uri);
    client.abort();
}
