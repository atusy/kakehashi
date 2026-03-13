// CLI integration tests for kakehashi
// Tests the command-line interface functionality

use std::process::Command;

/// Test that --help flag shows help message with program description
#[test]
fn test_help_flag_shows_help_message() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .arg("--help")
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should exit successfully
    assert!(output.status.success(), "Help should exit with success");

    // Should contain program name
    assert!(
        stdout.contains("kakehashi"),
        "Help should contain program name. Got: {}",
        stdout
    );

    // Should contain some description
    assert!(
        stdout.contains("Language Server") || stdout.contains("Tree-sitter"),
        "Help should contain description. Got: {}",
        stdout
    );

    // Should show language subcommand (hierarchical CLI)
    assert!(
        stdout.contains("language"),
        "Help should show language subcommand. Got: {}",
        stdout
    );
}

/// Test that language install --help shows usage with LANGUAGE argument
#[test]
fn test_install_help_shows_language_argument() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["language", "install", "--help"])
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should exit successfully
    assert!(
        output.status.success(),
        "Install help should exit with success"
    );

    // Should contain LANGUAGE or language reference
    assert!(
        stdout.to_lowercase().contains("language"),
        "Install help should mention language argument. Got: {}",
        stdout
    );
}

/// Test that language install command with unsupported language shows helpful error
#[test]
fn test_install_command_unsupported_language_shows_error() {
    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args([
            "language",
            "install",
            "nonexistent_language_xyz",
            "--data-dir",
            test_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute command");

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Should exit with failure for unsupported language
    assert!(
        !output.status.success(),
        "Install should exit with failure for unsupported language"
    );

    // Should contain error message about the language not being found
    assert!(
        stderr.to_lowercase().contains("not found")
            || stderr.to_lowercase().contains("not supported")
            || stderr.to_lowercase().contains("failed"),
        "Install should print helpful error for unsupported language. Got: {}",
        stderr
    );
}

/// Test that running with no arguments would start LSP server
/// (We can't fully test LSP startup without a proper client, but we can verify
/// the binary starts without errors when given empty stdin)
#[test]
fn test_no_args_does_not_show_help() {
    // When run with no args, it should NOT print help (it should try to start LSP)
    // We use timeout to prevent hanging on stdin read
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .stdin(std::process::Stdio::null())
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should NOT contain help output (would indicate wrong behavior)
    assert!(
        !stdout.contains("Usage:") || stdout.is_empty(),
        "No args should not print help (should try to start LSP). Got: {}",
        stdout
    );
}

/// Test that --version flag works
#[test]
fn test_version_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .arg("--version")
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should exit successfully
    assert!(output.status.success(), "Version should exit with success");

    // Should contain version number pattern
    assert!(
        stdout.contains("kakehashi") || stdout.contains("0."),
        "Version should show program name or version. Got: {}",
        stdout
    );
}

/// Test that language --help shows available actions
#[test]
fn test_language_help_shows_actions() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["language", "--help"])
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should exit successfully
    assert!(
        output.status.success(),
        "Language help should exit with success"
    );

    // Should contain install and list actions
    assert!(
        stdout.contains("install"),
        "Language help should show install action. Got: {}",
        stdout
    );
    assert!(
        stdout.contains("list"),
        "Language help should show list action. Got: {}",
        stdout
    );
}

/// Test that language list command works
#[test]
fn test_language_list_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["language", "list"])
        .output()
        .expect("Failed to execute command");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should exit successfully
    assert!(
        output.status.success(),
        "Language list should exit with success. stderr: {}",
        stderr
    );

    // Should contain some common languages
    let combined = format!("{}{}", stdout, stderr);
    assert!(
        combined.contains("lua") || combined.contains("rust") || combined.contains("python"),
        "Language list should show some languages. Got: {}",
        combined
    );
}

/// Test that config --help shows available actions
#[test]
fn test_config_help_shows_actions() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["config", "--help"])
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should exit successfully
    assert!(
        output.status.success(),
        "Config help should exit with success"
    );

    // Should contain init action
    assert!(
        stdout.contains("init"),
        "Config help should show init action. Got: {}",
        stdout
    );
}

