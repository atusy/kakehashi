//! Shared parsing orchestration for host documents.

use super::Kakehashi;
use super::coordinator::ParseCoordinator;

impl Kakehashi {
    pub(super) fn parse_coordinator(&self) -> ParseCoordinator<'_> {
        ParseCoordinator::new(self)
    }
}
