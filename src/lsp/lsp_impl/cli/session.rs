//! Shared CLI session lifecycle for both `kakehashi` subcommands.
//!
//! Sets up, gates the readiness of, and tears down the in-process LSP session
//! that the per-command wrappers ([`super::format`], [`super::diagnose`]) run
//! within: `cli_initialize` / `cli_shutdown`, the directory-walk path filter,
//! and the cold-start readiness waits. These need the private `language` /
//! `documents` / `bridge` fields, so they live under `lsp_impl` rather than
//! `crate::cli`.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use tower_lsp_server::ls_types::{InitializeParams, InitializedParams, WorkspaceFolder};
use url::Url;

use crate::language::InjectionResolver;

use super::super::{Kakehashi, url_to_uri};

impl Kakehashi {
    /// Run the LSP initialize/initialized lifecycle for CLI mode, loading
    /// configuration from `root` (typically the current directory) exactly
    /// like an editor session rooted there would.
    pub(crate) async fn cli_initialize(&self, root: &Path) {
        // One-shot CLI: no editor consumes proactive publishDiagnostics, so
        // did_open_impl skips the synthetic diagnostic task (#489).
        self.mark_cli_mode();
        let workspace_folders = Url::from_file_path(root)
            .ok()
            .and_then(|url| url_to_uri(&url).ok())
            .map(|uri| {
                vec![WorkspaceFolder {
                    uri,
                    name: root
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "workspace".to_string()),
                }]
            });
        let params = InitializeParams {
            workspace_folders,
            ..Default::default()
        };
        if let Err(e) = self.initialize_impl(params).await {
            log::warn!(target: "kakehashi::cli", "initialize failed: {e}");
        }
        self.initialized_impl(InitializedParams {}).await;
    }

    /// Whether the path alone identifies a known language (loading the
    /// parser if installed but not yet loaded). Used to filter directory
    /// walks for both `format` and `diagnose`; explicit files skip this
    /// (content-based detection still applies when they are opened). Lossy
    /// conversion keeps non-UTF-8 paths eligible for extension matching,
    /// consistent with [`Self::cli_format_text`].
    pub(crate) fn cli_can_handle_path(&self, path: &Path) -> bool {
        self.language
            .loadable_language_for_path(&path.to_string_lossy())
            .is_some()
    }

    /// Whether a file discovered during a directory walk identifies a known
    /// language by path or by a bounded prefix of its first line. Only path
    /// misses read content. The hard cap keeps ordinary directory traversal
    /// from scanning arbitrarily large extensionless files; explicitly named
    /// files remain authoritative for mode markers beyond the cap. Files whose
    /// first-line I/O fails are retained so the command's normal read path can
    /// report the operational error; invalid UTF-8 is filtered as non-text.
    pub(crate) fn cli_can_handle_discovered_file(&self, path: &Path) -> bool {
        use std::io::Read as _;

        if self.cli_can_handle_path(path) {
            return true;
        }
        let Ok(file) = std::fs::File::open(path) else {
            // Keep the candidate so the command's normal read path reports
            // the operational error instead of silently treating it as an
            // unsupported language.
            return true;
        };
        const MAX_FIRST_LINE_BYTES: usize = 8 * 1024;
        let path = path.to_string_lossy();
        let mut probe = Vec::with_capacity(MAX_FIRST_LINE_BYTES + 1);
        if std::io::BufReader::new(file)
            .take((MAX_FIRST_LINE_BYTES + 1) as u64)
            .read_to_end(&mut probe)
            .is_err()
        {
            return true;
        }
        let truncated = probe.len() > MAX_FIRST_LINE_BYTES;
        probe.truncate(MAX_FIRST_LINE_BYTES);
        let valid = match std::str::from_utf8(&probe) {
            Ok(valid) => valid,
            Err(error) if truncated && error.error_len().is_none() => {
                std::str::from_utf8(&probe[..error.valid_up_to()])
                    .expect("valid_up_to always identifies valid UTF-8")
            }
            Err(_) => return false,
        };
        self.language
            .loadable_language_for_document(&path, valid)
            .is_some()
    }

    /// Gracefully shut down downstream language servers (LSP shutdown/exit
    /// handshake, escalating for unresponsive ones).
    pub(crate) async fn cli_shutdown(&self) {
        let _ = self.shutdown_impl().await;
    }

    /// Wait (up to `timeout`) until the eager-open tasks `didOpen` spawned
    /// for this document have finished.
    ///
    /// An eager-open task claims each region's virtual document BEFORE its
    /// `didOpen` reaches the writer queue, so a request issued in that window
    /// skips its own `didOpen` (already claimed) and overtakes the eager one
    /// on the wire — the downstream server then sees an unknown URI and
    /// answers `null`, which the preferred fan-in accepts as authoritative
    /// (e.g. "no edits" for formatting, "no diagnostics" for diagnose).
    /// Editors retry past this cold-start race; a one-shot CLI run must
    /// instead wait the tasks out: once finished, every `didOpen` is enqueued
    /// and the single-writer FIFO keeps the request behind them.
    ///
    /// Returns `false` when the deadline expired with tasks still pending —
    /// the caller reports that as a failure rather than issuing into the race.
    pub(crate) async fn wait_eager_open_finished(&self, uri: &Url, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while !self.bridge.eager_open_tasks_finished(uri) {
            if tokio::time::Instant::now() >= deadline {
                log::warn!(
                    target: "kakehashi::cli",
                    "eager-open tasks for {uri} did not finish within {timeout:?}; \
                     the request may race their didOpen"
                );
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        true
    }

    /// Wait (up to `timeout` per server) until every downstream server
    /// configured for the document — injection-region (virt) servers and,
    /// when `bridge._self.enabled`, host-layer servers — is Ready, returning
    /// a description of each server that failed to come up.
    ///
    /// `method` is the LSP request the caller is about to make (e.g.
    /// `textDocument/formatting` or `textDocument/diagnostic`); it gates
    /// host-layer participation, since `bridge._self` opts in per method.
    ///
    /// The relevant handler treats a not-yet-ready server as a failed step
    /// and skips it — correct for an editor where the next request retries,
    /// but a one-shot CLI run would silently produce no result. Host requests
    /// do run their own handshake wait inside `get_or_create_connection`, but
    /// that time is spent inside the request's own budget — pre-warming here
    /// keeps a slow cold host server from surfacing as a spurious request
    /// failure. Spawning is idempotent
    /// (`get_or_create_connection_wait_ready`), so this overlaps with the
    /// eager-open `didOpen` triggered.
    pub(crate) async fn wait_bridge_servers_ready(
        &self,
        uri: &Url,
        method: &str,
        timeout: Duration,
    ) -> Vec<String> {
        let mut configs = Vec::new();

        // Virt layer: servers configured for the document's injection regions.
        if let Some(language_name) = self.document_language(uri)
            && let Some(injection_query) = self.language.injection_query(&language_name)
            && let Some(snapshot) = self.documents.get(uri).and_then(|doc| doc.snapshot())
        {
            let regions = InjectionResolver::resolve_all(
                &self.language,
                self.bridge.node_tracker(),
                uri,
                snapshot.tree(),
                snapshot.text(),
                injection_query.as_ref(),
                snapshot.incarnation(),
            );
            for resolved in regions {
                configs.extend(self.bridge_configs_for_injection_language(
                    &language_name,
                    &resolved.injection_language,
                ));
            }
        }

        // Host layer (bridge._self): resolves to nothing unless opted in.
        if let Ok(lsp_uri) = url_to_uri(uri)
            && let Some(ctx) = self.resolve_host_bridge_context(&lsp_uri, method)
        {
            configs.extend(ctx.configs);
        }

        let pool = self.bridge.pool_arc();
        let mut waited: HashSet<String> = HashSet::new();
        let mut ready_waits = Vec::new();
        for config in configs {
            if waited.insert(config.server_name.clone()) {
                let pool = std::sync::Arc::clone(&pool);
                ready_waits.push(async move {
                    let result = pool
                        .get_or_create_connection_wait_ready(
                            &config.server_name,
                            &config.config,
                            Some(uri),
                            timeout,
                        )
                        .await;
                    (config.server_name, result)
                });
            }
        }

        // Wait concurrently so several broken servers cost one `timeout`
        // total, not one each.
        let mut failures = Vec::new();
        for (server_name, result) in futures::future::join_all(ready_waits).await {
            if let Err(e) = result {
                log::warn!(
                    target: "kakehashi::cli",
                    "downstream server {server_name} not ready: {e}"
                );
                failures.push(format!(
                    "downstream server '{server_name}' failed to start: {e}"
                ));
            }
        }
        failures
    }
}