/// Test that config init outputs to stdout by default
#[test]
fn test_config_init_outputs_to_stdout() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["config", "init"])
        .output()
        .expect("Failed to execute command");

    // Should exit successfully
    assert!(
        output.status.success(),
        "Config init should exit with success. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Stdout should contain the config template
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("autoInstall"),
        "Stdout should contain config template. Got: {}",
        stdout
    );
}

/// Test that config init includes captureMappings in output
#[test]
fn test_config_init_includes_capture_mappings() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["config", "init"])
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should contain captureMappings section
    assert!(
        stdout.contains("[captureMappings._.highlights]"),
        "Should contain captureMappings section. Got: {}",
        stdout
    );

    // Should contain variable mapping
    assert!(
        stdout.contains("\"variable\""),
        "Should contain variable mapping. Got: {}",
        stdout
    );
}

/// Test that config init --output creates a configuration file
#[test]
fn test_config_init_output_creates_file() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let config_path = test_dir.path().join("kakehashi.toml");

    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["config", "init", "--output", "kakehashi.toml"])
        .current_dir(test_dir.path())
        .output()
        .expect("Failed to execute command");

    // Should exit successfully
    assert!(
        output.status.success(),
        "Config init --output should exit with success. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // File should exist
    assert!(
        config_path.exists(),
        "Config file should be created at {}",
        config_path.display()
    );

    // File should contain expected options
    let content = fs::read_to_string(&config_path).expect("Failed to read config");
    assert!(
        content.contains("autoInstall"),
        "Config should contain expected options. Got: {}",
        content
    );
}

/// Test that config init --output does not overwrite existing file without --force
#[test]
fn test_config_init_output_no_overwrite_without_force() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let config_path = test_dir.path().join("kakehashi.toml");

    fs::write(&config_path, "existing").expect("Failed to write existing config");

    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["config", "init", "--output", "kakehashi.toml"])
        .current_dir(test_dir.path())
        .output()
        .expect("Failed to execute command");

    // Should exit with failure
    assert!(
        !output.status.success(),
        "Config init --output should fail when file exists"
    );

    // Original content should be preserved
    let content = fs::read_to_string(&config_path).expect("Failed to read config");
    assert_eq!(content, "existing", "Original content should be preserved");
}

/// Test that config init --output --force overwrites existing file
#[test]
fn test_config_init_output_force_overwrites() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let config_path = test_dir.path().join("kakehashi.toml");

    fs::write(&config_path, "existing").expect("Failed to write existing config");

    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["config", "init", "--output", "kakehashi.toml", "--force"])
        .current_dir(test_dir.path())
        .output()
        .expect("Failed to execute command");

    // Should exit successfully
    assert!(
        output.status.success(),
        "Config init --output --force should exit with success"
    );

    // Content should be replaced
    let content = fs::read_to_string(&config_path).expect("Failed to read config");
    assert!(
        !content.contains("existing"),
        "Content should be overwritten"
    );
    assert!(
        content.contains("autoInstall"),
        "New content should be present"
    );
}

/// Test that config init --force without --output warns to stderr
#[test]
fn test_config_init_force_without_output_warns() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["config", "init", "--force"])
        .output()
        .expect("Failed to execute command");

    // Should still exit successfully (warning is not fatal)
    assert!(
        output.status.success(),
        "Config init --force should exit with success"
    );

    // Should warn on stderr
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Warning") && stderr.contains("--force"),
        "Should warn about --force without --output. Got: {}",
        stderr
    );

    // Should still output to stdout
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("autoInstall"),
        "Should still output config to stdout. Got: {}",
        stdout
    );
}

/// Test that config init --output - outputs to stdout
#[test]
fn test_config_init_output_dash_outputs_to_stdout() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["config", "init", "--output", "-"])
        .output()
        .expect("Failed to execute command");

    // Should exit successfully
    assert!(
        output.status.success(),
        "Config init --output - should exit with success. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Stdout should contain the config template
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("autoInstall"),
        "Stdout should contain config template. Got: {}",
        stdout
    );
}

