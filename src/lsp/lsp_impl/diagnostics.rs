//! Debounced diagnostic scheduling helpers.

use super::Kakehashi;
use super::coordinator::DiagnosticScheduler;

impl Kakehashi {
    pub(super) fn diagnostic_scheduler(&self) -> DiagnosticScheduler<'_> {
        DiagnosticScheduler::new(self)
    }
}
