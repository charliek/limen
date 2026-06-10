//! Bounded body buffering and body-limit enforcement (spec §3.3, §9.4).
//!
//! The streaming path never buffers, so unbounded bodies are fine there. The
//! buffer-for-compare path (Phase 4) and any request-body buffering must be
//! *bounded*: [`to_bytes_limited`] reads a body into memory but fails fast with
//! [`BodyError::TooLarge`] the moment it would exceed the configured cap, so the
//! caller can fall back to streaming with comparison skipped.

use axum::body::Body;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use thiserror::Error;

/// Why buffering a body failed.
#[derive(Debug, Error)]
pub enum BodyError {
    /// The body exceeded the configured byte limit.
    #[error("body exceeded the {limit}-byte limit")]
    TooLarge {
        /// The limit that was exceeded.
        limit: usize,
    },
    /// The underlying body stream errored.
    #[error("error reading body: {0}")]
    Read(String),
}

/// Read a body fully into memory, failing with [`BodyError::TooLarge`] as soon
/// as it would exceed `limit` bytes (so an over-limit body is never fully
/// buffered).
pub async fn to_bytes_limited(body: Body, limit: usize) -> Result<Bytes, BodyError> {
    let mut stream = body.into_data_stream();
    let mut buf = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| BodyError::Read(e.to_string()))?;
        if buf.len() + chunk.len() > limit {
            return Err(BodyError::TooLarge { limit });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_body_within_limit() {
        let body = Body::from("hello");
        let bytes = to_bytes_limited(body, 1024).await.unwrap();
        assert_eq!(&bytes[..], b"hello");
    }

    #[tokio::test]
    async fn rejects_body_over_limit() {
        let body = Body::from("0123456789");
        let err = to_bytes_limited(body, 4).await.unwrap_err();
        assert!(matches!(err, BodyError::TooLarge { limit: 4 }));
    }

    #[tokio::test]
    async fn empty_body_is_ok() {
        let bytes = to_bytes_limited(Body::empty(), 0).await.unwrap();
        assert!(bytes.is_empty());
    }
}