/// Test that config schema outputs valid JSON to stdout
#[test]
fn test_config_schema_outputs_valid_json_to_stdout() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["config", "schema"])
        .output()
        .expect("Failed to execute command");

    assert!(
        output.status.success(),
        "Config schema should exit with success. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let schema: serde_json::Value = serde_json::from_str(&stdout).expect("Should be valid JSON");
    // Should have properties with camelCase names
    let props = schema.get("properties").expect("Should have properties");
    assert!(
        props.get("searchPaths").is_some(),
        "Should have searchPaths property. Got: {}",
        stdout
    );
    assert!(
        props.get("autoInstall").is_some(),
        "Should have autoInstall property. Got: {}",
        stdout
    );
}

/// Test that config --help shows schema action
#[test]
fn test_config_help_shows_schema() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["config", "--help"])
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "Config help should exit with success"
    );

    assert!(
        stdout.contains("schema"),
        "Config help should show schema action. Got: {}",
        stdout
    );
}

/// Test that config schema --output creates a file
#[test]
fn test_config_schema_output_creates_file() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let schema_path = test_dir.path().join("schema.json");

    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["config", "schema", "--output", "schema.json"])
        .current_dir(test_dir.path())
        .output()
        .expect("Failed to execute command");

    assert!(
        output.status.success(),
        "Config schema --output should exit with success. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        schema_path.exists(),
        "Schema file should be created at {}",
        schema_path.display()
    );

    let content = fs::read_to_string(&schema_path).expect("Failed to read schema");
    let _: serde_json::Value =
        serde_json::from_str(&content).expect("File should contain valid JSON");
}

/// Test that config schema --output does not overwrite existing file without --force
#[test]
fn test_config_schema_output_no_overwrite_without_force() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let schema_path = test_dir.path().join("schema.json");

    fs::write(&schema_path, "existing").expect("Failed to write existing file");

    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["config", "schema", "--output", "schema.json"])
        .current_dir(test_dir.path())
        .output()
        .expect("Failed to execute command");

    assert!(
        !output.status.success(),
        "Config schema --output should fail when file exists"
    );

    let content = fs::read_to_string(&schema_path).expect("Failed to read schema");
    assert_eq!(content, "existing", "Original content should be preserved");
}

/// Test that config schema --output --force overwrites existing file
#[test]
fn test_config_schema_output_force_overwrites() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let schema_path = test_dir.path().join("schema.json");

    fs::write(&schema_path, "existing").expect("Failed to write existing file");

    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["config", "schema", "--output", "schema.json", "--force"])
        .current_dir(test_dir.path())
        .output()
        .expect("Failed to execute command");

    assert!(
        output.status.success(),
        "Config schema --output --force should exit with success"
    );

    let content = fs::read_to_string(&schema_path).expect("Failed to read schema");
    assert!(
        !content.contains("existing"),
        "Content should be overwritten"
    );
    let _: serde_json::Value =
        serde_json::from_str(&content).expect("Overwritten file should be valid JSON");
}

/// Test that config schema --force without --output warns to stderr
#[test]
fn test_config_schema_force_without_output_warns() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["config", "schema", "--force"])
        .output()
        .expect("Failed to execute command");

    assert!(
        output.status.success(),
        "Config schema --force should exit with success"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Warning") && stderr.contains("--force"),
        "Should warn about --force without --output. Got: {}",
        stderr
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let _: serde_json::Value =
        serde_json::from_str(&stdout).expect("Should still output valid JSON to stdout");
}

/// Test that config schema --output - outputs to stdout
#[test]
fn test_config_schema_output_dash_outputs_to_stdout() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["config", "schema", "--output", "-"])
        .output()
        .expect("Failed to execute command");

    assert!(
        output.status.success(),
        "Config schema --output - should exit with success. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let _: serde_json::Value =
        serde_json::from_str(&stdout).expect("Stdout should contain valid JSON");
}

/// Test that config schema output ends with a trailing newline
#[test]
fn test_config_schema_ends_with_trailing_newline() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["config", "schema"])
        .output()
        .expect("Failed to execute command");
    assert!(output.status.success());
    assert!(
        output.stdout.ends_with(b"\n"),
        "Schema output should end with a trailing newline"
    );
}

