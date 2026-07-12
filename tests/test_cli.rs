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

/// Test that config init documents the per-root-instance default by emitting
/// `preferSharedInstance = false` under the `languageServers._` wildcard, so
/// the opt-in (#391) is discoverable in the generated template.
#[test]
fn test_config_init_includes_prefer_shared_instance() {
    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(["config", "init"])
        .output()
        .expect("Failed to execute command");

    assert!(
        output.status.success(),
        "config init should exit successfully. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("[languageServers._]"),
        "Should contain languageServers wildcard section. Got: {}",
        stdout
    );
    assert!(
        stdout.contains("preferSharedInstance = false"),
        "Should document the per-root-instance default. Got: {}",
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

/// Test that language status treats zero-byte unmarked queries as incomplete
#[test]
fn test_language_status_zero_byte_unmarked_queries_missing() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");

    fs::create_dir_all(test_dir.path().join("parser")).expect("Failed to create parser dir");
    fs::create_dir_all(test_dir.path().join("queries/partial"))
        .expect("Failed to create queries dir");
    let ext = std::env::consts::DLL_EXTENSION;
    fs::write(
        test_dir.path().join(format!("parser/partial.{ext}")),
        "fake",
    )
    .expect("Failed to write parser");
    fs::write(test_dir.path().join("queries/partial/highlights.scm"), "")
        .expect("Failed to write empty query");

    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args([
            "language",
            "status",
            "--data-dir",
            test_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute command");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        combined.contains("partial") && combined.contains("missing"),
        "Status should match installer completeness for zero-byte unmarked queries. Got: {combined}"
    );
}

/// Test that language status ignores internal query staging directories
#[test]
fn test_language_status_ignores_internal_query_dirs() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    fs::create_dir_all(test_dir.path().join("queries/.testlang.123.tmp"))
        .expect("Failed to create internal query dir");
    fs::write(
        test_dir
            .path()
            .join("queries/.testlang.123.tmp/highlights.scm"),
        "(comment) @comment",
    )
    .expect("Failed to write internal query");

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

    assert!(
        !combined.contains(".testlang.123.tmp"),
        "Status must not report internal query dirs as languages. Got: {combined}"
    );
    assert!(
        combined.contains("No languages installed"),
        "Only internal query dirs should not count as installed languages. Got: {combined}"
    );
}

/// Test that language status recovers a query dir stranded as a hidden backup
#[test]
fn test_language_status_recovers_internal_query_backup() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    fs::create_dir_all(test_dir.path().join("parser")).expect("Failed to create parser dir");
    fs::create_dir_all(test_dir.path().join("queries/.recover_lang.123.0.backup"))
        .expect("Failed to create backup query dir");
    let ext = std::env::consts::DLL_EXTENSION;
    fs::write(
        test_dir.path().join(format!("parser/recover_lang.{ext}")),
        "fake",
    )
    .expect("Failed to write parser");
    fs::write(
        test_dir
            .path()
            .join("queries/.recover_lang.123.0.backup/highlights.scm"),
        "(comment) @comment",
    )
    .expect("Failed to write backup query");
    fs::write(
        test_dir
            .path()
            .join("queries/.recover_lang.123.0.backup/.kakehashi-install-complete"),
        "ok\n",
    )
    .expect("Failed to write backup marker");
    fs::write(
        test_dir
            .path()
            .join("queries/.recover_lang.123.0.backup.kakehashi-backup"),
        "ok\n",
    )
    .expect("Failed to write backup ownership marker");

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

    assert!(
        output.status.success(),
        "Status should exit with success. stderr: {stderr}"
    );
    assert!(
        combined.contains("recover_lang") && combined.contains("✓ queries"),
        "Status should report recovered queries. Got: {combined}"
    );
    assert!(
        test_dir.path().join("queries/recover_lang").exists(),
        "Backup query dir should be restored to the canonical path"
    );
    assert!(
        !test_dir
            .path()
            .join("queries/.recover_lang.123.0.backup")
            .exists(),
        "Recovered backup dir should not remain stranded"
    );
}

