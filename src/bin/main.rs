use clap::{Parser, Subcommand};
use kakehashi::install::{default_data_dir, metadata, parser, queries};
use std::path::PathBuf;
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

    /// Config file(s) to use instead of default locations (LSP mode only).
    /// Can be specified multiple times; files merge in order.
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
        #[arg(long)]
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

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Set data directory override so default_data_dir() and config expansion
    // all resolve consistently from this single flag
    if let Some(ref dir) = cli.data_dir {
        kakehashi::config::set_data_dir_override(dir.clone());
    }

    if !cli.config_file.is_empty() {
        kakehashi::config::set_config_file_override(cli.config_file);
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
        None => {
            // Start LSP server (backward compatible default behavior)
            // Only create tokio runtime for LSP mode to avoid conflicts with reqwest::blocking
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
            let path = entry.path();
            if path.is_dir()
                && let Some(name) = path.file_name()
            {
                languages.insert(name.to_string_lossy().to_string());
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
        let queries_path = queries_dir.join(lang).join("highlights.scm");

        let parser_status = if parser_path.is_some() {
            "✓ parser"
        } else {
            "✗ parser"
        };

        let queries_status = if queries_path.exists() {
            "✓ queries"
        } else {
            "✗ queries (missing)"
        };

        println!("  {:<12} {}  {}", lang, parser_status, queries_status);

        if verbose {
            if let Some(ref p) = parser_path {
                println!("               parser: {}", p.display());
            }
            if queries_path.exists() {
                println!(
                    "               queries: {}",
                    queries_path.parent().unwrap().display()
                );
            }
        }
    }

    Ok(())
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
                let path = entry.path();
                if path.is_dir()
                    && let Some(name) = path.file_name()
                {
                    languages.insert(name.to_string_lossy().to_string());
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
    for lang in &languages_to_uninstall {
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
                }
            }
        }

        // Remove queries directory
        let queries_path = queries_dir.join(lang);
        if queries_path.exists() {
            match fs::remove_dir_all(&queries_path) {
                Ok(()) => {
                    eprintln!("✓ Removed queries: {}", queries_path.display());
                    removed_something = true;
                }
                Err(e) => {
                    eprintln!(
                        "✗ Failed to remove queries {}: {}",
                        queries_path.display(),
                        e
                    );
                }
            }
        }

        if removed_something {
            any_removed = true;
        } else if !all {
            eprintln!("Language '{}' is not installed.", lang);
        }
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

    match output {
        None => {
            print!("{}", content);
        }
        Some(path) if path.as_os_str() == "-" => {
            print!("{}", content);
        }
        Some(path) => {
            if path.exists() && !force {
                eprintln!(
                    "Error: File '{}' already exists. Use --force to overwrite.",
                    path.display()
                );
                return Err(ExitCode::FAILURE);
            }

            match std::fs::write(&path, content) {
                Ok(()) => {
                    eprintln!("Created {label} file: {}", path.display());
                }
                Err(e) => {
                    eprintln!("Failed to write {label} file: {}", e);
                    return Err(ExitCode::FAILURE);
                }
            }
        }
    }

    Ok(())
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

    // Install parser
    eprintln!("Installing parser for '{}' to {:?}...", language, data_dir);

    let options = parser::InstallOptions {
        data_dir: data_dir.clone(),
        force,
        verbose,
        no_cache,
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

    match queries::install_queries_with_dependencies(language, &data_dir, force) {
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

/// Run the LSP server (requires tokio runtime)
#[tokio::main]
async fn run_lsp_server() {
    use env_logger::Builder;
    use kakehashi::lsp::{CancelForwarder, Kakehashi, LanguageServerPool, RequestIdCapture};
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
    .finish();

    // Wrap service with RequestIdCapture to:
    // 1. Capture upstream request IDs (for ADR-0016 bridge requests)
    // 2. Forward $/cancelRequest notifications to downstream servers
    let service = RequestIdCapture::with_cancel_forwarder(service, cancel_forwarder);

    Server::new(stdin, stdout, socket).serve(service).await;
}