/// Test that language status --help shows expected options
#[test]
fn test_language_status_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["language", "status", "--help"])
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should exit successfully
    assert!(
        output.status.success(),
        "Language status help should exit with success"
    );

    // Should contain --data-dir and --verbose options
    assert!(
        stdout.contains("--data-dir"),
        "Status help should show --data-dir option. Got: {}",
        stdout
    );
    assert!(
        stdout.contains("--verbose") || stdout.contains("-v"),
        "Status help should show --verbose option. Got: {}",
        stdout
    );
}

/// Test that language status shows installed languages
#[test]
fn test_language_status_shows_installed() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");

    // Setup with a fake parser and queries
    fs::create_dir_all(test_dir.path().join("parser")).expect("Failed to create parser dir");
    fs::create_dir_all(test_dir.path().join("queries/testlang"))
        .expect("Failed to create queries dir");
    let ext = std::env::consts::DLL_EXTENSION;
    fs::write(
        test_dir.path().join(format!("parser/testlang.{ext}")),
        "fake",
    )
    .expect("Failed to write parser");
    fs::write(
        test_dir.path().join("queries/testlang/highlights.scm"),
        "(comment) @comment",
    )
    .expect("Failed to write query");

    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args([
            "language",
            "status",
            "--data-dir",
            test_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);

    // Should show testlang with parser and queries
    assert!(
        combined.contains("testlang"),
        "Status should show testlang. Got: {}",
        combined
    );
    assert!(
        combined.contains("✓ parser") && combined.contains("✓ queries"),
        "Status should show parser and queries as installed. Got: {}",
        combined
    );
}

/// Test that language status shows missing queries
#[test]
fn test_language_status_missing_queries() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");

    // Setup with parser only (no queries)
    fs::create_dir_all(test_dir.path().join("parser")).expect("Failed to create parser dir");
    let ext = std::env::consts::DLL_EXTENSION;
    fs::write(
        test_dir.path().join(format!("parser/incomplete.{ext}")),
        "fake",
    )
    .expect("Failed to write parser");

    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args([
            "language",
            "status",
            "--data-dir",
            test_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);

    // Should show incomplete with missing queries
    assert!(
        combined.contains("incomplete"),
        "Status should show incomplete. Got: {}",
        combined
    );
    assert!(
        combined.contains("missing"),
        "Status should indicate missing queries. Got: {}",
        combined
    );
}

/// Test that language uninstall --help shows expected options
#[test]
fn test_language_uninstall_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["language", "uninstall", "--help"])
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should exit successfully
    assert!(
        output.status.success(),
        "Language uninstall help should exit with success"
    );

    // Should contain --force and --all options
    assert!(
        stdout.contains("--force"),
        "Uninstall help should show --force option. Got: {}",
        stdout
    );
    assert!(
        stdout.contains("--all"),
        "Uninstall help should show --all option. Got: {}",
        stdout
    );
}

/// Test that language uninstall removes parser and queries
#[test]
fn test_language_uninstall_removes_files() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");

    // Setup
    fs::create_dir_all(test_dir.path().join("parser")).expect("Failed to create parser dir");
    fs::create_dir_all(test_dir.path().join("queries/testlang"))
        .expect("Failed to create queries dir");
    let ext = std::env::consts::DLL_EXTENSION;
    fs::write(
        test_dir.path().join(format!("parser/testlang.{ext}")),
        "fake",
    )
    .expect("Failed to write parser");
    fs::write(
        test_dir.path().join("queries/testlang/highlights.scm"),
        "(comment) @comment",
    )
    .expect("Failed to write query");

    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args([
            "language",
            "uninstall",
            "testlang",
            "--data-dir",
            test_dir.path().to_str().unwrap(),
            "--force",
        ])
        .output()
        .expect("Failed to execute command");

    // Should exit successfully
    assert!(
        output.status.success(),
        "Uninstall should exit with success. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Parser should be removed
    assert!(
        !test_dir
            .path()
            .join(format!("parser/testlang.{ext}"))
            .exists(),
        "Parser should be removed"
    );

    // Queries should be removed
    assert!(
        !test_dir.path().join("queries/testlang").exists(),
        "Queries directory should be removed"
    );
}

