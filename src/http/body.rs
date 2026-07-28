//! Bounded body buffering for the buffer-for-compare path (spec §3.3, §9.4).
//!
//! The streaming path never buffers, so unbounded bodies are fine there. When a
//! body is buffered for comparison it must be *bounded*: [`buffer_or_stream`]
//! reads up to the configured cap and, the moment a body would exceed it, falls
//! back to streaming (the already-read prefix chained with the remaining
//! stream) so the client still receives the complete body while the comparison
//! is skipped. The same bounded read serves the *request* leg
//! ([`buffer_request_or_stream`]) for a route that opted a write method into
//! shadowing: the buffered bytes are replayed to both upstreams, and an
//! over-limit body streams to the primary untouched with the shadow skipped.
//! Exercised end-to-end by the shadow integration tests.

use axum::body::Body;
use axum::BoxError;
use bytes::{Bytes, BytesMut};
use futures::{stream, Stream, StreamExt};

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
    buffer_bounded(resp.bytes_stream(), limit).await
}

/// Buffer a client *request* body up to `limit` bytes so the identical bytes can
/// be sent to the primary and replayed to the shadow (spec §6.1). Same bound and
/// same fallback as [`buffer_or_stream`]: over the limit the body is never fully
/// held in memory, and the returned [`Buffered::TooLarge`] streams prefix +
/// remainder to the primary unchanged while the caller skips shadowing.
pub async fn buffer_request_or_stream(body: Body, limit: usize) -> Buffered {
    buffer_bounded(body.into_data_stream(), limit).await
}

/// The shared bounded read behind both entry points: buffer while the running
/// total stays within `limit`, otherwise hand back the untouched byte sequence
/// as a stream (already-read prefix chained with the rest).
async fn buffer_bounded<S, E>(stream: S, limit: usize) -> Buffered
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Into<BoxError> + 'static,
{
    // Boxed so the (possibly `!Unpin`) source stream can be polled here and
    // still be moved into the over-limit passthrough below.
    let mut stream = Box::pin(stream);
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
                    let prefix = stream::iter(chunks.into_iter().map(Ok::<Bytes, E>));
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
