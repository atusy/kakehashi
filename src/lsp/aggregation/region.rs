//! Region-level aggregation with cancel support.
//!
//! [`collect_region_results_with_cancel()`] collects results from all regions
//! of an outer `JoinSet`, aborting immediately on cancel.

use tokio::task::JoinSet;
use tower_lsp_server::jsonrpc::{Error, Result};

use crate::lsp::request_id::CancelReceiver;

/// Collect results from all regions, aborting immediately if cancelled.
///
/// Uses `tokio::select!` with biased mode to prioritize cancel handling.
/// When cancelled:
/// - Returns `RequestCancelled` error immediately
/// - Drops the JoinSet, which aborts all spawned tasks
///
/// When all regions complete:
/// - Returns aggregated results from all successful regions
///
/// The `extend_from` closure is called for each successful join result,
/// allowing callers to extract items from the join result type `JR` and
/// extend the accumulator.
///
/// If `cancel_rx` is `None`, cancel handling is disabled (graceful degradation
/// when subscription failed due to `AlreadySubscribedError`).
pub(crate) async fn collect_region_results_with_cancel<JR, T, F>(
    mut join_set: JoinSet<JR>,
    cancel_rx: Option<CancelReceiver>,
    mut extend_from: F,
) -> Result<Vec<T>>
where
    JR: Send + 'static,
    T: Send,
    F: FnMut(&mut Vec<T>, JR),
{
    let mut results: Vec<T> = Vec::new();

    let Some(cancel_rx) = cancel_rx else {
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(jr) => extend_from(&mut results, jr),
                Err(join_err) => {
                    log::warn!("region task panicked: {join_err}");
                }
            }
        }
        return Ok(results);
    };

    tokio::pin!(cancel_rx);

    loop {
        tokio::select! {
            biased;
            _ = &mut cancel_rx => {
                return Err(Error::request_cancelled());
            }
            result = join_set.join_next() => {
                match result {
                    Some(Ok(jr)) => extend_from(&mut results, jr),
                    Some(Err(join_err)) => {
                        log::warn!("region task panicked: {join_err}");
                    }
                    None => break,
                }
            }
        }
    }

    Ok(results)
}
