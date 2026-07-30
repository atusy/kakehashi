//! End-to-end tests for relative path resolution in configuration (issue #732).
//!
//! Every test here launches the server with a working directory that is *not*
//! the workspace it serves, which is the situation that made the bug visible: an
//! editor-spawned server inherits the editor's working directory, so a
//! documented project-local `./queries/highlights.scm` resolved somewhere that
//! depended on how the editor was started.
//!
//! Most assertions read `kakehashi/internal/effectiveConfiguration`, which
//! reports settings after anchoring but before variable expansion. An anchored
//! relative path is therefore final there, while a `$`/`~` value still appears
//! as the user wrote it.
//!
//! Reported configuration is not proof the path is *usable*, though, so the last
//! test drives a project-local parser and query all the way to a semantic-token
//! response — the symptom #732 was actually reported as.
//!
//! Run with: `cargo test --test e2e_config_relative_paths --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use tempfile::TempDir;

/// Initialize against `root` and return the effective raw settings.
fn effective_settings(
    client: &mut LspClient,
    root: &std::path::Path,
    init_options: Value,
) -> Value {
    let mut params = json!({
        "processId": std::process::id(),
        "rootUri": format!("file://{}", root.display()),
        "capabilities": {}
    });
    if !init_options.is_null() {
        params["initializationOptions"] = init_options;
    }
    let _init = client.send_request("initialize", params);
    client.send_notification("initialized", json!({}));

    client
        .send_request("kakehashi/internal/effectiveConfiguration", json!({}))
        .get("result")
        .expect("effectiveConfiguration should return a result")
        .get("settings")
        .expect("result should contain settings")
        .clone()
}

fn search_paths(settings: &Value) -> Vec<String> {
    settings["searchPaths"]
        .as_array()
        .expect("searchPaths should be an array")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("searchPath should be a string")
                .to_string()
        })
        .collect()
}

/// The reproduction from issue #732: a project config naming project-local
/// assets, served to a client whose server was launched somewhere else.
#[test]
fn project_config_paths_resolve_against_the_project_not_the_launch_directory() {
    let project = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    std::fs::write(
        project.path().join("kakehashi.toml"),
        "searchPaths = [\"./runtime\"]\n\
         [languages.test]\n\
         parser = \"./parsers/test.so\"\n\
         queries = [{ path = \"./queries/highlights.scm\", kind = \"highlights\" }]\n",
    )
    .unwrap();

    let mut client = LspClient::builder()
        .current_dir(elsewhere.path())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();
    let settings = effective_settings(&mut client, project.path(), json!(null));

    assert!(
        search_paths(&settings).contains(
            &project
                .path()
                .join("runtime")
                .to_string_lossy()
                .into_owned()
        ),
        "searchPaths should name the project's own runtime dir: {:?}",
        search_paths(&settings)
    );
    assert_eq!(
        settings["languages"]["test"]["parser"].as_str(),
        Some(
            project
                .path()
                .join("parsers/test.so")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert_eq!(
        settings["languages"]["test"]["queries"][0]["path"].as_str(),
        Some(
            project
                .path()
                .join("queries/highlights.scm")
                .to_string_lossy()
                .as_ref()
        )
    );
}

/// A client-supplied relative path is workspace-local: the client knows the
/// workspace it opened, not where the server process was started.
#[test]
fn initialization_option_paths_resolve_against_the_workspace_root() {
    let project = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();

    let mut client = LspClient::builder()
        .current_dir(elsewhere.path())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();
    let settings = effective_settings(
        &mut client,
        project.path(),
        json!({ "searchPaths": ["./runtime"] }),
    );

    assert_eq!(
        search_paths(&settings),
        vec![
            project
                .path()
                .join("runtime")
                .to_string_lossy()
                .into_owned()
        ]
    );
}

/// Each `--config-file` layer anchors to its own directory, so two files that
/// both say `./runtime` name two different directories — and the later layer
/// still wins on precedence.
#[test]
fn each_explicit_config_file_anchors_to_its_own_directory() {
    let base_dir = TempDir::new().unwrap();
    let overlay_dir = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    let base = base_dir.path().join("base.toml");
    let overlay = overlay_dir.path().join("overlay.toml");
    std::fs::write(
        &base,
        "searchPaths = [\"./runtime\"]\n[languages.test]\nparser = \"./parsers/test.so\"\n",
    )
    .unwrap();
    std::fs::write(&overlay, "searchPaths = [\"./runtime\"]\n").unwrap();

    let mut client = LspClient::builder()
        .current_dir(elsewhere.path())
        .arg("--config-file")
        .arg(base.to_str().unwrap())
        .arg("--config-file")
        .arg(overlay.to_str().unwrap())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();
    let project = TempDir::new().unwrap();
    let settings = effective_settings(&mut client, project.path(), json!(null));

    assert_eq!(
        search_paths(&settings),
        vec![
            overlay_dir
                .path()
                .join("runtime")
                .to_string_lossy()
                .into_owned()
        ],
        "the overlay's searchPaths replace the base's, anchored to the overlay's own directory"
    );
    assert_eq!(
        settings["languages"]["test"]["parser"].as_str(),
        Some(
            base_dir
                .path()
                .join("parsers/test.so")
                .to_string_lossy()
                .as_ref()
        ),
        "a field only the base layer supplies keeps the base layer's directory"
    );
}

/// Anchoring runs ahead of expansion and must not consume its syntax: a
/// `$VAR`-led value is reported as written, and `~` still means the home
/// directory rather than a directory named `~` under the workspace.
#[test]
fn expansion_syntax_is_left_for_the_expansion_pass() {
    let project = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    std::fs::write(
        project.path().join("kakehashi.toml"),
        "searchPaths = [\"${KAKEHASHI_DATA_DIR}\", \"~/parsers\", \"./runtime\"]\n",
    )
    .unwrap();

    let mut client = LspClient::builder().current_dir(elsewhere.path()).build();
    let settings = effective_settings(&mut client, project.path(), json!(null));

    assert_eq!(
        search_paths(&settings),
        vec![
            "${KAKEHASHI_DATA_DIR}".to_string(),
            "~/parsers".to_string(),
            project
                .path()
                .join("runtime")
                .to_string_lossy()
                .into_owned(),
        ],
        "only the relative value is rebased; the other two reach expansion intact"
    );
}

/// The shared data dir the rest of the e2e suite uses, with the test parsers
/// already installed. Read-only once built, so parallel test processes share it.
fn installed_data_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = kakehashi::install::test_support::test_data_dir_path();
        std::fs::create_dir_all(&dir).expect("create shared test data dir");
        kakehashi::install::test_support::ensure_test_languages_installed(&dir)
            .expect("install test parsers into the shared data dir");
        dir
    })
    .as_path()
}

