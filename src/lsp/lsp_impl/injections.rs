//! Injection resolution and bridge warmup helpers.

use super::Kakehashi;
use super::coordinator::InjectionCoordinator;

impl Kakehashi {
    pub(super) fn injection_coordinator(&self) -> InjectionCoordinator<'_> {
        InjectionCoordinator::new(self)
    }
}
