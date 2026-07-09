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
    agent_with_timeout_with_https_policy(timeout, true)
}

#[cfg(test)]
pub(crate) fn agent_with_timeout_allowing_http(timeout: Duration) -> Agent {
    agent_with_timeout_with_https_policy(timeout, false)
}

fn agent_with_timeout_with_https_policy(timeout: Duration, https_only: bool) -> Agent {
    Agent::new_with_config(
        Agent::config_builder()
            .timeout_global(Some(timeout))
            .https_only(https_only)
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_agent_rejects_plain_http() {
        let err = agent_with_timeout(Duration::from_secs(1))
            .get("http://127.0.0.1:1/")
            .call()
            .expect_err("install downloads must be HTTPS-only");

        assert!(
            matches!(err, ureq::Error::RequireHttpsOnly(_)),
            "expected HTTPS-only rejection before transport, got {err:?}"
        );
    }
}