/// Test that status recovery does not delete an incomplete canonical query dir
#[test]
fn test_language_status_preserves_incomplete_query_dir_when_backup_exists() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    fs::create_dir_all(test_dir.path().join("parser")).expect("Failed to create parser dir");
    fs::create_dir_all(test_dir.path().join("queries/recover_lang"))
        .expect("Failed to create canonical query dir");
    fs::create_dir_all(test_dir.path().join("queries/.recover_lang.123.0.backup"))
        .expect("Failed to create backup query dir");
    let ext = std::env::consts::DLL_EXTENSION;
    fs::write(
        test_dir.path().join(format!("parser/recover_lang.{ext}")),
        "fake",
    )
    .expect("Failed to write parser");
    fs::write(
        test_dir.path().join("queries/recover_lang/bindings.scm"),
        "user managed query",
    )
    .expect("Failed to write canonical query");
    fs::write(
        test_dir
            .path()
            .join("queries/.recover_lang.123.0.backup/highlights.scm"),
        "(comment) @comment",
    )
    .expect("Failed to write backup query");
    fs::write(
        test_dir
            .path()
            .join("queries/.recover_lang.123.0.backup/.kakehashi-install-complete"),
        "ok\n",
    )
    .expect("Failed to write backup marker");
    fs::write(
        test_dir
            .path()
            .join("queries/.recover_lang.123.0.backup.kakehashi-backup"),
        "ok\n",
    )
    .expect("Failed to write backup ownership marker");

    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args([
            "language",
            "status",
            "--data-dir",
            test_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute command");

    assert!(
        output.status.success(),
        "Status should exit with success. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(test_dir.path().join("queries/recover_lang/bindings.scm")).unwrap(),
        "user managed query",
        "Status recovery must not delete incomplete canonical query dirs"
    );
    assert!(
        test_dir
            .path()
            .join("queries/.recover_lang.123.0.backup")
            .exists(),
        "Backup should remain when canonical query dir already exists"
    );
}

/// Test that status does not recover user-created hidden backup directories
#[test]
fn test_language_status_ignores_manual_query_backup() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    fs::create_dir_all(test_dir.path().join("parser")).expect("Failed to create parser dir");
    fs::create_dir_all(test_dir.path().join("queries/.recover_lang.manual.backup"))
        .expect("Failed to create manual backup query dir");
    let ext = std::env::consts::DLL_EXTENSION;
    fs::write(
        test_dir.path().join(format!("parser/recover_lang.{ext}")),
        "fake",
    )
    .expect("Failed to write parser");
    fs::write(
        test_dir
            .path()
            .join("queries/.recover_lang.manual.backup/highlights.scm"),
        "(comment) @comment",
    )
    .expect("Failed to write manual backup query");

    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args([
            "language",
            "status",
            "--data-dir",
            test_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute command");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        combined.contains("recover_lang") && combined.contains("missing"),
        "Manual backup should not be restored as installed queries. Got: {combined}"
    );
    assert!(
        !test_dir.path().join("queries/recover_lang").exists(),
        "Manual backup must not be moved to the canonical query path"
    );
    assert!(
        test_dir
            .path()
            .join("queries/.recover_lang.manual.backup")
            .exists(),
        "Manual backup should be left untouched"
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

    // All installed query directories should be removed; internal tombstone/lock
    // files may remain hidden under queries/.
    let queries: Vec<_> = fs::read_dir(test_dir.path().join("queries"))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_none_or(|name| !name.starts_with('.'))
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(queries.is_empty(), "All queries should be removed");
}

#[cfg(unix)]
fn assert_uninstall_all_fails_for_unreadable_dir(dir_name: &str) {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let install_dir = test_dir.path().join(dir_name);
    if dir_name == "parser" {
        fs::create_dir_all(&install_dir).expect("Failed to create parser dir");
        fs::write(
            install_dir.join(format!("testlang.{}", std::env::consts::DLL_EXTENSION)),
            "fake",
        )
        .expect("Failed to write parser");
    } else {
        fs::create_dir_all(install_dir.join("testlang")).expect("Failed to create queries dir");
        fs::write(
            install_dir.join("testlang/highlights.scm"),
            "(comment) @comment",
        )
        .expect("Failed to write query");
    }
    fs::set_permissions(&install_dir, fs::Permissions::from_mode(0o000))
        .expect("Failed to make install dir unreadable");

    // Elevated test environments may retain permission to read mode-000 paths.
    // Skip there rather than asserting a failure the OS cannot produce.
    if fs::read_dir(&install_dir).is_ok() {
        fs::set_permissions(&install_dir, fs::Permissions::from_mode(0o700))
            .expect("Failed to restore install dir permissions");
        return;
    }

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

    fs::set_permissions(&install_dir, fs::Permissions::from_mode(0o700))
        .expect("Failed to restore install dir permissions");

    assert!(
        !output.status.success(),
        "unreadable install state must fail instead of reporting success; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Failed to scan installed languages"),
        "stderr should identify the failed install scan: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `--all` must not claim success when either asset root cannot be enumerated.
#[cfg(unix)]
#[test]
fn test_language_uninstall_all_fails_for_unreadable_install_dirs() {
    assert_uninstall_all_fails_for_unreadable_dir("parser");
    assert_uninstall_all_fails_for_unreadable_dir("queries");
}

#[cfg(unix)]
#[test]
fn test_language_uninstall_all_fails_for_dangling_install_dir() {
    use std::os::unix::fs::symlink;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    symlink("missing-parser-target", test_dir.path().join("parser"))
        .expect("Failed to create dangling parser link");

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

    assert!(
        !output.status.success(),
        "a dangling install root must fail; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn test_language_uninstall_all_rejects_symlinked_install_roots() {
    use std::fs;
    use std::os::unix::fs::symlink;

    for root_name in ["parser", "queries"] {
        let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let data_dir = test_dir.path().join("data");
        let outside = test_dir.path().join("outside");
        fs::create_dir_all(&data_dir).expect("Failed to create data dir");
        let asset = if root_name == "parser" {
            fs::create_dir_all(&outside).expect("Failed to create outside parser dir");
            let path = outside.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
            fs::write(&path, "fake").expect("Failed to write outside parser");
            path
        } else {
            let path = outside.join("lua/highlights.scm");
            fs::create_dir_all(path.parent().unwrap()).expect("Failed to create outside query dir");
            fs::write(&path, "(comment) @comment").expect("Failed to write outside query");
            path
        };
        symlink(&outside, data_dir.join(root_name)).expect("Failed to link install root");

        let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
            .args([
                "language",
                "uninstall",
                "--all",
                "--data-dir",
                data_dir.to_str().unwrap(),
                "--force",
            ])
            .output()
            .expect("Failed to execute command");

        assert!(
            !output.status.success(),
            "symlinked {root_name} root must fail; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            asset.exists(),
            "outside {root_name} asset must remain untouched"
        );
    }
}

#[cfg(unix)]
#[test]
fn test_language_uninstall_rejects_symlinked_install_roots() {
    use std::fs;
    use std::os::unix::fs::symlink;

    for root_name in ["parser", "queries"] {
        let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let data_dir = test_dir.path().join("data");
        let outside = test_dir.path().join("outside");
        fs::create_dir_all(&data_dir).expect("Failed to create data dir");
        let asset = if root_name == "parser" {
            fs::create_dir_all(&outside).expect("Failed to create outside parser dir");
            let path = outside.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
            fs::write(&path, "fake").expect("Failed to write outside parser");
            path
        } else {
            let path = outside.join("lua/highlights.scm");
            fs::create_dir_all(path.parent().unwrap()).expect("Failed to create outside query dir");
            fs::write(&path, "(comment) @comment").expect("Failed to write outside query");
            path
        };
        symlink(&outside, data_dir.join(root_name)).expect("Failed to link install root");

        let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
            .args([
                "language",
                "uninstall",
                "lua",
                "--data-dir",
                data_dir.to_str().unwrap(),
                "--force",
            ])
            .output()
            .expect("Failed to execute command");

        assert!(
            !output.status.success(),
            "targeted uninstall must reject symlinked {root_name}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            asset.exists(),
            "outside {root_name} asset must remain untouched"
        );
    }
}

#[cfg(unix)]
#[test]
fn test_language_uninstall_all_removes_dangling_parser_entry() {
    use std::fs;
    use std::os::unix::fs::symlink;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let parser_dir = test_dir.path().join("parser");
    fs::create_dir_all(&parser_dir).expect("Failed to create parser dir");
    let parser = parser_dir.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
    symlink("missing-parser-library", &parser).expect("Failed to create dangling parser link");

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

    assert!(
        output.status.success(),
        "dangling parser entry should be removable; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::symlink_metadata(&parser).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "dangling parser entry must be removed"
    );
}

#[cfg(unix)]
#[test]
fn test_language_uninstall_all_removes_query_symlink_entry() {
    use std::fs;
    use std::os::unix::fs::symlink;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let parser_dir = test_dir.path().join("parser");
    let queries_dir = test_dir.path().join("queries");
    fs::create_dir_all(&parser_dir).expect("Failed to create parser dir");
    fs::create_dir_all(&queries_dir).expect("Failed to create queries dir");
    let parser = parser_dir.join(format!("testlang.{}", std::env::consts::DLL_EXTENSION));
    fs::write(&parser, "fake").expect("Failed to write parser");
    symlink("loop", queries_dir.join("loop")).expect("Failed to create query symlink loop");

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

    assert!(
        output.status.success(),
        "query symlink should be removable without resolving its target; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!parser.exists(), "the discovered parser must be removed");
    assert!(
        fs::symlink_metadata(queries_dir.join("loop"))
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "query symlink entry must be removed"
    );
}

#[cfg(unix)]
#[test]
fn test_language_uninstall_all_preflights_before_query_recovery() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let parser_dir = test_dir.path().join("parser");
    let stranded_tmp = test_dir.path().join("queries/.lua.4294967295.0.tmp");
    fs::create_dir_all(&parser_dir).expect("Failed to create parser dir");
    fs::create_dir_all(&stranded_tmp).expect("Failed to create stranded query temp dir");
    fs::set_permissions(&parser_dir, fs::Permissions::from_mode(0o000))
        .expect("Failed to make parser dir unreadable");
    if fs::read_dir(&parser_dir).is_ok() {
        fs::set_permissions(&parser_dir, fs::Permissions::from_mode(0o700))
            .expect("Failed to restore parser dir permissions");
        return;
    }

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

    fs::set_permissions(&parser_dir, fs::Permissions::from_mode(0o700))
        .expect("Failed to restore parser dir permissions");
    assert!(!output.status.success(), "unreadable preflight must fail");
    assert!(
        stranded_tmp.exists(),
        "query recovery must not mutate state before all install roots pass preflight"
    );
}

#[cfg(unix)]
#[test]
fn test_language_uninstall_all_fails_when_query_recovery_fails() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let queries_dir = test_dir.path().join("queries");
    let stranded_tmp = queries_dir.join(".lua.4294967295.0.tmp");
    fs::create_dir_all(&stranded_tmp).expect("Failed to create stranded query temp dir");
    fs::set_permissions(&queries_dir, fs::Permissions::from_mode(0o500))
        .expect("Failed to make queries dir read-only");

    // Elevated environments may still create the recovery lock.
    let probe = queries_dir.join("permission-probe");
    if fs::write(&probe, "probe").is_ok() {
        let _ = fs::remove_file(probe);
        fs::set_permissions(&queries_dir, fs::Permissions::from_mode(0o700))
            .expect("Failed to restore queries dir permissions");
        return;
    }

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

    fs::set_permissions(&queries_dir, fs::Permissions::from_mode(0o700))
        .expect("Failed to restore queries dir permissions");
    assert!(
        !output.status.success(),
        "failed recovery must fail --all; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stranded_tmp.exists(), "failed recovery must stay visible");
}

#[cfg(unix)]
#[test]
fn test_language_uninstall_all_preflights_query_contents_before_parser_removal() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let parser_dir = test_dir.path().join("parser");
    let query_dir = test_dir.path().join("queries/lua");
    fs::create_dir_all(&parser_dir).expect("Failed to create parser dir");
    fs::create_dir_all(&query_dir).expect("Failed to create query dir");
    let parser = parser_dir.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
    fs::write(&parser, "fake").expect("Failed to write parser");
    fs::write(query_dir.join("highlights.scm"), "(comment) @comment")
        .expect("Failed to write query");
    fs::set_permissions(&query_dir, fs::Permissions::from_mode(0o000))
        .expect("Failed to make query dir unreadable");
    if fs::read_dir(&query_dir).is_ok() {
        fs::set_permissions(&query_dir, fs::Permissions::from_mode(0o700))
            .expect("Failed to restore query dir permissions");
        return;
    }

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

    fs::set_permissions(&query_dir, fs::Permissions::from_mode(0o700))
        .expect("Failed to restore query dir permissions");
    assert!(!output.status.success(), "unreadable query tree must fail");
    assert!(
        parser.exists(),
        "query preflight failure must happen before parser removal"
    );
}

#[cfg(unix)]
#[test]
fn test_language_uninstall_all_query_removal_failure_preserves_parser() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let parser_dir = test_dir.path().join("parser");
    let query_dir = test_dir.path().join("queries/lua");
    fs::create_dir_all(&parser_dir).expect("Failed to create parser dir");
    fs::create_dir_all(&query_dir).expect("Failed to create query dir");
    let parser = parser_dir.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
    fs::write(&parser, "fake").expect("Failed to write parser");
    fs::write(query_dir.join("highlights.scm"), "(comment) @comment")
        .expect("Failed to write query");
    fs::set_permissions(&query_dir, fs::Permissions::from_mode(0o500))
        .expect("Failed to make query dir non-writable");
    let probe = query_dir.join("permission-probe");
    if fs::write(&probe, "probe").is_ok() {
        let _ = fs::remove_file(probe);
        fs::set_permissions(&query_dir, fs::Permissions::from_mode(0o700))
            .expect("Failed to restore query dir permissions");
        return;
    }

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

    fs::set_permissions(&query_dir, fs::Permissions::from_mode(0o700))
        .expect("Failed to restore query dir permissions");
    assert!(!output.status.success(), "query removal must fail");
    assert!(
        parser.exists(),
        "query removal must finish before the parser is deleted"
    );
}

#[cfg(unix)]
#[test]
fn test_language_uninstall_all_preflights_recovery_backups_before_restore() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let queries_dir = test_dir.path().join("queries");
    let backup = queries_dir.join(".lua.4294967295.0.backup");
    let nested = backup.join("nested");
    fs::create_dir_all(&nested).expect("Failed to create backup tree");
    fs::write(backup.join("highlights.scm"), "(comment) @comment")
        .expect("Failed to write backup query");
    fs::write(nested.join("query.scm"), "(comment) @comment")
        .expect("Failed to write nested backup query");
    fs::write(backup.join(".kakehashi-install-complete"), "ok\n")
        .expect("Failed to mark backup complete");
    fs::write(
        queries_dir.join(".lua.4294967295.0.backup.kakehashi-backup"),
        "ok\n",
    )
    .expect("Failed to mark backup ownership");
    fs::set_permissions(&nested, fs::Permissions::from_mode(0o000))
        .expect("Failed to make backup subtree unreadable");
    if fs::read_dir(&nested).is_ok() {
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o700))
            .expect("Failed to restore backup permissions");
        return;
    }

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

    let restored_nested = queries_dir.join("lua/nested");
    let nested_to_restore = if nested.exists() {
        &nested
    } else {
        &restored_nested
    };
    fs::set_permissions(nested_to_restore, fs::Permissions::from_mode(0o700))
        .expect("Failed to restore backup subtree permissions");
    assert!(
        !output.status.success(),
        "unreadable backup must fail preflight; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(backup.exists(), "preflight must not restore the backup");
    assert!(
        !queries_dir.join("lua").exists(),
        "failed preflight must not revive the canonical install"
    );
}

