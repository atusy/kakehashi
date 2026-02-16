//! Auto-install functionality for kakehashi.
//!
//! This module handles automatic installation of missing language parsers and queries
//! when a file is opened that requires them.
//!
//! # Module Structure
//!
//! - `InstallingLanguages`: Type alias for `InProgressSet<String>` tracking concurrent installs
//! - `InstallingLanguagesExt`: Extension trait providing domain-specific method names
//! - `AutoInstallManager`: Isolated coordinator for installation

mod manager;

pub(crate) use manager::{AutoInstallManager, InstallEvent};

use crate::lsp::in_progress_set::InProgressSet;

/// Tracks languages currently being installed to prevent duplicate installs.
///
/// This is a type alias for `InProgressSet<String>`, providing domain-specific
/// semantics while reusing the generic concurrent set implementation.
pub type InstallingLanguages = InProgressSet<String>;

/// Extension trait providing domain-specific method names for `InstallingLanguages`.
pub trait InstallingLanguagesExt {
    /// Try to start installing a language. Returns true if this call started the install,
    /// false if it was already being installed.
    fn try_start_install(&self, language: &str) -> bool;

    /// Mark a language installation as complete.
    fn finish_install(&self, language: &str);
}

impl InstallingLanguagesExt for InstallingLanguages {
    fn try_start_install(&self, language: &str) -> bool {
        self.try_start(&language.to_string())
    }

    fn finish_install(&self, language: &str) {
        self.finish(&language.to_string());
    }
}
