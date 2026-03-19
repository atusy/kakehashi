//! Parser install and post-install reload orchestration.

use super::Kakehashi;
use super::coordinator::InstallCoordinator;

impl Kakehashi {
    pub(super) fn install_coordinator(&self) -> InstallCoordinator<'_> {
        InstallCoordinator::new(self)
    }
}