/// Test that language uninstall --all ignores internal query staging directories
#[test]
fn test_language_uninstall_all_ignores_internal_query_dirs() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");

    fs::create_dir_all(test_dir.path().join("parser")).expect("Failed to create parser dir");
    fs::create_dir_all(test_dir.path().join("queries/lang1")).expect("Failed to create query dir");
    fs::create_dir_all(test_dir.path().join("queries/.lang1.123.tmp"))
        .expect("Failed to create internal query dir");
    let ext = std::env::consts::DLL_EXTENSION;
    fs::write(test_dir.path().join(format!("parser/lang1.{ext}")), "fake")
        .expect("Failed to write parser");
    fs::write(
        test_dir.path().join("queries/lang1/highlights.scm"),
        "(comment) @comment",
    )
    .expect("Failed to write query");
    fs::write(
        test_dir
            .path()
            .join("queries/.lang1.123.tmp/highlights.scm"),
        "(comment) @comment",
    )
    .expect("Failed to write internal query");

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

    assert!(
        output.status.success(),
        "Uninstall --all should exit with success. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !test_dir.path().join(format!("parser/lang1.{ext}")).exists(),
        "Installed parser should be removed"
    );
    assert!(
        !test_dir.path().join("queries/lang1").exists(),
        "Installed query dir should be removed"
    );
    assert!(
        test_dir.path().join("queries/.lang1.123.tmp").exists(),
        "Internal query dir should not be treated as an installed language"
    );
}

