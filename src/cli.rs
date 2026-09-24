//! CLI-mode commands that reuse the LSP server's machinery in-process.
//!
//! Unlike the thin install/config subcommands in `src/bin/main.rs`, the
//! commands here need the full server stack (config loading, parser
//! registry, injection resolution, downstream language-server bridge), so
//! they live in the library crate where `pub(crate)` internals are reachable.

pub mod diagnose;
pub(crate) mod files;
pub mod format;

/// Drain server→client traffic so `Client` calls never block while a CLI
/// command drives the LSP handlers in-process: notifications (logMessage etc.)
/// are dropped — the CLI reports its own progress — and the rare server→client
/// *request* (e.g. workDoneProgress/create) is answered with `null` so the
/// awaiting handler proceeds. Shared by `format` and `diagnose`.
pub(crate) fn spawn_client_pump(socket: tower_lsp_server::ClientSocket) {
    use futures::{SinkExt, StreamExt};
    use tower_lsp_server::jsonrpc::Response;

    let (mut requests, mut responses) = socket.split();
    tokio::spawn(async move {
        while let Some(request) = requests.next().await {
            let (_method, id, _params) = request.into_parts();
            if let Some(id) = id {
                let _ = responses
                    .send(Response::from_parts(id, Ok(serde_json::Value::Null)))
                    .await;
            }
        }
    });
}
