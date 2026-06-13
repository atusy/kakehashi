//! `kakehashi diagnose <paths...>` — pull diagnostics for files through the
//! same injection-region + host bridge the LSP server uses, and print them in
//! a `--output-format` (grep / quickfix / JSONL).
//!
//! Like `kakehashi format`, the command runs the LSP server in-process (no
//! JSON-RPC framing): it builds the [`LspService`] the same way the server
//! does, then drives `initialize` → `didOpen` → `textDocument/diagnostic` →
//! `didClose` per file by calling the handler implementations directly. This
//! reuses config loading, language detection, injection resolution, and the
//! downstream language-server pool verbatim, so CLI diagnostics can never
//! drift from editor diagnostics.
//!
//! Exit codes:
//! - `0`: no diagnostics met `--threshold` (and no operational error).
//! - `1`: at least one diagnostic was at least as severe as `--threshold`.
//!   `--threshold none` disables this entirely (diagnostics never set exit 1).
//! - `2`: an operational error (a file could not be read, a path could not be
//!   opened, or a configured downstream server failed). This is independent
//!   of `--threshold` — a tool that could not even read a file must not look
//!   "clean" to CI, so `none` still surfaces operational failures as `2`.
//!
//! File selection mirrors `format` (see [`crate::cli::files`]).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tower_lsp_server::LspService;
use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, NumberOrString};

use crate::cli::files::collect_files;
use crate::lsp::Kakehashi;

/// How to render each diagnostic. Derives clap's kebab-case value names:
/// `grep`, `quickfix`, `jsonl`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum OutputFormat {
    /// `file:line:col:message` — terse, `grep`/`ripgrep --vimgrep` style.
    #[default]
    Grep,
    /// `file:line:col: severity: message [source]` — compiler/quickfix style.
    Quickfix,
    /// One JSON object per line with the full structured diagnostic.
    Jsonl,
}

/// Minimum severity that makes the run exit `1`. `none` disables
/// diagnostic-based exit codes entirely. Derives clap value names `error`,
/// `warning`, `info`, `hint`, `none`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Threshold {
    /// Only errors set exit `1`.
    #[default]
    Error,
    /// Errors and warnings set exit `1`.
    Warning,
    /// Errors, warnings, and information set exit `1`.
    Info,
    /// Any diagnostic sets exit `1`.
    Hint,
    /// Diagnostics never set exit `1`.
    None,
}

/// Options for the `diagnose` subcommand, mirroring its CLI flags.
pub struct DiagnoseOptions {
    /// Files or directories to diagnose. With `--stdin-filename`, must be
    /// empty or exactly `["-"]`.
    pub paths: Vec<PathBuf>,
    /// Read content from stdin, treat it as this file path (for language
    /// detection and config resolution), and print its diagnostics.
    pub stdin_filename: Option<PathBuf>,
    /// Gitignore-style exclusion patterns, relative to the current directory.
    pub excludes: Vec<String>,
    /// Output rendering for each diagnostic.
    pub output_format: OutputFormat,
    /// Minimum severity that triggers exit `1`.
    pub threshold: Threshold,
    /// CI mode: suppress the human-readable summary on stderr (diagnostics on
    /// stdout and operational errors on stderr are still printed).
    pub quiet: bool,
}

/// No diagnostics met `--threshold`, and no operational error.
pub const EXIT_OK: u8 = 0;
/// At least one diagnostic was at least as severe as `--threshold`.
pub const EXIT_DIAGNOSTICS: u8 = 1;
/// An operational error (unreadable file, un-openable path, downstream server
/// failure). Independent of `--threshold`.
pub const EXIT_ERROR: u8 = 2;

/// Per-server bound for waiting on cold downstream language servers, matching
/// `format`'s budget: generous enough for a slow first launch without hanging
/// an unconfigured run forever.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Entry point for `kakehashi diagnose`. Returns the process exit code.
pub fn run(options: DiagnoseOptions) -> u8 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: failed to start async runtime: {e}");
            return EXIT_ERROR;
        }
    };
    runtime.block_on(run_async(options))
}