/// The installed Lua parser's filename, which is platform-specific.
fn parser_library_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "lua.dylib"
    } else if cfg!(windows) {
        "lua.dll"
    } else {
        "lua.so"
    }
}

/// The reported configuration is only half the claim. This drives a
/// project-local parser and highlights query all the way to a semantic-token
/// response, so the anchored path is proven to reach `dlopen` and `read` — the
/// symptom #732 was reported as ("project-local custom parsers and queries can
/// be reported missing even though they exist next to the project config").
///
/// `searchPaths` is emptied so nothing can be found by the fallback discovery
/// route: if anchoring were wrong, there is no second way for the server to
/// locate these files, and the token list comes back empty.
#[test]
fn a_project_local_parser_and_query_are_actually_loaded() {
    let installed = installed_data_dir();
    let project = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();

    let vendor = project.path().join("vendor");
    std::fs::create_dir_all(&vendor).unwrap();
    std::fs::copy(
        installed.join("parser").join(parser_library_name()),
        vendor.join(parser_library_name()),
    )
    .expect("copy the installed lua parser into the project");
    std::fs::copy(
        installed.join("queries").join("lua").join("highlights.scm"),
        vendor.join("highlights.scm"),
    )
    .expect("copy the installed lua highlights query into the project");

    std::fs::write(
        project.path().join("kakehashi.toml"),
        format!(
            "autoInstall = false\n\
             searchPaths = []\n\
             [languages.lua]\n\
             parser = \"./vendor/{}\"\n\
             queries = [{{ path = \"./vendor/highlights.scm\", kind = \"highlights\" }}]\n",
            parser_library_name()
        ),
    )
    .unwrap();

    let source = project.path().join("main.lua");
    std::fs::write(&source, "local x = 1\n").unwrap();

    let mut client = LspClient::builder()
        .current_dir(elsewhere.path())
        .env("KAKEHASHI_DATA_DIR", installed.to_str().unwrap())
        .build();
    let init = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": format!("file://{}", project.path().display()),
            "capabilities": {
                "textDocument": {
                    "semanticTokens": {
                        "requests": { "full": true },
                        "tokenTypes": ["keyword", "variable", "function"],
                        "tokenModifiers": [],
                        "formats": ["relative"]
                    }
                }
            }
        }),
    );
    assert!(
        init.get("error").is_none(),
        "initialize should succeed: {init:?}"
    );
    client.send_notification("initialized", json!({}));

    let uri = format!("file://{}", source.display());
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "lua",
                "version": 1,
                "text": "local x = 1\n"
            }
        }),
    );
    std::thread::sleep(Duration::from_millis(500));

    let response = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );
    let data = response
        .get("result")
        .and_then(|result| result.get("data"))
        .and_then(Value::as_array)
        .expect("semanticTokens/full should return token data");

    assert!(
        !data.is_empty(),
        "a project-local parser and query must produce tokens; an empty list \
         means the anchored paths never reached the filesystem: {response:?}"
    );
}