/// Test that uninstall removes backups so later status cannot recover them
#[test]
fn test_language_uninstall_removes_query_backups() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");

    fs::create_dir_all(test_dir.path().join("parser")).expect("Failed to create parser dir");
    fs::create_dir_all(test_dir.path().join("queries/lang1")).expect("Failed to create query dir");
    fs::create_dir_all(test_dir.path().join("queries/.lang1.123.0.backup"))
        .expect("Failed to create backup query dir");
    let ext = std::env::consts::DLL_EXTENSION;
    fs::write(test_dir.path().join(format!("parser/lang1.{ext}")), "fake")
        .expect("Failed to write parser");
    fs::write(
        test_dir.path().join("queries/lang1/highlights.scm"),
        "(comment) @comment",
    )
    .expect("Failed to write query");
    fs::write(
        test_dir
            .path()
            .join("queries/.lang1.123.0.backup/highlights.scm"),
        "(comment) @comment",
    )
    .expect("Failed to write backup query");
    fs::write(
        test_dir
            .path()
            .join("queries/.lang1.123.0.backup/.kakehashi-install-complete"),
        "ok\n",
    )
    .expect("Failed to write backup marker");
    fs::write(
        test_dir
            .path()
            .join("queries/.lang1.123.0.backup.kakehashi-backup"),
        "ok\n",
    )
    .expect("Failed to write backup ownership marker");

    let output = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args([
            "language",
            "uninstall",
            "lang1",
            "--data-dir",
            test_dir.path().to_str().unwrap(),
            "--force",
        ])
        .output()
        .expect("Failed to execute command");

    assert!(
        output.status.success(),
        "Uninstall should exit with success. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !test_dir.path().join("queries/.lang1.123.0.backup").exists(),
        "Uninstall must remove query backups so they cannot be recovered later"
    );

    let status = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args([
            "language",
            "status",
            "--data-dir",
            test_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute command");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );

    assert!(
        !combined.contains("lang1"),
        "Status must not recover an uninstalled language from backup. Got: {combined}"
    );
}

/// Test that language uninstall rejects a language together with --all
#[test]
fn test_language_uninstall_rejects_language_with_all() {
    use std::fs;

    let test_dir = tempfile::tempdir().expect("Failed to create temp dir");

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
            "--all",
            "--data-dir",
            test_dir.path().to_str().unwrap(),
            "--force",
        ])
        .output()
        .expect("Failed to execute command");

    assert!(
        !output.status.success(),
        "Uninstall should reject a language with --all"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--all") && stderr.contains("language"),
        "Stderr should contain clap conflict error message. Got: {stderr}"
    );
    assert!(
        test_dir
            .path()
            .join(format!("parser/testlang.{ext}"))
            .exists(),
        "Parser should not be removed when arguments are invalid"
    );
    assert!(
        test_dir.path().join("queries/testlang").exists(),
        "Queries directory should not be removed when arguments are invalid"
    );
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

