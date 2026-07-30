//! End-to-end tests for relative path resolution in configuration (issue #732).
//!
//! Every test here launches the server with a working directory that is *not*
//! the workspace it serves, which is the situation that made the bug visible: an
//! editor-spawned server inherits the editor's working directory, so a
//! documented project-local `./queries/highlights.scm` resolved somewhere that
//! depended on how the editor was started.
//!
//! Assertions read `kakehashi/internal/effectiveConfiguration`, which reports
//! settings after anchoring but before variable expansion. An anchored relative
//! path is therefore final there, while a `$`/`~` value still appears as the
//! user wrote it — which the last test pins.
//!
//! Run with: `cargo test --test e2e_config_relative_paths --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};
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
