//! Integration coverage for the hidden `__compile-parser` subcommand — the
//! re-exec target of the killable parser-compile deadline (see the
//! per-document-parse-actor ADR). A *successful* in-process compile is unit-tested
//! in `install::parser` (`test_compile_parser_with_loader`); here we lock the
//! subcommand's contract through the real binary: it exists, is hidden from
//! `--help`, and maps a failed compile to a non-zero exit without producing an
//! output artifact.

use std::process::Command;

fn kakehashi() -> Command {
    Command::new(env!("CARGO_BIN_EXE_kakehashi"))
}

#[test]
fn compile_parser_subcommand_exits_nonzero_on_nonexistent_grammar() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let nonexistent_grammar = tmp.path().join("no-such-grammar");
    let out = tmp
        .path()
        .join(format!("out.{}", std::env::consts::DLL_EXTENSION));

    let output = kakehashi()
        .arg("__compile-parser")
        .arg(&nonexistent_grammar)
        .arg(&out)
        .output()
        .expect("spawn kakehashi __compile-parser");

    assert!(
        !output.status.success(),
        "compiling a nonexistent grammar must exit non-zero (stderr: {})",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !out.exists(),
        "no shared library should be produced for a nonexistent grammar"
    );
}

#[test]
fn compile_parser_subcommand_is_hidden_from_help() {
    let output = kakehashi()
        .arg("--help")
        .output()
        .expect("spawn kakehashi --help");

    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        !help.contains("__compile-parser"),
        "the internal subcommand must be hidden from --help, got:\n{help}"
    );
}
