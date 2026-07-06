//! Runtime opt-in for experimental features.
//!
//! Experimental features (currently the native lexical-resolution layer
//! and documentColor / colorPresentation bridging) ship in every binary but
//! stay dormant unless the server process is started with
//! `KAKEHASHI_EXPERIMENTAL=true`. The variable is read once
//! per process; consumers that need per-instance test control (e.g.
//! [`crate::lsp::Kakehashi`]) copy the value at construction instead of
//! re-reading it.

use std::sync::OnceLock;

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Whether experimental features are enabled for this process
/// (`KAKEHASHI_EXPERIMENTAL=true`; read once, then cached).
pub(crate) fn enabled() -> bool {
    *ENABLED.get_or_init(|| parse(std::env::var("KAKEHASHI_EXPERIMENTAL").ok().as_deref()))
}

/// Only the exact value `true` opts in — an unset, empty, or any other
/// value keeps experimental features off.
fn parse(value: Option<&str>) -> bool {
    value == Some("true")
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn only_the_exact_value_true_opts_in() {
        assert!(parse(Some("true")));
        assert!(!parse(Some("TRUE")));
        assert!(!parse(Some("1")));
        assert!(!parse(Some("")));
        assert!(!parse(None));
    }
}
