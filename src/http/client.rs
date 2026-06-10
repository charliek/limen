//! The upstream HTTP client (`reqwest`): TLS, timeouts, connection pooling.
//!
//! A single pooled client serves both the primary and (later) shadow paths.
//! Per-request timeouts are applied at call sites because they are per-route,
//! not per-client. The client never follows redirects — a proxy must relay a
//! 3xx to the client rather than chase it.

use reqwest::Client;
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
    client: Client,
}

impl UpstreamClient {
    /// Build the client from the upstream TLS settings.
    pub fn build(tls: &UpstreamTlsConfig) -> Result<Self, ClientBuildError> {
        let mut builder = Client::builder()
            // A proxy relays redirects to the client; it must not follow them.
            .redirect(reqwest::redirect::Policy::none())
            .tcp_nodelay(true);

        if !tls.verify_certificates {
            builder = builder.danger_accept_invalid_certs(true);
        }
        if let Some(path) = &tls.ca_bundle_path {
            let pem = std::fs::read(path).map_err(|source| ClientBuildError::CaRead {
                path: path.display().to_string(),
                source,
            })?;
            let cert = reqwest::Certificate::from_pem(&pem).map_err(|source| {
                ClientBuildError::CaParse {
                    path: path.display().to_string(),
                    source,
                }
            })?;
            builder = builder.add_root_certificate(cert);
        }

        Ok(Self {
            client: builder.build()?,
        })
    }

    /// The underlying reqwest client (for issuing requests).
    pub fn inner(&self) -> &Client {
        &self.client
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
