//! Language support checking with timeout protection.
//!
//! This module provides async support checking for auto-install workflows,
//! wrapping the synchronous `is_language_supported` with timeout handling
//! to keep the LSP responsive.

use std::path::PathBuf;
use std::time::Duration;

use tower_lsp_server::ls_types::MessageType;

use super::metadata::{FetchOptions, MetadataError, is_language_supported};

/// Reason why a language was skipped during auto-install.
#[derive(Debug)]
pub enum SkipReason {
    /// Language is not supported by nvim-treesitter.
    UnsupportedLanguage { language: String },
    /// Metadata could not be fetched or verified.
    MetadataUnavailable {
        language: String,
        error: MetadataError,
    },
}

impl SkipReason {
    /// Get a human-readable message explaining why the language was skipped.
    pub fn message(&self) -> String {
        match self {
            SkipReason::UnsupportedLanguage { language } => format!(
                "Language '{}' is not supported by nvim-treesitter. Skipping auto-install.",
                language
            ),
            SkipReason::MetadataUnavailable { language, error } => format!(
                "Could not verify support for '{}' due to metadata error: {}. Skipping auto-install.",
                language, error
            ),
        }
    }

    /// Get the LSP message type appropriate for this skip reason.
    pub fn message_type(&self) -> MessageType {
        match self {
            SkipReason::UnsupportedLanguage { .. } => MessageType::INFO,
            SkipReason::MetadataUnavailable { .. } => MessageType::WARNING,
        }
    }
}

// Default timeout for metadata support checks; keeps the LSP path responsive
const METADATA_CHECK_TIMEOUT: Duration = Duration::from_secs(65);

/// Support-check result plus work that outlived the timeout.
///
/// `completion` is present when the check times out. It finishes after the
/// blocking task is cancelled before starting or, if already running, after
/// the lookup exits. Auto-install retains its per-language claim until then.
pub(crate) struct TrackedSupportCheck {
    pub(crate) should_skip: bool,
    pub(crate) reason: Option<SkipReason>,
    pub(crate) completion: Option<tokio::task::JoinHandle<()>>,
}

impl TrackedSupportCheck {
    pub(crate) fn completed(should_skip: bool, reason: Option<SkipReason>) -> Self {
        Self {
            should_skip,
            reason,
            completion: None,
        }
    }
}

/// Check support while retaining completion for blocking work that times out.
pub(crate) async fn should_skip_unsupported_language_tracked(
    language: &str,
    options: Option<&FetchOptions<'_>>,
) -> TrackedSupportCheck {
    should_skip_unsupported_language_with_checker_tracked(
        language,
        options,
        METADATA_CHECK_TIMEOUT,
        default_support_check,
    )
    .await
}

#[derive(Debug, Clone)]
struct FetchOptionsOwned {
    data_dir: Option<PathBuf>,
    use_cache: bool,
}

impl From<&FetchOptions<'_>> for FetchOptionsOwned {
    fn from(options: &FetchOptions<'_>) -> Self {
        Self {
            data_dir: options.data_dir.map(PathBuf::from),
            use_cache: options.use_cache,
        }
    }
}

impl FetchOptionsOwned {
    fn as_borrowed(&self) -> FetchOptions<'_> {
        FetchOptions {
            data_dir: self.data_dir.as_deref(),
            use_cache: self.use_cache,
        }
    }
}

fn default_support_check(
    language: String,
    options: Option<FetchOptionsOwned>,
) -> Result<bool, MetadataError> {
    let options = options.as_ref().map(FetchOptionsOwned::as_borrowed);
    is_language_supported(&language, options.as_ref())
}