async fn run_async(options: DiagnoseOptions) -> u8 {
    let cwd = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("error: cannot determine current directory: {e}");
            return EXIT_ERROR;
        }
    };

    // Same construction as LSP server mode, but the loopback client socket is
    // pumped by a stub instead of an editor, and handlers are called directly.
    let (service, socket) = LspService::new(Kakehashi::new);
    crate::cli::spawn_client_pump(socket);
    let server = service.inner();
    server.cli_initialize(&cwd).await;

    let code = if options.stdin_filename.is_some() {
        run_stdin(server, &cwd, &options).await
    } else {
        run_paths(server, &cwd, &options).await
    };

    server.cli_shutdown().await;
    code
}

/// Running tally for one `diagnose` invocation, used for the summary and the
/// final exit code.
#[derive(Default)]
struct Report {
    /// Total diagnostics printed.
    total: usize,
    /// Whether any printed diagnostic was at least as severe as the threshold.
    threshold_hit: bool,
    /// Whether any operational error occurred (unreadable file, server
    /// failure, un-openable path).
    operational_error: bool,
}

impl Report {
    /// Account for one file's diagnostics, appending their rendered lines to
    /// `out`. Diagnostics are sorted for deterministic output.
    fn record_file(
        &mut self,
        display: &str,
        mut diagnostics: Vec<Diagnostic>,
        format: OutputFormat,
        threshold: Threshold,
        out: &mut String,
    ) {
        // The diagnostic fan-out collects across regions and servers in
        // completion order, so position alone is not a stable key. Sort by
        // position, then tie-break by severity, source, code, and message so
        // two findings at the same spot always print in the same order.
        diagnostics.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
        for diagnostic in &diagnostics {
            out.push_str(&format_diagnostic(format, display, diagnostic));
            out.push('\n');
            self.total += 1;
            if meets_threshold(diagnostic, threshold) {
                self.threshold_hit = true;
            }
        }
    }

    /// The process exit code: operational errors win over diagnostic gating so
    /// a broken run never looks "clean" to CI.
    fn exit_code(&self) -> u8 {
        if self.operational_error {
            EXIT_ERROR
        } else if self.threshold_hit {
            EXIT_DIAGNOSTICS
        } else {
            EXIT_OK
        }
    }
}

/// File mode: expand `paths`, diagnose each file, and print per `--output-format`.
async fn run_paths(server: &Kakehashi, cwd: &Path, options: &DiagnoseOptions) -> u8 {
    if options.paths.is_empty() {
        eprintln!("error: no paths given; pass files/directories or use --stdin-filename");
        return EXIT_ERROR;
    }

    let files = match collect_files(cwd, &options.paths, &options.excludes, &|path| {
        server.cli_can_handle_path(path)
    }) {
        Ok(files) => files,
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_ERROR;
        }
    };

    let mut report = Report::default();
    let mut output = String::new();
    for file in &files {
        // Collected paths are absolute; report them cwd-relative so the output
        // stays readable (and editor-openable) in deep trees.
        let display = file.strip_prefix(cwd).unwrap_or(file).display().to_string();
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("error: cannot read '{display}': {e}");
                report.operational_error = true;
                continue;
            }
        };
        let outcome = server
            .cli_diagnose_text(file, &text, SERVER_READY_TIMEOUT)
            .await;
        for failure in &outcome.server_failures {
            eprintln!("error: {display}: {failure}");
            report.operational_error = true;
        }
        report.record_file(
            &display,
            outcome.diagnostics,
            options.output_format,
            options.threshold,
            &mut output,
        );
    }

    finish(&output, &report, files.len(), options)
}

