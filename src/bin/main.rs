use clap::{Parser, Subcommand};
use kakehashi::install::{default_data_dir, metadata, parser, queries};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// A Language Server Protocol (LSP) server using Tree-sitter for parsing
#[derive(Parser)]
#[command(name = "kakehashi")]
#[command(version)]
#[command(about = "A Language Server Protocol (LSP) server using Tree-sitter for parsing")]
struct Cli {
    /// Custom data directory (overrides KAKEHASHI_DATA_DIR and platform default)
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    /// Config file(s) to use instead of default locations (LSP and format
    /// modes). Can be specified multiple times; files merge in order.
    /// Skips ~/.config/kakehashi/kakehashi.toml and ./kakehashi.toml.
    #[arg(long, global = true)]
    config_file: Vec<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage language parsers and queries
    Language {
        #[command(subcommand)]
        action: LanguageAction,
    },
    /// Manage configuration files
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Format files via the configured downstream language servers
    ///
    /// Directories are walked recursively respecting .gitignore; explicitly
    /// listed files are formatted even when gitignored.
    Format {
        /// Files or directories to format ("-" for stdin with --stdin-filename)
        paths: Vec<PathBuf>,

        /// Don't write changes; exit 1 if any file would be reformatted
        #[arg(long)]
        check: bool,

        /// Read from stdin, treat content as this file path, print result to stdout
        #[arg(long)]
        stdin_filename: Option<PathBuf>,

        /// Exclude paths matching this gitignore-style pattern (repeatable)
        #[arg(long = "excludes")]
        excludes: Vec<String>,

        /// Write changes, but exit 1 if any file was changed
        #[arg(long)]
        fail_on_change: bool,

        /// Indentation-width hint sent to downstream servers
        /// (LSP FormattingOptions.tabSize; servers may ignore it)
        #[arg(long, default_value_t = 4)]
        tab_size: u32,

        /// Prefer-spaces-over-tabs hint sent to downstream servers
        /// (LSP FormattingOptions.insertSpaces; servers may ignore it)
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set, num_args = 1)]
        insert_spaces: bool,
    },
    /// Internal: compile a parser grammar in a killable subprocess.
    ///
    /// Re-exec target of `install::parser::compile_parser`, which runs this inside
    /// a process group it can kill on a deadline (the loader shells out to `cc`
    /// with no surfaced child). Not for direct use; hidden from `--help`.
    #[command(name = "__compile-parser", hide = true)]
    CompileParser {
        /// Grammar source directory (contains src/parser.c)
        grammar_dir: PathBuf,
        /// Output path for the compiled shared library
        output_path: PathBuf,
    },
    /// Report diagnostics for files via the configured downstream language servers
    ///
    /// Only pull diagnostics (textDocument/diagnostic) are collected. Push
    /// diagnostics (textDocument/publishDiagnostics) are NOT reported, so a
    /// downstream server that only publishes diagnostics and does not answer a
    /// pull request will contribute nothing here.
    ///
    /// Directories are walked recursively respecting .gitignore; explicitly
    /// listed files are diagnosed even when gitignored.
    ///
    /// Exit codes: 0 = no failing diagnostics; 1 = a failing diagnostic (any
    /// error, plus warnings with --fail-on-warning; info/hint never fail —
    /// append `|| true` to never fail); 2 = an operational error (unreadable
    /// file, path open/enumeration failure, downstream server failure),
    /// independent of the diagnostics.
    Diagnose {
        /// Files or directories to diagnose ("-" for stdin with --stdin-filename)
        paths: Vec<PathBuf>,

        /// Read from stdin, treat content as this file path, print its diagnostics
        #[arg(long)]
        stdin_filename: Option<PathBuf>,

        /// Exclude paths matching this gitignore-style pattern (repeatable)
        #[arg(long = "excludes")]
        excludes: Vec<String>,

        /// How to render each diagnostic
        #[arg(long, value_enum, default_value = "default")]
        output_format: kakehashi::cli::diagnose::OutputFormat,

        /// Exit 1 on warnings too, not just errors (info/hint never fail)
        #[arg(long)]
        fail_on_warning: bool,
    },
}