/// Test that language uninstall --all removes all languages
#[test]
fn test_language_uninstall_all() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");

    // Setup multiple languages
    fs::create_dir_all(test_dir.path().join("parser")).expect("Failed to create parser dir");
    fs::create_dir_all(test_dir.path().join("queries/lang1"))
        .expect("Failed to create queries dir");
    fs::create_dir_all(test_dir.path().join("queries/lang2"))
        .expect("Failed to create queries dir");
    let ext = std::env::consts::DLL_EXTENSION;
    fs::write(test_dir.path().join(format!("parser/lang1.{ext}")), "fake")
        .expect("Failed to write parser");
    fs::write(test_dir.path().join(format!("parser/lang2.{ext}")), "fake")
        .expect("Failed to write parser");
    fs::write(
        test_dir.path().join("queries/lang1/highlights.scm"),
        "(comment) @comment",
    )
    .expect("Failed to write query");
    fs::write(
        test_dir.path().join("queries/lang2/highlights.scm"),
        "(comment) @comment",
    )
    .expect("Failed to write query");

    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args([
            "language",
            "uninstall",
            "--all",
            "--data-dir",
            test_dir.path().to_str().unwrap(),
            "--force",
        ])
        .output()
        .expect("Failed to execute command");

    // Should exit successfully
    assert!(
        output.status.success(),
        "Uninstall --all should exit with success. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // All parsers should be removed
    let parsers: Vec<_> = fs::read_dir(test_dir.path().join("parser"))
        .map(|entries| entries.filter_map(|e| e.ok()).collect())
        .unwrap_or_default();
    assert!(parsers.is_empty(), "All parsers should be removed");

    // All queries should be removed
    let queries: Vec<_> = fs::read_dir(test_dir.path().join("queries"))
        .map(|entries| entries.filter_map(|e| e.ok()).collect())
        .unwrap_or_default();
    assert!(queries.is_empty(), "All queries should be removed");
}

/// Test that --data-dir works as a top-level flag (before subcommand)
#[test]
fn test_data_dir_top_level_flag() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");

    // Setup with a fake parser and queries
    fs::create_dir_all(test_dir.path().join("parser")).expect("Failed to create parser dir");
    fs::create_dir_all(test_dir.path().join("queries/testlang"))
        .expect("Failed to create queries dir");
    let ext = std::env::consts::DLL_EXTENSION;
    fs::write(
        test_dir.path().join(format!("parser/testlang.{ext}")),
        "fake",
    )
    .expect("Failed to write parser");
    fs::write(
        test_dir.path().join("queries/testlang/highlights.scm"),
        "(comment) @comment",
    )
    .expect("Failed to write query");

    // Use --data-dir BEFORE the subcommand (top-level position)
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args([
            "--data-dir",
            test_dir.path().to_str().unwrap(),
            "language",
            "status",
        ])
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);

    // Should exit successfully
    assert!(
        output.status.success(),
        "Top-level --data-dir should work with language status. stderr: {}",
        stderr
    );

    // Should show testlang
    assert!(
        combined.contains("testlang"),
        "Status should show testlang with top-level --data-dir. Got: {}",
        combined
    );
}

/// Test that language uninstall cancels without --force when user enters 'n'
#[test]
fn test_language_uninstall_cancel() {
    use std::fs;
    use std::process::Stdio;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");

    // Setup
    fs::create_dir_all(test_dir.path().join("parser")).expect("Failed to create parser dir");
    let ext = std::env::consts::DLL_EXTENSION;
    fs::write(
        test_dir.path().join(format!("parser/testlang.{ext}")),
        "fake",
    )
    .expect("Failed to write parser");

    let mut child = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args([
            "language",
            "uninstall",
            "testlang",
            "--data-dir",
            test_dir.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn command");

    // Write 'n' to stdin
    use std::io::Write;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(b"n\n").expect("Failed to write to stdin");
    }

    let output = child
        .wait_with_output()
        .expect("Failed to wait for command");

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Should contain "Cancelled"
    assert!(
        stderr.to_lowercase().contains("cancel"),
        "Should show cancelled message. Got: {}",
        stderr
    );

    // Parser should still exist
    assert!(
        test_dir
            .path()
            .join(format!("parser/testlang.{ext}"))
            .exists(),
        "Parser should still exist after cancellation"
    );
}
