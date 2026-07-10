//! Shared HTTP agent construction for install-path downloads.

use std::time::Duration;
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig};

/// Build an agent with a global timeout and OS trust-store TLS verification.
///
/// `PlatformVerifier` preserves the trust posture of the previous
/// reqwest(rustls) client: installs behind enterprise TLS interception or
/// private root CAs trusted by the OS keep working, where ureq's default
/// WebPKI roots would reject them.
pub(crate) fn agent_with_timeout(timeout: Duration) -> Agent {
    let mut config = Agent::config_builder().timeout_global(Some(timeout));
    // Unit tests use loopback HTTP fixture servers; production install
    // downloads stay HTTPS-only.
    if !cfg!(test) {
        config = config.https_only(true);
    }

    Agent::new_with_config(
        config
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build(),
    )
}