/// Stdin mode: diagnose stdin as if it were `--stdin-filename`.
async fn run_stdin(server: &Kakehashi, cwd: &Path, options: &DiagnoseOptions) -> u8 {
    let name = options
        .stdin_filename
        .as_ref()
        .expect("run_stdin requires stdin_filename");
    // Documented contract (shared with `format`): with --stdin-filename, paths
    // must be empty or exactly ["-"]; anything else is a usage error.
    let stdin_paths_ok = options.paths.is_empty()
        || (options.paths.len() == 1 && options.paths[0].as_os_str() == "-");
    if !stdin_paths_ok {
        eprintln!("error: --stdin-filename accepts no paths (optionally a single \"-\")");
        return EXIT_ERROR;
    }

    let mut text = String::new();
    if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut text) {
        eprintln!("error: failed to read stdin: {e}");
        return EXIT_ERROR;
    }

    let absolute = if name.is_absolute() {
        name.clone()
    } else {
        cwd.join(name)
    };
    let outcome = server
        .cli_diagnose_text(&absolute, &text, SERVER_READY_TIMEOUT)
        .await;

    let mut report = Report::default();
    let display = name.display().to_string();
    for failure in &outcome.server_failures {
        eprintln!("error: {display}: {failure}");
        report.operational_error = true;
    }
    let mut output = String::new();
    report.record_file(
        &display,
        outcome.diagnostics,
        options.output_format,
        options.threshold,
        &mut output,
    );

    finish(&output, &report, 1, options)
}

/// Write the buffered diagnostics to stdout, print the summary (unless
/// `--quiet`), and return the exit code.
fn finish(output: &str, report: &Report, file_count: usize, options: &DiagnoseOptions) -> u8 {
    // SIGPIPE is ignored in diagnose mode (the bridge needs BrokenPipe as a
    // recoverable error), so a consumer that stops reading (`… | head`)
    // surfaces here as a write error rather than killing the process — its
    // early exit is normal, not ours.
    let mut stdout = std::io::stdout().lock();
    if let Err(e) = stdout
        .write_all(output.as_bytes())
        .and_then(|()| stdout.flush())
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        eprintln!("error: failed to write stdout: {e}");
        return EXIT_ERROR;
    }

    if !options.quiet {
        let file_label = if file_count == 1 { "file" } else { "files" };
        let diag_label = if report.total == 1 {
            "diagnostic"
        } else {
            "diagnostics"
        };
        eprintln!("{} {diag_label} in {file_count} {file_label}", report.total);
    }

    report.exit_code()
}

/// Render one diagnostic in the requested format. `display` is the
/// already-formatted (cwd-relative or stdin) path.
fn format_diagnostic(format: OutputFormat, display: &str, diagnostic: &Diagnostic) -> String {
    // LSP positions are 0-based; editors and grep/quickfix consumers expect
    // 1-based line and column. `character` is a UTF-16 offset — presenting it
    // as a column is the same approximation grep/ripgrep make.
    let line = diagnostic.range.start.line.saturating_add(1);
    let col = diagnostic.range.start.character.saturating_add(1);
    match format {
        OutputFormat::Grep => {
            format!("{display}:{line}:{col}:{}", one_line(&diagnostic.message))
        }
        OutputFormat::Quickfix => {
            let severity = severity_word(diagnostic.severity);
            let source = diagnostic
                .source
                .as_deref()
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            format!(
                "{display}:{line}:{col}: {severity}: {}{source}",
                one_line(&diagnostic.message)
            )
        }
        OutputFormat::Jsonl => {
            let code = diagnostic.code.as_ref().map(|c| match c {
                NumberOrString::Number(n) => serde_json::json!(n),
                NumberOrString::String(s) => serde_json::json!(s),
            });
            let value = serde_json::json!({
                "file": display,
                "line": line,
                "column": col,
                "endLine": diagnostic.range.end.line.saturating_add(1),
                "endColumn": diagnostic.range.end.character.saturating_add(1),
                "severity": severity_word(diagnostic.severity),
                "code": code,
                // Borrow rather than serialize the owned fields by value (the
                // macro borrows either way, but the explicit `&str` makes that
                // unambiguous).
                "source": diagnostic.source.as_deref(),
                "message": diagnostic.message.as_str(),
            });
            value.to_string()
        }
    }
}

