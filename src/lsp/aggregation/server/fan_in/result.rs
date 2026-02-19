//! Fan-in result type shared by all collection strategies.
//!
//! [`FanInResult`] represents the outcome of aggregating concurrent bridge
//! requests. It is produced by all strategies (preferred, collect-all) and
//! consumed by handler code via the [`handle()`](FanInResult::handle) method.

/// Result of fan-in dispatch (used by all strategies).
#[derive(Debug)]
#[must_use]
pub(crate) enum FanInResult<T> {
    /// A result was produced by the dispatch.
    Done(T),
    /// All tasks completed without producing a result.
    ///
    /// `errors` counts tasks that failed with panics (`JoinError`) or I/O errors.
    /// Handlers use this to choose log severity: `WARNING` when `errors > 0`
    /// (real failures), `LOG` when `errors == 0` (all servers returned empty — normal).
    NoResult {
        /// Number of tasks that failed with errors (panics or IO errors).
        errors: usize,
    },
    /// The upstream client cancelled the request via `$/cancelRequest`.
    Cancelled,
}

impl<T> FanInResult<T> {
    /// Handle the common post-dispatch pattern for fan-in results.
    ///
    /// - `Done`: calls `on_done` to transform the value into the handler's return type.
    /// - `NoResult`: logs at WARNING (if errors > 0) or LOG (if errors == 0),
    ///   then returns `Ok(no_result)`.
    /// - `Cancelled`: returns `Err(Error::request_cancelled())`.
    pub(crate) async fn handle<R>(
        self,
        client: &tower_lsp_server::Client,
        method_name: &str,
        no_result: R,
        on_done: impl FnOnce(T) -> tower_lsp_server::jsonrpc::Result<R>,
    ) -> tower_lsp_server::jsonrpc::Result<R> {
        match self {
            FanInResult::Done(value) => on_done(value),
            FanInResult::NoResult { errors } => {
                let level = if errors > 0 {
                    tower_lsp_server::ls_types::MessageType::WARNING
                } else {
                    tower_lsp_server::ls_types::MessageType::LOG
                };
                client
                    .log_message(
                        level,
                        format!("No {method_name} response from any bridge server"),
                    )
                    .await;
                Ok(no_result)
            }
            FanInResult::Cancelled => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
        }
    }
}
