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

/// Check if a language should be skipped during auto-install because it's not supported.
///
/// Skips (returns `should_skip = true`) when the language is unsupported by
/// nvim-treesitter or when metadata can't be fetched within the timeout. Uses
/// cached metadata to avoid repeated HTTP requests.
pub async fn should_skip_unsupported_language(
    language: &str,
    options: Option<&FetchOptions<'_>>,
) -> (bool, Option<SkipReason>) {
    should_skip_unsupported_language_with_checker(
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

async fn should_skip_unsupported_language_with_checker<F>(
    language: &str,
    options: Option<&FetchOptions<'_>>,
    timeout: Duration,
    check_fn: F,
) -> (bool, Option<SkipReason>)
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
                Ok(Ok(true)) => (false, None),
                Ok(Ok(false)) => (
                    true,
                    Some(SkipReason::UnsupportedLanguage {
                        language: owned_language,
                    }),
                ),
                Ok(Err(error)) => (
                    true,
                    Some(SkipReason::MetadataUnavailable {
                        language: owned_language,
                        error,
                    }),
                ),
                Err(err) => (
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
            // The task is still running; abort to avoid leaking blocking work
            // and report the timeout to the caller.
            check.abort();
            (
                true,
                Some(SkipReason::MetadataUnavailable {
                    language: owned_language,
                    error: MetadataError::Timeout,
                }),
            )
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

        let (should_skip, reason) =
            should_skip_unsupported_language("fake_lang_xyz", Some(&options)).await;
        assert!(
            should_skip,
            "Expected to skip unsupported language 'fake_lang_xyz'"
        );
        let reason = reason.expect("Expected a reason for skipping");
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

        let (should_skip, reason) = should_skip_unsupported_language("lua", Some(&options)).await;
        assert!(
            !should_skip,
            "Expected NOT to skip supported language 'lua'"
        );
        assert!(reason.is_none(), "Expected no reason when not skipping");
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

        let (should_skip, reason) = should_skip_unsupported_language("lua", Some(&options)).await;
        assert!(
            should_skip,
            "Metadata errors should prevent auto-install attempts"
        );
        assert!(
            matches!(reason, Some(SkipReason::MetadataUnavailable { .. })),
            "Expected MetadataUnavailable reason"
        );
    }

    #[tokio::test]
    async fn test_should_skip_unsupported_language_times_out_and_skips() {
        let (should_skip, reason) = should_skip_unsupported_language_with_checker(
            "lua",
            None,
            Duration::from_millis(20),
            |lang, _options| {
                std::thread::sleep(Duration::from_millis(50));
                Ok(lang == "lua")
            },
        )
        .await;

        assert!(should_skip, "Timeouts should skip auto-install attempts");
        assert!(
            matches!(reason, Some(SkipReason::MetadataUnavailable { language, .. }) if language == "lua"),
            "Timeouts should report metadata unavailable for the language"
        );
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