#[derive(Subcommand)]
enum LanguageAction {
    /// Install a Tree-sitter parser and its queries for a language
    Install {
        /// The language to install (e.g., lua, rust, python)
        language: String,

        /// Overwrite existing files if they exist
        #[arg(long)]
        force: bool,

        /// Print verbose output
        #[arg(long, short)]
        verbose: bool,

        /// Bypass the metadata cache and fetch fresh data from network
        #[arg(long)]
        no_cache: bool,
    },
    /// List supported languages for installation
    List {
        /// Bypass the metadata cache and fetch fresh data from network
        #[arg(long)]
        no_cache: bool,
    },
    /// Show installed languages and their status
    Status {
        /// Print verbose output (show file paths)
        #[arg(long, short)]
        verbose: bool,
    },
    /// Remove installed parser and queries for a language
    Uninstall {
        /// The language to uninstall (e.g., lua, rust, python)
        #[arg(required_unless_present = "all")]
        language: Option<String>,

        /// Skip confirmation prompt
        #[arg(long)]
        force: bool,

        /// Remove all installed languages
        #[arg(long, conflicts_with = "language")]
        all: bool,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Generate a default configuration template
    ///
    /// By default, outputs to stdout for piping or redirection.
    /// Use --output to write directly to a file.
    Init {
        /// Write to specified file instead of stdout. Use "-" for explicit stdout.
        #[arg(long)]
        output: Option<PathBuf>,

        /// Overwrite existing file (only applies with --output)
        #[arg(long)]
        force: bool,
    },
    /// Generate JSON Schema for the configuration format
    ///
    /// By default, outputs to stdout for piping or redirection.
    /// Use --output to write directly to a file.
    Schema {
        /// Write to specified file instead of stdout. Use "-" for explicit stdout.
        #[arg(long)]
        output: Option<PathBuf>,

        /// Overwrite existing file (only applies with --output)
        #[arg(long)]
        force: bool,
    },
}

/// Restore the default `SIGPIPE` disposition (Unix only).
///
/// Rust ignores `SIGPIPE` at startup, which turns a broken pipe into a panic on
/// the next `print!`/`println!` (e.g. `kakehashi config schema | head`, or even
/// `kakehashi --help | head`). Restoring the conventional Unix behavior makes the
/// process terminate quietly with `SIGPIPE` when the reader goes away. This is
/// installed at the very start of `main` so it also covers clap's `--help` /
/// `--version` output emitted during argument parsing; LSP server mode restores
/// the ignored disposition afterwards via [`ignore_sigpipe`].
#[cfg(unix)]
fn reset_sigpipe() {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
    let action = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
    // SAFETY: `SigDfl` is async-signal-safe and installed before any output.
    let result = unsafe { sigaction(Signal::SIGPIPE, &action) };
    // Restoring the default disposition for a valid signal cannot realistically
    // fail, but surface it on stderr rather than swallowing it: otherwise a
    // silent failure would regress to panicking on a broken pipe. Use `writeln!`
    // (ignoring its result) instead of `eprintln!`, which would itself panic if
    // stderr is a broken pipe.
    if let Err(e) = result {
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr(),
            "warning: failed to restore default SIGPIPE handler: {e}"
        );
    }
}

