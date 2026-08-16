//! The upstream HTTP client (`reqwest`): TLS, timeouts, connection pooling.
//!
//! A single pooled client serves both the primary and shadow paths. Per-request
//! timeouts are applied at call sites because they are per-route, not
//! per-client. The client never follows redirects — a proxy must relay a 3xx to
//! the client rather than chase it.
//!
//! The client itself is [`stridelabs_http::proxy::UpstreamClient`], which takes
//! a bool and PEM *bytes*. What stays here is the part that is limen's: reading
//! the configured bundle off disk, and carrying the configured path into every
//! error so an operator is told *which* file to go look at. The shared crate
//! deliberately refuses to guess at that — it never sees a path.

use reqwest::Client;
use stridelabs_http::proxy::{ClientBuildError as SharedError, UpstreamClient as SharedClient};
use thiserror::Error;

use crate::config::model::UpstreamTlsConfig;

/// Failure building the upstream client.
#[derive(Debug, Error)]
pub enum ClientBuildError {
    /// reqwest could not construct the client.
    #[error("failed to build upstream HTTP client: {0}")]
    Build(#[from] reqwest::Error),
    /// The configured CA bundle could not be read.
    #[error("failed to read CA bundle {path}: {source}")]
    CaRead {
        /// The CA bundle path.
        path: String,
        /// The I/O error.
        source: std::io::Error,
    },
    /// The configured CA bundle was not valid PEM.
    #[error("invalid CA bundle {path}: {source}")]
    CaParse {
        /// The CA bundle path.
        path: String,
        /// The parse error.
        source: reqwest::Error,
    },
}

/// A pooled HTTP client for upstream calls.
#[derive(Clone)]
pub struct UpstreamClient {
    client: stridelabs_http::proxy::UpstreamClient,
}

impl UpstreamClient {
    /// Build the client from the upstream TLS settings.
    ///
    /// # Boot-path behavior change (adoption of the shared client)
    ///
    /// A malformed CA bundle is now reported *eagerly and attributably*. Limen
    /// previously called `reqwest::Certificate::from_pem`, which under rustls
    /// only stores the bytes — the parse happens inside `build()`, so garbage
    /// PEM surfaced as an unattributed [`ClientBuildError::Build`]. The shared
    /// client uses `from_pem_bundle`, which runs the same parser up front over
    /// the same "every certificate in the file" semantics rustls applies at
    /// build time. **The resulting trust anchors are identical**; what changes
    /// is that a bad bundle now fails as [`ClientBuildError::CaParse`], naming
    /// the file, instead of as a generic build error.
    ///
    /// Both outcomes are a hard boot failure either way, so nothing that ever
    /// served traffic behaves differently — but the boot *message* an operator
    /// reads is not the one it was before, which is why it is written down
    /// here rather than left to be discovered.
    pub fn build(tls: &UpstreamTlsConfig) -> Result<Self, ClientBuildError> {
        // The configured path is exactly what the shared client never sees, and
        // exactly what limen must not lose: every error below names the file an
        // operator has to go fix.
        let bundle_path = tls.ca_bundle_path.as_ref().map(|p| p.display().to_string());

        let pem = tls
            .ca_bundle_path
            .as_ref()
            .map(|path| {
                std::fs::read(path).map_err(|source| ClientBuildError::CaRead {
                    path: path.display().to_string(),
                    source,
                })
            })
            .transpose()?;

        let client = SharedClient::build(tls.verify_certificates, pem.as_deref()).map_err(|e| {
            match e {
                SharedError::CaParse { source } => ClientBuildError::CaParse {
                    // Only reachable when a bundle was configured — that is the
                    // only way PEM bytes reached the shared client at all — so
                    // the fallback is unreachable, not a silent blank.
                    path: bundle_path.unwrap_or_default(),
                    source,
                },
                SharedError::Build(source) => ClientBuildError::Build(source),
            }
        })?;

        Ok(Self { client })
    }

    /// The underlying reqwest client (for issuing requests).
    pub fn inner(&self) -> &Client {
        self.client.inner()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_with_defaults() {
        let client = UpstreamClient::build(&UpstreamTlsConfig::default());
        assert!(client.is_ok());
    }

    #[test]
    fn builds_with_verification_disabled() {
        let tls = UpstreamTlsConfig {
            verify_certificates: false,
            ca_bundle_path: None,
        };
        assert!(UpstreamClient::build(&tls).is_ok());
    }

    #[test]
    fn missing_ca_bundle_errors() {
        let tls = UpstreamTlsConfig {
            verify_certificates: true,
            ca_bundle_path: Some("/nonexistent/ca.pem".into()),
        };
        assert!(matches!(
            UpstreamClient::build(&tls),
            Err(ClientBuildError::CaRead { .. })
        ));
    }
}