/// Run the kakehashi binary with `args`, giving it a stdout pipe whose read end
/// is closed *before* the child is spawned, and return the finished output.
///
/// With no reader, the child inherits only the write end, so its first stdout
/// write fails with EPIPE — and, once the default SIGPIPE disposition is
/// restored, the process is killed by SIGPIPE. This reproduces a broken pipe
/// deterministically, independent of the kernel pipe-buffer size and free of any
/// spawn/write race.
#[cfg(unix)]
fn run_with_broken_stdout_pipe(args: &[&str]) -> std::process::Output {
    use std::os::fd::OwnedFd;
    use std::process::Stdio;

    let (read_fd, write_fd): (OwnedFd, OwnedFd) = nix::unistd::pipe().expect("create pipe");
    // Mark both ends CLOEXEC immediately: `pipe()` creates inheritable fds,
    // and tests run in parallel threads — a concurrently spawned child of
    // ANOTHER test could inherit our read end and keep it open, in which
    // case the child under test writes into a live pipe and exits 0 instead
    // of dying by SIGPIPE (the historical flake in these tests). CLOEXEC
    // stops the leak; `Stdio::from(write_fd)` still works because std dup2s
    // stdio fds for the child, which clears CLOEXEC on the duplicate.
    // (macOS has no `pipe2`, so the flags are set in a second syscall; the
    // remaining pipe()-to-fcntl window is a few instructions wide.)
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    fcntl(&read_fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).expect("set CLOEXEC on read end");
    fcntl(&write_fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).expect("set CLOEXEC on write end");
    // Close the read end before spawning: the child gets only the write end, so
    // there is no reader and the first write hits EPIPE.
    drop(read_fd);

    let child = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args(args)
        .stdout(Stdio::from(write_fd))
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn command");

    child
        .wait_with_output()
        .expect("Failed to wait for command")
}