/// Collapse a (possibly multi-line) diagnostic message onto one line so it
/// stays parseable in the line-oriented grep/quickfix formats.
fn one_line(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The lower-case severity word. A diagnostic with no severity is treated as
/// an error (the conservative choice — see [`effective_severity`]); an
/// out-of-spec numeric severity renders as `unknown`.
fn severity_word(severity: Option<DiagnosticSeverity>) -> &'static str {
    match severity {
        None => "error",
        Some(DiagnosticSeverity::ERROR) => "error",
        Some(DiagnosticSeverity::WARNING) => "warning",
        Some(DiagnosticSeverity::INFORMATION) => "info",
        Some(DiagnosticSeverity::HINT) => "hint",
        Some(_) => "unknown",
    }
}

/// A diagnostic's effective severity for gating. LSP allows an absent
/// severity (the client decides); we treat it as an error so a server that
/// omits severity can never silently slip past a threshold.
fn effective_severity(diagnostic: &Diagnostic) -> DiagnosticSeverity {
    diagnostic.severity.unwrap_or(DiagnosticSeverity::ERROR)
}

/// Whether `diagnostic` is at least as severe as `threshold`. `DiagnosticSeverity`
/// orders ERROR(1) < WARNING(2) < INFORMATION(3) < HINT(4), so "at least as
/// severe" is `<=`.
fn meets_threshold(diagnostic: &Diagnostic, threshold: Threshold) -> bool {
    let gate = match threshold {
        Threshold::None => return false,
        Threshold::Error => DiagnosticSeverity::ERROR,
        Threshold::Warning => DiagnosticSeverity::WARNING,
        Threshold::Info => DiagnosticSeverity::INFORMATION,
        Threshold::Hint => DiagnosticSeverity::HINT,
    };
    effective_severity(diagnostic) <= gate
}

/// A diagnostic's code as an order key: `(discriminant, number, string)` —
/// `None` (0) < numeric (1) < string (2), numbers compared as integers (not
/// lexically), strings by text. Borrows from the diagnostic, so allocation-free.
type CodeSortKey<'a> = (u8, i32, Option<&'a str>);