/// Ignore `SIGPIPE` (Unix only) — the disposition the Rust runtime installs by
/// default.
///
/// LSP server mode uses this to undo [`reset_sigpipe`]: the bridge writes to
/// downstream language-server stdin and must observe a closed peer as a
/// recoverable `BrokenPipe` I/O error rather than being killed by the signal.
#[cfg(unix)]
fn ignore_sigpipe() {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
    let action = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    // SAFETY: `SigIgn` is async-signal-safe and installed before bridge I/O.
    let result = unsafe { sigaction(Signal::SIGPIPE, &action) };
    // LSP server mode relies on the ignored disposition so the bridge sees a
    // closed downstream peer as a recoverable BrokenPipe; surface a failure
    // rather than silently risking a SIGPIPE kill. Use `writeln!` (ignoring its
    // result) instead of `eprintln!`, which would panic on a broken stderr.
    if let Err(e) = result {
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "warning: failed to ignore SIGPIPE: {e}");
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {}

#[cfg(not(unix))]
fn ignore_sigpipe() {}

fn main() -> ExitCode {
    // Restore the default SIGPIPE disposition before clap may write `--help` /
    // `--version` to a (possibly piped) stdout during `Cli::parse()`.
    reset_sigpipe();

    let cli = Cli::parse();

    // Set data directory override so default_data_dir() and config expansion
    // all resolve consistently from this single flag
    if let Some(ref dir) = cli.data_dir {
        kakehashi::config::set_data_dir_override(dir.clone());
    }

    if !cli.config_file.is_empty() {
        kakehashi::config::set_config_file_override(cli.config_file);
    }

    // LSP server mode keeps SIGPIPE ignored so the bridge sees a closed
    // downstream peer as a recoverable BrokenPipe error. The format and
    // diagnose commands drive the same bridge (they write to downstream
    // language-server stdin), so they need the same disposition — otherwise a
    // crashed downstream server would kill the CLI with SIGPIPE instead of
    // exiting 2 with a useful error; their own stdout writes handle BrokenPipe
    // explicitly (see `cli::format::run_stdin` / `cli::diagnose::write_chunk`).
    // Other subcommands keep the default disposition restored above.
    if matches!(
        cli.command,
        None | Some(Commands::Format { .. } | Commands::Diagnose { .. })
    ) {
        ignore_sigpipe();
    }

    let result = match cli.command {
        Some(Commands::Language { action }) => match action {
            LanguageAction::Install {
                language,
                force,
                verbose,
                no_cache,
            } => run_install(&language, force, verbose, no_cache),
            LanguageAction::List { no_cache } => run_list_languages(no_cache),
            LanguageAction::Status { verbose } => run_language_status(verbose),
            LanguageAction::Uninstall {
                language,
                force,
                all,
            } => run_language_uninstall(language, force, all),
        },
        Some(Commands::Config { action }) => match action {
            ConfigAction::Init { output, force } => run_config_init(output, force),
            ConfigAction::Schema { output, force } => run_config_schema(output, force),
        },
        Some(Commands::Format {
            paths,
            check,
            stdin_filename,
            excludes,
            fail_on_change,
            tab_size,
            insert_spaces,
        }) => run_format(kakehashi::cli::format::FormatOptions {
            paths,
            check,
            stdin_filename,
            excludes,
            fail_on_change,
            tab_size,
            insert_spaces,
        }),
        Some(Commands::Diagnose {
            paths,
            stdin_filename,
            excludes,
            output_format,
            fail_on_warning,
        }) => run_diagnose(kakehashi::cli::diagnose::DiagnoseOptions {
            paths,
            stdin_filename,
            excludes,
            output_format,
            fail_on_warning,
        }),
        Some(Commands::CompileParser {
            grammar_dir,
            output_path,
        }) => run_compile_parser(&grammar_dir, &output_path),
        None => {
            // Start LSP server (backward compatible default behavior)
            // Only LSP mode needs a tokio runtime; CLI subcommands are synchronous
            run_lsp_server();
            Ok(())
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

/// Run the list-languages command
fn run_list_languages(no_cache: bool) -> Result<(), ExitCode> {
    let data_dir = default_data_dir();
    let options = metadata::FetchOptions {
        data_dir: data_dir.as_deref(),
        use_cache: !no_cache,
    };

    if no_cache {
        eprintln!("Fetching supported languages from nvim-treesitter (cache bypassed)...");
    } else {
        eprintln!("Fetching supported languages from nvim-treesitter...");
    }

    match metadata::list_supported_languages(Some(&options)) {
        Ok(languages) => {
            eprintln!("Supported languages ({} total):", languages.len());
            for lang in languages {
                println!("  {}", lang);
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("Failed to fetch language list: {}", e);
            Err(ExitCode::FAILURE)
        }
    }
}

/// Documentation link for configuration
const DOC_LINK: &str =
    "# Documentation: https://github.com/atusy/kakehashi/blob/main/docs/README.md\n";

/// Run the language status command
fn run_language_status(verbose: bool) -> Result<(), ExitCode> {
    use std::collections::BTreeSet;
    use std::fs;

    let data_dir = default_data_dir().ok_or_else(|| {
        eprintln!("Error: Could not determine data directory. Please specify --data-dir.");
        ExitCode::FAILURE
    })?;

    let parser_dir = data_dir.join("parser");
    let queries_dir = data_dir.join("queries");
    if let Err(e) = queries::recover_interrupted_query_installs(&queries_dir) {
        eprintln!("Warning: failed to recover interrupted query installs: {e}");
    }

    // Collect all installed languages from both parser and queries directories
    let mut languages = BTreeSet::new();

    // Scan parser directory for .so, .dylib, .dll files
    if let Ok(entries) = fs::read_dir(&parser_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_parser = path
                .extension()
                .map(|ext| ext == std::env::consts::DLL_EXTENSION)
                .unwrap_or(false);
            if is_parser && let Some(stem) = path.file_stem() {
                languages.insert(stem.to_string_lossy().to_string());
            }
        }
    }

    // Also check queries directory for languages that might only have queries
    if let Ok(entries) = fs::read_dir(&queries_dir) {
        for entry in entries.flatten() {
            if let Some(name) = installed_query_language_name(&entry.path()) {
                languages.insert(name);
            }
        }
    }

    if languages.is_empty() {
        eprintln!("No languages installed in {}", data_dir.display());
        eprintln!("Use 'kakehashi language install <language>' to install one.");
        return Ok(());
    }

    eprintln!("Installed languages (data dir: {}):", data_dir.display());

    for lang in &languages {
        let parser_path = find_parser_file(&parser_dir, lang);
        let queries_path = queries_dir.join(lang);

        let parser_status = if parser_path.is_some() {
            "✓ parser"
        } else {
            "✗ parser"
        };

        let queries_status = if queries::query_install_is_complete(&queries_path) {
            "✓ queries"
        } else {
            "✗ queries (missing)"
        };

        println!("  {:<12} {}  {}", lang, parser_status, queries_status);

        if verbose {
            if let Some(ref p) = parser_path {
                println!("               parser: {}", p.display());
            }
            if queries::query_install_is_complete(&queries_path) {
                println!("               queries: {}", queries_path.display());
            }
        }
    }

    Ok(())
}

fn installed_query_language_name(path: &Path) -> Option<String> {
    if !path.is_dir() {
        return None;
    }
    let name = path.file_name()?.to_string_lossy();
    if name.starts_with('.') {
        return None;
    }
    if !queries::is_safe_language_name(&name) {
        return None;
    }
    Some(name.to_string())
}

/// Run the language uninstall command
fn run_language_uninstall(
    language: Option<String>,
    force: bool,
    all: bool,
) -> Result<(), ExitCode> {
    use std::collections::BTreeSet;
    use std::fs;
    use std::io::{self, Write};

    let data_dir = default_data_dir().ok_or_else(|| {
        eprintln!("Error: Could not determine data directory. Please specify --data-dir.");
        ExitCode::FAILURE
    })?;

    let parser_dir = data_dir.join("parser");
    let queries_dir = data_dir.join("queries");
    if let Err(e) = queries::recover_interrupted_query_installs(&queries_dir) {
        eprintln!("Warning: failed to recover interrupted query installs: {e}");
    }

    // Determine which languages to uninstall
    let languages_to_uninstall: Vec<String> = if all {
        // Collect all installed languages
        let mut languages = BTreeSet::new();

        if let Ok(entries) = fs::read_dir(&parser_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let is_parser = path
                    .extension()
                    .map(|ext| ext == std::env::consts::DLL_EXTENSION)
                    .unwrap_or(false);
                if is_parser && let Some(stem) = path.file_stem() {
                    languages.insert(stem.to_string_lossy().to_string());
                }
            }
        }

        if let Ok(entries) = fs::read_dir(&queries_dir) {
            for entry in entries.flatten() {
                if let Some(name) = installed_query_language_name(&entry.path()) {
                    languages.insert(name);
                }
            }
        }

        languages.into_iter().collect()
    } else {
        vec![language.expect("language required when --all not specified")]
    };

    if languages_to_uninstall.is_empty() {
        eprintln!("No languages installed to uninstall.");
        return Ok(());
    }

    // Confirmation prompt unless --force
    if !force {
        if all {
            eprint!(
                "Uninstall all {} languages? [y/N] ",
                languages_to_uninstall.len()
            );
        } else {
            eprint!("Uninstall '{}'? [y/N] ", languages_to_uninstall[0]);
        }
        io::stderr().flush().unwrap();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() || !input.trim().eq_ignore_ascii_case("y") {
            eprintln!("Cancelled.");
            return Ok(());
        }
    }

    // Uninstall each language
    let mut any_removed = false;
    let mut any_failed = false;
    for lang in &languages_to_uninstall {
        // Reject unsafe names before building any path from them: `lang` is
        // user input and feeds fs::remove_file via find_parser_file, so a
        // separator-carrying name must not escape the data dir.
        if !queries::is_safe_language_name(lang) {
            // Debug-format: untrusted input could smuggle ANSI escapes.
            eprintln!("✗ Invalid language name {:?}", lang);
            any_failed = true;
            continue;
        }

        let mut removed_something = false;

        // Remove parser file
        if let Some(parser_path) = find_parser_file(&parser_dir, lang) {
            match fs::remove_file(&parser_path) {
                Ok(()) => {
                    eprintln!("✓ Removed parser: {}", parser_path.display());
                    removed_something = true;
                }
                Err(e) => {
                    eprintln!("✗ Failed to remove parser {}: {}", parser_path.display(), e);
                    any_failed = true;
                }
            }
        }

        // Remove queries directory and any kakehashi-created backups under the
        // same lock used by install replacement, so uninstall cannot race a
        // concurrent install into resurrecting queries after reporting success.
        match queries::remove_query_install_and_backups(&queries_dir, lang) {
            Ok(removal) => {
                if removal.removed_queries {
                    eprintln!("✓ Removed queries: {}", queries_dir.join(lang).display());
                }
                if removal.removed_backups {
                    eprintln!("✓ Removed query backups for '{}'", lang);
                }
                removed_something |= removal.removed_anything();
            }
            Err(e) => {
                eprintln!("✗ Failed to remove queries for '{}': {}", lang, e);
                any_failed = true;
            }
        }

        if removed_something {
            any_removed = true;
        } else if !all {
            eprintln!("Language '{}' is not installed.", lang);
        }
    }

    if any_failed {
        return Err(ExitCode::FAILURE);
    }

    if any_removed {
        if all {
            eprintln!("\nUninstalled all languages.");
        } else {
            eprintln!("\nUninstalled '{}'.", languages_to_uninstall[0]);
        }
    }

    Ok(())
}

/// Find the parser file for a language.
fn find_parser_file(parser_dir: &std::path::Path, lang: &str) -> Option<PathBuf> {
    let path = parser_dir.join(format!("{}.{}", lang, std::env::consts::DLL_EXTENSION));
    if path.exists() { Some(path) } else { None }
}

/// Write content to stdout or a file, with --force / --output semantics.
fn write_content_to_output(
    content: &str,
    output: Option<PathBuf>,
    force: bool,
    label: &str,
) -> Result<(), ExitCode> {
    // Check for --force without --output (warn but continue)
    if force && output.is_none() {
        eprintln!("Warning: --force has no effect without --output");
    }

    if let Some(path) = output.as_ref().filter(|p| p.as_os_str() != "-") {
        let write_result = if force {
            write_forced_output(path, content)
        } else {
            use std::io::Write as _;

            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .and_then(|mut file| file.write_all(content.as_bytes()))
        };
        match write_result {
            Ok(()) => {
                eprintln!("Created {label} file: {}", path.display());
            }
            Err(e) if !force && e.kind() == std::io::ErrorKind::AlreadyExists => {
                eprintln!(
                    "Error: An entry already exists at '{}'. Use --force to overwrite.",
                    path.display()
                );
                return Err(ExitCode::FAILURE);
            }
            Err(e) => {
                eprintln!("Failed to write {label} file: {}", e);
                return Err(ExitCode::FAILURE);
            }
        }
    } else {
        print!("{}", content);
    }

    Ok(())
}

fn write_forced_output(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(path)?;
        file.write_all(content.as_bytes())
    }
    #[cfg(not(unix))]
    {
        #[cfg(windows)]
        {
            use std::io::Write as _;
            use std::os::windows::fs::OpenOptionsExt as _;
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(path)?;
            file.write_all(content.as_bytes())
        }
        #[cfg(not(windows))]
        std::fs::write(path, content)
    }
}

/// Run the config init command
fn run_config_init(output: Option<PathBuf>, force: bool) -> Result<(), ExitCode> {
    use kakehashi::config::defaults::default_settings;

    let settings = default_settings();
    let config_toml = toml::to_string_pretty(&settings).map_err(|e| {
        eprintln!("Failed to serialize configuration: {}", e);
        ExitCode::FAILURE
    })?;

    // Prepend documentation link
    let content = format!("{}\n{}", DOC_LINK, config_toml);

    write_content_to_output(&content, output, force, "configuration")
}

/// Run the config schema command
fn run_config_schema(output: Option<PathBuf>, force: bool) -> Result<(), ExitCode> {
    use kakehashi::config::json_schema;

    let schema = json_schema();
    let schema_json = serde_json::to_string_pretty(&schema).map_err(|e| {
        eprintln!("Failed to serialize schema: {}", e);
        ExitCode::FAILURE
    })?;
    let content = format!("{}\n", schema_json);

    write_content_to_output(&content, output, force, "schema")
}

/// Run the install command (synchronous - no tokio runtime)
fn run_install(language: &str, force: bool, verbose: bool, no_cache: bool) -> Result<(), ExitCode> {
    let data_dir = default_data_dir().ok_or_else(|| {
        eprintln!("Error: Could not determine data directory. Please specify --data-dir.");
        ExitCode::FAILURE
    })?;

    // Track success/failure for exit code
    let mut parser_success = true;
    let mut queries_success = true;

    if let Err(e) = queries::clear_uninstall_tombstone_for_install(&data_dir, language) {
        eprintln!("✗ Failed to prepare query installation: {}", e);
        queries_success = false;
    }

    // Install parser
    eprintln!("Installing parser for '{}' to {:?}...", language, data_dir);

    let options = parser::InstallOptions {
        data_dir: data_dir.clone(),
        force,
        verbose,
        no_cache,
        // The CLI runs from the kakehashi binary, so the killable subprocess path
        // is available and a hung cc is deadline-bounded.
        compile: parser::ParserCompile::KillableSubprocess,
    };

    match parser::install_parser(language, &options) {
        Ok(result) => {
            eprintln!("✓ Parser installed: {}", result.install_path.display());
            if verbose {
                eprintln!("  Revision: {}", result.revision);
            }
        }
        Err(e) => {
            eprintln!("✗ Parser installation failed: {}", e);
            parser_success = false;
        }
    }

    // Install queries (with inherited dependencies)
    eprintln!("Installing queries for '{}' to {:?}...", language, data_dir);

    match queries::install_queries_with_dependencies_after_install_started(
        language, &data_dir, force,
    ) {
        Ok(result) => {
            eprintln!("✓ Queries installed: {}", result.install_path.display());
            if verbose {
                eprintln!("  Files: {}", result.files_downloaded.join(", "));
            }
        }
        Err(e) => {
            eprintln!("✗ Query installation failed: {}", e);
            queries_success = false;
        }
    }

    // Summary
    if parser_success && queries_success {
        eprintln!("\nSuccessfully installed '{}' language support.", language);
        Ok(())
    } else if !parser_success && !queries_success {
        eprintln!("\nFailed to install '{}' language support.", language);
        Err(ExitCode::FAILURE)
    } else {
        eprintln!("\nPartially installed '{}' language support.", language);
        Err(ExitCode::FAILURE)
    }
}

/// Run the hidden `__compile-parser` subprocess entry: compile one grammar
/// in-process and exit (success → 0, failure → non-zero). Invoked by
/// `install::parser::compile_parser`, which runs this binary as a killable
/// subprocess so a hung `cc` can be deadline-killed.
fn run_compile_parser(
    grammar_dir: &std::path::Path,
    output_path: &std::path::Path,
) -> Result<(), ExitCode> {
    // Self-bound the compile so a parent crash mid-compile can't leave us (and a
    // hung cc) running as an orphan; the parent's deadline is still the usual
    // trigger.
    parser::arm_compile_watchdog();
    match parser::compile_parser_inprocess(grammar_dir, output_path) {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!("parser compile failed: {e}");
            Err(ExitCode::FAILURE)
        }
    }
}

/// Run the format command. Formatting goes through the same downstream
/// language-server bridge as LSP mode, so it builds its own tokio runtime
/// inside `cli::format::run`.
fn run_format(options: kakehashi::cli::format::FormatOptions) -> Result<(), ExitCode> {
    // Logging to stderr, configured via RUST_LOG — same posture as LSP mode
    // (stdout carries formatted output in stdin mode).
    env_logger::Builder::from_default_env()
        .target(env_logger::Target::Stderr)
        .init();

    let code = kakehashi::cli::format::run(options);
    if code == kakehashi::cli::format::EXIT_OK {
        Ok(())
    } else {
        Err(ExitCode::from(code))
    }
}

/// Run the diagnose command. Like `format`, diagnostics flow through the same
/// downstream language-server bridge as LSP mode, so it builds its own tokio
/// runtime inside `cli::diagnose::run`.
fn run_diagnose(options: kakehashi::cli::diagnose::DiagnoseOptions) -> Result<(), ExitCode> {
    // Logging to stderr, configured via RUST_LOG — diagnostics go to stdout.
    env_logger::Builder::from_default_env()
        .target(env_logger::Target::Stderr)
        .init();

    let code = kakehashi::cli::diagnose::run(options);
    if code == kakehashi::cli::diagnose::EXIT_OK {
        Ok(())
    } else {
        Err(ExitCode::from(code))
    }
}

/// Run the LSP server (requires tokio runtime)
#[tokio::main]
async fn run_lsp_server() {
    use env_logger::Builder;
    use kakehashi::lsp::{
        CancelForwarder, IngressOrderGate, Kakehashi, LanguageServerPool, RequestIdCapture,
    };
    use std::sync::Arc;
    use tokio::io::{stdin, stdout};
    use tower_lsp_server::{LspService, Server};

    // Initialize logging to stderr (CRITICAL: stdout is used for LSP JSON-RPC)
    // Configure via RUST_LOG, e.g.: RUST_LOG=kakehashi=debug
    Builder::from_default_env()
        .target(env_logger::Target::Stderr)
        .init();

    let stdin = stdin();
    let stdout = stdout();

    // Async-runtime stall watchdog: a detached task ticks every 100ms and
    // logs whenever its own wakeup was delayed — the definitive signal that
    // the tokio workers were wedged (a stalled watchdog with a fast compute
    // pool is what distinguishes "runtime starved" from "pool queued" in a
    // slow-response report). Debug-level, so it costs nothing unless a user
    // is already collecting diagnostics.
    tokio::spawn(async {
        const TICK: std::time::Duration = std::time::Duration::from_millis(100);
        let mut last = tokio::time::Instant::now();
        loop {
            tokio::time::sleep(TICK).await;
            let now = tokio::time::Instant::now();
            let lag = now.saturating_duration_since(last + TICK);
            if lag.as_millis() > 250 {
                log::debug!(
                    target: "kakehashi::runtime_watchdog",
                    "async runtime stalled: watchdog tick delayed by {}ms",
                    lag.as_millis()
                );
            }
            last = now;
        }
    });

    // Create shared pool and cancel forwarder
    // Both are shared between Kakehashi and the RequestIdCapture middleware:
    // - Pool: for downstream server connections
    // - CancelForwarder: for upstream cancel notification to handlers
    let pool = Arc::new(LanguageServerPool::new());
    let cancel_forwarder = CancelForwarder::new(Arc::clone(&pool));

    // Create Kakehashi with the shared pool and cancel forwarder
    let pool_for_service = Arc::clone(&pool);
    let forwarder_for_service = cancel_forwarder.clone();
    let (service, socket) = LspService::build(move |client| {
        Kakehashi::with_cancel_forwarder(
            client,
            Arc::clone(&pool_for_service),
            forwarder_for_service.clone(),
        )
    })
    .custom_method(
        "kakehashi/internal/effectiveConfiguration",
        Kakehashi::effective_configuration,
    )
    // Captures (captures-protocol) — semanticTokens-style triple over a
    // server-owned query kind.
    .custom_method(
        "kakehashi/captures/full",
        Kakehashi::kakehashi_captures_full,
    )
    .custom_method(
        "kakehashi/captures/full/delta",
        Kakehashi::kakehashi_captures_full_delta,
    )
    .custom_method(
        "kakehashi/captures/range",
        Kakehashi::kakehashi_captures_range,
    )
    .custom_method("kakehashi/node", Kakehashi::kakehashi_node)
    .custom_method("kakehashi/node/text", Kakehashi::kakehashi_node_text)
    .custom_method("kakehashi/node/parent", Kakehashi::kakehashi_node_parent)
    .custom_method(
        "kakehashi/node/children",
        Kakehashi::kakehashi_node_children,
    )
    // Scalar accessors (node-reference-protocol) — mirror tree-sitter `Node`.
    .custom_method("kakehashi/node/kind", Kakehashi::kakehashi_node_kind)
    .custom_method(
        "kakehashi/node/grammarName",
        Kakehashi::kakehashi_node_grammar_name,
    )
    .custom_method("kakehashi/node/isNamed", Kakehashi::kakehashi_node_is_named)
    .custom_method("kakehashi/node/isExtra", Kakehashi::kakehashi_node_is_extra)
    .custom_method(
        "kakehashi/node/hasError",
        Kakehashi::kakehashi_node_has_error,
    )
    .custom_method("kakehashi/node/isError", Kakehashi::kakehashi_node_is_error)
    .custom_method(
        "kakehashi/node/isMissing",
        Kakehashi::kakehashi_node_is_missing,
    )
    .custom_method(
        "kakehashi/node/startByte",
        Kakehashi::kakehashi_node_start_byte,
    )
    .custom_method("kakehashi/node/endByte", Kakehashi::kakehashi_node_end_byte)
    .custom_method(
        "kakehashi/node/byteRange",
        Kakehashi::kakehashi_node_byte_range,
    )
    .custom_method(
        "kakehashi/node/childCount",
        Kakehashi::kakehashi_node_child_count,
    )
    .custom_method(
        "kakehashi/node/namedChildCount",
        Kakehashi::kakehashi_node_named_child_count,
    )
    .custom_method(
        "kakehashi/node/descendantCount",
        Kakehashi::kakehashi_node_descendant_count,
    )
    .custom_method("kakehashi/node/toSexp", Kakehashi::kakehashi_node_to_sexp)
    // Tree-walking accessors (node-reference-protocol).
    .custom_method("kakehashi/node/child", Kakehashi::kakehashi_node_child)
    .custom_method(
        "kakehashi/node/namedChild",
        Kakehashi::kakehashi_node_named_child,
    )
    .custom_method(
        "kakehashi/node/namedChildren",
        Kakehashi::kakehashi_node_named_children,
    )
    .custom_method(
        "kakehashi/node/childWithDescendant",
        Kakehashi::kakehashi_node_child_with_descendant,
    )
    .custom_method(
        "kakehashi/node/nextSibling",
        Kakehashi::kakehashi_node_next_sibling,
    )
    .custom_method(
        "kakehashi/node/prevSibling",
        Kakehashi::kakehashi_node_prev_sibling,
    )
    .custom_method(
        "kakehashi/node/nextNamedSibling",
        Kakehashi::kakehashi_node_next_named_sibling,
    )
    .custom_method(
        "kakehashi/node/prevNamedSibling",
        Kakehashi::kakehashi_node_prev_named_sibling,
    )
    .custom_method(
        "kakehashi/node/firstChildForByte",
        Kakehashi::kakehashi_node_first_child_for_byte,
    )
    .custom_method(
        "kakehashi/node/descendantForByteRange",
        Kakehashi::kakehashi_node_descendant_for_byte_range,
    )
    .custom_method(
        "kakehashi/node/namedDescendantForByteRange",
        Kakehashi::kakehashi_node_named_descendant_for_byte_range,
    )
    // Position / range accessors (node-reference-protocol) — LSP Position (UTF-16).
    .custom_method("kakehashi/node/range", Kakehashi::kakehashi_node_range)
    .custom_method(
        "kakehashi/node/startPosition",
        Kakehashi::kakehashi_node_start_position,
    )
    .custom_method(
        "kakehashi/node/endPosition",
        Kakehashi::kakehashi_node_end_position,
    )
    .custom_method(
        "kakehashi/node/descendantForPointRange",
        Kakehashi::kakehashi_node_descendant_for_point_range,
    )
    .custom_method(
        "kakehashi/node/namedDescendantForPointRange",
        Kakehashi::kakehashi_node_named_descendant_for_point_range,
    )
    // Field-aware accessors (node-reference-protocol).
    .custom_method(
        "kakehashi/node/childByFieldName",
        Kakehashi::kakehashi_node_child_by_field_name,
    )
    .custom_method(
        "kakehashi/node/childrenByFieldName",
        Kakehashi::kakehashi_node_children_by_field_name,
    )
    .custom_method(
        "kakehashi/node/fieldNameForChild",
        Kakehashi::kakehashi_node_field_name_for_child,
    )
    .custom_method(
        "kakehashi/node/fieldNameForNamedChild",
        Kakehashi::kakehashi_node_field_name_for_named_child,
    )
    .finish();

    // Reap downstream servers when the editor terminates this process without
    // completing the shutdown handshake (SIGTERM/SIGHUP) — without this, the
    // spawned language servers are orphaned to launchd and can outlive the
    // session indefinitely.
    #[cfg(unix)]
    service.inner().spawn_termination_cleanup();

    // Wrap service with RequestIdCapture to:
    // 1. Capture upstream request IDs (for ls-bridge-server-pool-coordination bridge requests)
    // 2. Forward $/cancelRequest notifications to downstream servers
    let service = RequestIdCapture::with_cancel_forwarder(service, cancel_forwarder);

    // Outermost: assign per-document sequence tickets in wire order so
    // didChange/didClose apply strictly ordered and semanticTokens requests
    // observe every edit that preceded them on the wire (#342).
    let service = IngressOrderGate::new(service);

    // Lift tower-lsp's default 4-message `buffer_unordered` cap: editors fire
    // bursts of concurrent requests per keystroke (Neovim: semanticTokens +
    // captures lineages + diagnostics + …), and handlers park awaiting the
    // per-URI parse snapshot. With only 4 slots, parked readers exhaust the
    // buffer and the very didChange notifications that would release them
    // queue behind — a priority inversion observed as multi-second
    // handler-start delays. Ordering is IngressOrderGate's job (tickets are
    // assigned synchronously in wire order, independent of this value) and
    // CPU is the bounded ComputePool's, so a wider admission costs only
    // parked futures. This NARROWS the inversion rather than removing it:
    // the wedge threshold becomes INGRESS_CONCURRENCY + tower-lsp's 100-slot
    // channel queue of outstanding messages, and a wedge self-heals within
    // the parked readers' settle backstop. `$/cancelRequest` is immune either
    // way — `RequestIdCapture::call` dispatches its forwarding as a detached
    // fire-and-forget spawn and needs no admission slot to do so. Sized for
    // the worst observed per-keystroke
    // burst (≈10 concurrent reader parks per document) across several
    // documents, with headroom.
    const INGRESS_CONCURRENCY: usize = 64;
    Server::new(stdin, stdout, socket)
        .concurrency_level(INGRESS_CONCURRENCY)
        .serve(service)
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_query_language_name_filters_unsafe_dirs() {
        let temp = tempfile::TempDir::new().unwrap();
        let safe = temp.path().join("lua");
        let unsafe_name = temp.path().join("foo.bar");
        let hidden = temp.path().join(".lua");
        std::fs::create_dir_all(&safe).unwrap();
        std::fs::create_dir_all(&unsafe_name).unwrap();
        std::fs::create_dir_all(&hidden).unwrap();

        assert_eq!(
            installed_query_language_name(&safe),
            Some("lua".to_string())
        );
        assert_eq!(installed_query_language_name(&unsafe_name), None);
        assert_eq!(installed_query_language_name(&hidden), None);
    }

    #[cfg(unix)]
    #[test]
    fn output_without_force_rejects_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new().unwrap();
        let redirected = temp.path().join("redirected.toml");
        let output = temp.path().join("config.toml");
        symlink(&redirected, &output).unwrap();

        let result =
            write_content_to_output("generated", Some(output.clone()), false, "configuration");

        assert!(result.is_err());
        assert!(output.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(!redirected.exists());
    }

    #[test]
    fn output_without_force_preserves_existing_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let output = temp.path().join("config.toml");
        std::fs::write(&output, "existing").unwrap();

        let result =
            write_content_to_output("generated", Some(output.clone()), false, "configuration");

        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(output).unwrap(), "existing");
    }

    #[cfg(unix)]
    #[test]
    fn force_output_rejects_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("shared.toml");
        let output = temp.path().join("config.toml");
        std::fs::write(&target, "shared").unwrap();
        symlink(&target, &output).unwrap();

        let result =
            write_content_to_output("generated", Some(output.clone()), true, "configuration");

        assert!(result.is_err());
        assert!(output.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "shared");
    }

    #[cfg(windows)]
    #[test]
    fn force_output_rejects_windows_symlink_without_touching_target() {
        use std::os::windows::fs::symlink_file;

        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("shared.toml");
        let output = temp.path().join("config.toml");
        std::fs::write(&target, "shared").unwrap();
        if let Err(error) = symlink_file(&target, &output) {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create test symlink: {error}");
        }

        let result = write_content_to_output("generated", Some(output), true, "configuration");
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "shared");
    }
}