/// The platform's `SIGPIPE` signal number (13 on Linux/macOS), taken from `nix`
/// rather than hardcoded so it stays correct across architectures.
#[cfg(unix)]
const SIGPIPE: i32 = nix::sys::signal::Signal::SIGPIPE as i32;

/// A CLI subcommand whose stdout reader is gone (e.g. `kakehashi config schema |
/// head`) must not panic. Rust ignores SIGPIPE by default, turning a broken pipe
/// into a panic on the next `print!`; the fix restores the default SIGPIPE
/// disposition so the process is terminated quietly by the signal instead.
///
/// Unix-only: `reset_sigpipe` is a no-op on Windows (no SIGPIPE).
#[cfg(unix)]
#[test]
fn config_schema_does_not_panic_on_broken_pipe() {
    use std::os::unix::process::ExitStatusExt;

    let output = run_with_broken_stdout_pipe(&["config", "schema"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !stderr.contains("panicked"),
        "Broken pipe must not cause a panic. stderr: {stderr}"
    );
    // The process must be terminated by SIGPIPE — proving the default disposition
    // was restored — rather than exiting via a panic (code 101) or normally.
    assert_eq!(
        output.status.signal(),
        Some(SIGPIPE),
        "config schema should be terminated by SIGPIPE; status: {:?}, stderr: {stderr}",
        output.status
    );
}

/// `--help` output is written by clap during argument parsing, before any
/// subcommand dispatch. `reset_sigpipe` runs as the first line of `main`, so a
/// closed reader (e.g. `kakehashi --help | head`) must be handled the same way.
///
/// Unix-only for the same reason as `config_schema_does_not_panic_on_broken_pipe`.
#[cfg(unix)]
#[test]
fn help_does_not_panic_on_broken_pipe() {
    use std::os::unix::process::ExitStatusExt;

    let output = run_with_broken_stdout_pipe(&["--help"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !stderr.contains("panicked"),
        "Broken pipe on --help must not cause a panic. stderr: {stderr}"
    );
    assert_eq!(
        output.status.signal(),
        Some(SIGPIPE),
        "--help should be terminated by SIGPIPE; status: {:?}, stderr: {stderr}",
        output.status
    );
}