async fn should_skip_unsupported_language_with_checker_tracked<F>(
    language: &str,
    options: Option<&FetchOptions<'_>>,
    timeout: Duration,
    check_fn: F,
) -> TrackedSupportCheck
where
    F: FnOnce(String, Option<FetchOptionsOwned>) -> Result<bool, MetadataError> + Send + 'static,
{
    let owned_language = language.to_string();
    let owned_options = options.map(FetchOptionsOwned::from);

    let language_for_check = owned_language.clone();
    let mut check =
        tokio::task::spawn_blocking(move || check_fn(language_for_check, owned_options));
    let timeout_fut = tokio::time::sleep(timeout);
    tokio::pin!(timeout_fut);

    tokio::select! {
        result = &mut check => {
            match result {
                Ok(Ok(true)) => TrackedSupportCheck::completed(false, None),
                Ok(Ok(false)) => TrackedSupportCheck::completed(
                    true,
                    Some(SkipReason::UnsupportedLanguage {
                        language: owned_language,
                    }),
                ),
                Ok(Err(error)) => TrackedSupportCheck::completed(
                    true,
                    Some(SkipReason::MetadataUnavailable {
                        language: owned_language,
                        error,
                    }),
                ),
                Err(err) => TrackedSupportCheck::completed(
                    true,
                    Some(SkipReason::MetadataUnavailable {
                        language: owned_language,
                        error: MetadataError::TaskFailure(format!(
                            "Metadata support check task failed: {}",
                            err
                        )),
                    }),
                ),
            }
        }
        _ = &mut timeout_fut => {
            // Abort prevents a queued blocking task from starting. Once a
            // blocking closure has started Tokio cannot stop it, so expose a
            // completion waiter to callers that must retain an in-flight claim.
            check.abort();
            let completion = tokio::spawn(async move {
                let _ = check.await;
            });
            TrackedSupportCheck {
                should_skip: true,
                reason: Some(SkipReason::MetadataUnavailable {
                    language: owned_language,
                    error: MetadataError::Timeout,
                }),
                completion: Some(completion),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_should_skip_unsupported_language_returns_true_for_unsupported() {
        use crate::install::test_helpers::setup_mock_metadata_cache;
        use tempfile::tempdir;

        let temp = tempdir().expect("Failed to create temp dir");

        let mock_parsers_lua = r#"
return {
  lua = {
    install_info = {
      revision = 'abc123',
      url = 'https://github.com/MunifTanjim/tree-sitter-lua',
    },
    tier = 2,
  },
}
"#;
        setup_mock_metadata_cache(temp.path(), mock_parsers_lua);

        let options = FetchOptions {
            data_dir: Some(temp.path()),
            use_cache: true,
        };

        let result =
            should_skip_unsupported_language_tracked("fake_lang_xyz", Some(&options)).await;
        assert!(
            result.should_skip,
            "Expected to skip unsupported language 'fake_lang_xyz'"
        );
        let reason = result.reason.expect("Expected a reason for skipping");
        assert!(
            matches!(reason, SkipReason::UnsupportedLanguage { language } if language == "fake_lang_xyz"),
            "Expected UnsupportedLanguage reason"
        );
    }

    #[tokio::test]
    async fn test_should_skip_unsupported_language_returns_false_for_supported() {
        use crate::install::test_helpers::setup_mock_metadata_cache;
        use tempfile::tempdir;

        let temp = tempdir().expect("Failed to create temp dir");

        let mock_parsers_lua = r#"
return {
  lua = {
    install_info = {
      revision = 'abc123',
      url = 'https://github.com/MunifTanjim/tree-sitter-lua',
    },
    tier = 2,
  },
}
"#;
        setup_mock_metadata_cache(temp.path(), mock_parsers_lua);

        let options = FetchOptions {
            data_dir: Some(temp.path()),
            use_cache: true,
        };

        let result = should_skip_unsupported_language_tracked("lua", Some(&options)).await;
        assert!(
            !result.should_skip,
            "Expected NOT to skip supported language 'lua'"
        );
        assert!(
            result.reason.is_none(),
            "Expected no reason when not skipping"
        );
    }

    #[tokio::test]
    async fn test_should_skip_unsupported_language_reports_metadata_error() {
        use crate::install::test_helpers::setup_mock_metadata_cache;
        use tempfile::tempdir;

        let temp = tempdir().expect("Failed to create temp dir");
        setup_mock_metadata_cache(temp.path(), "return {}");

        let options = FetchOptions {
            data_dir: Some(temp.path()),
            use_cache: true,
        };

        let result = should_skip_unsupported_language_tracked("lua", Some(&options)).await;
        assert!(
            result.should_skip,
            "Metadata errors should prevent auto-install attempts"
        );
        assert!(
            matches!(result.reason, Some(SkipReason::MetadataUnavailable { .. })),
            "Expected MetadataUnavailable reason"
        );
    }

    #[tokio::test]
    async fn test_should_skip_unsupported_language_times_out_and_skips() {
        let result = should_skip_unsupported_language_with_checker_tracked(
            "lua",
            None,
            Duration::from_millis(20),
            |lang, _options| {
                std::thread::sleep(Duration::from_millis(50));
                Ok(lang == "lua")
            },
        )
        .await;

        assert!(
            result.should_skip,
            "Timeouts should skip auto-install attempts"
        );
        assert!(
            matches!(result.reason, Some(SkipReason::MetadataUnavailable { language, .. }) if language == "lua"),
            "Timeouts should report metadata unavailable for the language"
        );
    }

    #[tokio::test]
    async fn timed_out_blocking_check_exposes_its_completion() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let mut result = should_skip_unsupported_language_with_checker_tracked(
            "lua",
            None,
            Duration::from_millis(20),
            move |_lang, _options| {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
                Ok(true)
            },
        )
        .await;

        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocking checker must start before timing out");
        let completion = result
            .completion
            .take()
            .expect("a timed-out blocking checker must expose completion");
        assert!(!completion.is_finished());

        let _ = release_tx.send(());
        completion.await.expect("completion waiter must finish");
    }

    #[test]
    fn skip_reason_reports_message_type() {
        let unsupported = SkipReason::UnsupportedLanguage {
            language: "lua".into(),
        };
        assert_eq!(unsupported.message_type(), MessageType::INFO);

        let metadata_err = SkipReason::MetadataUnavailable {
            language: "lua".into(),
            error: MetadataError::HttpError("boom".into()),
        };
        assert_eq!(metadata_err.message_type(), MessageType::WARNING);
    }
}
