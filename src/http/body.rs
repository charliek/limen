//! Bounded body buffering for the buffer-for-compare path (spec §3.3, §9.4).
//!
//! The streaming path never buffers, so unbounded bodies are fine there. When a
//! response is buffered for comparison it must be *bounded*: [`buffer_or_stream`]
//! reads up to the configured cap and, the moment a body would exceed it, falls
//! back to streaming (the already-read prefix chained with the remaining
//! upstream stream) so the client still receives the complete body while the
//! comparison is skipped. Exercised end-to-end by the shadow integration tests.

use axum::body::Body;
use bytes::{Bytes, BytesMut};
use futures::{stream, StreamExt};

/// The outcome of buffering a response body for comparison.
pub enum Buffered {
    /// The body fit within the limit and is fully buffered.
    Full(Bytes),
    /// The body exceeded the limit; comparison must be skipped. The carried
    /// [`Body`] streams the already-read prefix followed by the remaining
    /// upstream stream, so the client still receives the full, unbuffered body.
    TooLarge(Body),
    /// The upstream body stream errored before completing.
    Error,
}

/// Buffer a reqwest response body up to `limit` bytes for comparison, falling
/// back to streaming (prefix + remainder) the moment it would exceed the limit —
/// so an over-limit body is never fully buffered, yet the client is still served
/// the complete body. A body of exactly `limit` bytes is buffered; `limit + 1`
/// streams.
pub async fn buffer_or_stream(resp: reqwest::Response, limit: usize) -> Buffered {
    let mut stream = resp.bytes_stream();
    let mut chunks: Vec<Bytes> = Vec::new();
    let mut total = 0usize;

    loop {
        match stream.next().await {
            Some(Ok(chunk)) => {
                total += chunk.len();
                chunks.push(chunk);
                if total > limit {
                    // Over the limit: hand the client the buffered prefix
                    // chained with the rest of the still-open stream.
                    let prefix = stream::iter(chunks.into_iter().map(Ok::<Bytes, reqwest::Error>));
                    return Buffered::TooLarge(Body::from_stream(prefix.chain(stream)));
                }
            }
            Some(Err(_)) => return Buffered::Error,
            None => break,
        }
    }

    let mut buf = BytesMut::with_capacity(total);
    for chunk in chunks {
        buf.extend_from_slice(&chunk);
    }
    Buffered::Full(buf.freeze())
}