/// A total-order key for stable output. Ordering: position, then severity
/// (most severe first), source, code, and message — so a diagnostic with no
/// severity (ranked as error) and one out-of-spec severity still sort
/// deterministically.
///
/// Allocation-free (it borrows from `diagnostic`): `sort_key` is evaluated
/// twice per comparison across an `O(N log N)` sort, so it must not allocate.
fn sort_key(diagnostic: &Diagnostic) -> (u32, u32, u8, Option<&str>, CodeSortKey<'_>, &str) {
    let severity_rank = match diagnostic.severity {
        None => 1,
        Some(DiagnosticSeverity::ERROR) => 1,
        Some(DiagnosticSeverity::WARNING) => 2,
        Some(DiagnosticSeverity::INFORMATION) => 3,
        Some(DiagnosticSeverity::HINT) => 4,
        Some(_) => 5,
    };
    let code_key = match &diagnostic.code {
        None => (0, 0, None),
        Some(NumberOrString::Number(n)) => (1, *n, None),
        Some(NumberOrString::String(s)) => (2, 0, Some(s.as_str())),
    };
    (
        diagnostic.range.start.line,
        diagnostic.range.start.character,
        severity_rank,
        diagnostic.source.as_deref(),
        code_key,
        diagnostic.message.as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{Position, Range};

    fn diag(
        line: u32,
        col: u32,
        severity: Option<DiagnosticSeverity>,
        message: &str,
    ) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(line, col), Position::new(line, col + 1)),
            severity,
            message: message.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn grep_format_is_one_based_and_terse() {
        let d = diag(0, 0, Some(DiagnosticSeverity::WARNING), "unused variable");
        assert_eq!(
            format_diagnostic(OutputFormat::Grep, "src/a.lua", &d),
            "src/a.lua:1:1:unused variable"
        );
    }

    #[test]
    fn quickfix_format_includes_severity_and_source() {
        let mut d = diag(4, 2, Some(DiagnosticSeverity::ERROR), "boom");
        d.source = Some("lua-ls".to_string());
        assert_eq!(
            format_diagnostic(OutputFormat::Quickfix, "a.md", &d),
            "a.md:5:3: error: boom [lua-ls]"
        );
    }

    #[test]
    fn quickfix_without_source_omits_the_bracket() {
        let d = diag(0, 0, Some(DiagnosticSeverity::HINT), "hint here");
        assert_eq!(
            format_diagnostic(OutputFormat::Quickfix, "a.md", &d),
            "a.md:1:1: hint: hint here"
        );
    }

    #[test]
    fn jsonl_format_is_structured_and_one_based() {
        let mut d = diag(2, 5, Some(DiagnosticSeverity::WARNING), "msg");
        d.source = Some("ruff".to_string());
        d.code = Some(NumberOrString::String("E501".to_string()));
        let value: serde_json::Value =
            serde_json::from_str(&format_diagnostic(OutputFormat::Jsonl, "x.py", &d)).unwrap();
        assert_eq!(value["file"], "x.py");
        assert_eq!(value["line"], 3);
        assert_eq!(value["column"], 6);
        assert_eq!(value["endLine"], 3);
        assert_eq!(value["endColumn"], 7);
        assert_eq!(value["severity"], "warning");
        assert_eq!(value["source"], "ruff");
        assert_eq!(value["code"], "E501");
        assert_eq!(value["message"], "msg");
    }

    #[test]
    fn jsonl_numeric_code_stays_numeric() {
        let mut d = diag(0, 0, Some(DiagnosticSeverity::ERROR), "m");
        d.code = Some(NumberOrString::Number(42));
        let value: serde_json::Value =
            serde_json::from_str(&format_diagnostic(OutputFormat::Jsonl, "x", &d)).unwrap();
        assert_eq!(value["code"], 42);
    }

    #[test]
    fn jsonl_absent_code_and_source_are_null() {
        let d = diag(0, 0, Some(DiagnosticSeverity::ERROR), "m");
        let value: serde_json::Value =
            serde_json::from_str(&format_diagnostic(OutputFormat::Jsonl, "x", &d)).unwrap();
        assert!(value["code"].is_null());
        assert!(value["source"].is_null());
    }

    #[test]
    fn message_newlines_are_collapsed_for_line_formats() {
        let d = diag(
            0,
            0,
            Some(DiagnosticSeverity::ERROR),
            "line one\n  line two",
        );
        assert_eq!(
            format_diagnostic(OutputFormat::Grep, "f", &d),
            "f:1:1:line one line two"
        );
    }

    #[test]
    fn absent_severity_renders_and_gates_as_error() {
        let d = diag(0, 0, None, "no severity");
        assert_eq!(severity_word(d.severity), "error");
        assert!(meets_threshold(&d, Threshold::Error));
    }

    #[test]
    fn threshold_none_never_gates() {
        let d = diag(0, 0, Some(DiagnosticSeverity::ERROR), "err");
        assert!(!meets_threshold(&d, Threshold::None));
    }

    #[test]
    fn threshold_warning_gates_errors_and_warnings_only() {
        let error = diag(0, 0, Some(DiagnosticSeverity::ERROR), "e");
        let warning = diag(0, 0, Some(DiagnosticSeverity::WARNING), "w");
        let info = diag(0, 0, Some(DiagnosticSeverity::INFORMATION), "i");
        let hint = diag(0, 0, Some(DiagnosticSeverity::HINT), "h");
        assert!(meets_threshold(&error, Threshold::Warning));
        assert!(meets_threshold(&warning, Threshold::Warning));
        assert!(!meets_threshold(&info, Threshold::Warning));
        assert!(!meets_threshold(&hint, Threshold::Warning));
    }

    #[test]
    fn threshold_error_gates_errors_only() {
        let warning = diag(0, 0, Some(DiagnosticSeverity::WARNING), "w");
        let error = diag(0, 0, Some(DiagnosticSeverity::ERROR), "e");
        assert!(meets_threshold(&error, Threshold::Error));
        assert!(!meets_threshold(&warning, Threshold::Error));
    }

    #[test]
    fn threshold_hint_gates_everything() {
        for sev in [
            DiagnosticSeverity::ERROR,
            DiagnosticSeverity::WARNING,
            DiagnosticSeverity::INFORMATION,
            DiagnosticSeverity::HINT,
        ] {
            assert!(meets_threshold(
                &diag(0, 0, Some(sev), "x"),
                Threshold::Hint
            ));
        }
    }

    #[test]
    fn record_file_sorts_by_position_and_counts() {
        let mut report = Report::default();
        let mut out = String::new();
        let diags = vec![
            diag(5, 0, Some(DiagnosticSeverity::WARNING), "second"),
            diag(1, 0, Some(DiagnosticSeverity::ERROR), "first"),
        ];
        report.record_file("f", diags, OutputFormat::Grep, Threshold::Error, &mut out);
        assert_eq!(out, "f:2:1:first\nf:6:1:second\n");
        assert_eq!(report.total, 2);
        assert!(report.threshold_hit, "an error meets the error threshold");
    }

    #[test]
    fn record_file_tie_breaks_same_position_by_severity_then_message() {
        // Two findings at the same spot arriving in "worst" order must still
        // print deterministically: more severe first, then message order.
        let mut report = Report::default();
        let mut out = String::new();
        let diags = vec![
            diag(0, 0, Some(DiagnosticSeverity::WARNING), "zebra"),
            diag(0, 0, Some(DiagnosticSeverity::ERROR), "apple"),
            diag(0, 0, Some(DiagnosticSeverity::WARNING), "apple"),
        ];
        report.record_file(
            "f",
            diags,
            OutputFormat::Quickfix,
            Threshold::None,
            &mut out,
        );
        assert_eq!(
            out,
            "f:1:1: error: apple\nf:1:1: warning: apple\nf:1:1: warning: zebra\n"
        );
    }

    #[test]
    fn sort_key_orders_numeric_codes_numerically_not_lexically() {
        // "n10" < "n9" lexically would mis-order; integer codes must compare
        // as integers.
        let mut nine = diag(0, 0, Some(DiagnosticSeverity::ERROR), "m");
        nine.code = Some(NumberOrString::Number(9));
        let mut ten = diag(0, 0, Some(DiagnosticSeverity::ERROR), "m");
        ten.code = Some(NumberOrString::Number(10));
        assert!(sort_key(&nine) < sort_key(&ten));
    }

    #[test]
    fn sort_key_orders_absent_then_numeric_then_string_codes() {
        let mut none = diag(0, 0, Some(DiagnosticSeverity::ERROR), "m");
        none.code = None;
        let mut num = diag(0, 0, Some(DiagnosticSeverity::ERROR), "m");
        num.code = Some(NumberOrString::Number(999));
        let mut text = diag(0, 0, Some(DiagnosticSeverity::ERROR), "m");
        text.code = Some(NumberOrString::String("E1".to_string()));
        assert!(sort_key(&none) < sort_key(&num));
        assert!(sort_key(&num) < sort_key(&text));
    }

    #[test]
    fn exit_code_prefers_operational_error_over_diagnostics() {
        let report = Report {
            total: 1,
            threshold_hit: true,
            operational_error: true,
        };
        assert_eq!(report.exit_code(), EXIT_ERROR);
    }

    #[test]
    fn exit_code_diagnostics_when_threshold_hit_without_error() {
        let report = Report {
            total: 1,
            threshold_hit: true,
            operational_error: false,
        };
        assert_eq!(report.exit_code(), EXIT_DIAGNOSTICS);
    }

    #[test]
    fn exit_code_ok_when_clean() {
        assert_eq!(Report::default().exit_code(), EXIT_OK);
    }
}
