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
//!
//! Size is not the only way a body fails to arrive: one that trickles (or stops
//! entirely) is small forever and would hold the buffering open just as long.
//! [`buffer_or_stream_within`] therefore bounds the buffering *in time* as well,
//! demoting to the same prefix-plus-stream fallback at a caller-supplied
//! deadline. Only the sampled primary response leg passes one — it is the only
//! buffering that sits on the client's response path.

use axum::body::Body;
use axum::BoxError;
use bytes::{Bytes, BytesMut};
use futures::{stream, Stream, StreamExt};
use tokio::time::Instant;

/// The outcome of buffering a response body for comparison.
pub enum Buffered {
    /// The body fit within the limit and is fully buffered.
    Full(Bytes),
    /// The body exceeded the limit; comparison must be skipped. The carried
    /// [`Body`] streams the already-read prefix followed by the remaining
    /// upstream stream, so the client still receives the full, unbuffered body.
    TooLarge(Body),
    /// The deadline elapsed before the body completed; comparison must be
    /// skipped. Carries the same prefix-plus-remainder [`Body`] as
    /// [`Buffered::TooLarge`] — a slow body is served in full, just uncompared.
    /// Only [`buffer_or_stream_within`] can produce this.
    TimedOut(Body),
    /// The upstream body stream errored before completing.
    Error,
}

/// Buffer a reqwest response body up to `limit` bytes for comparison, falling
/// back to streaming (prefix + remainder) the moment it would exceed the limit —
/// so an over-limit body is never fully buffered, yet the client is still served
/// the complete body. A body of exactly `limit` bytes is buffered; `limit + 1`
/// streams.
pub async fn buffer_or_stream(resp: reqwest::Response, limit: usize) -> Buffered {
    buffer_bounded(resp.bytes_stream(), limit, None).await
}

/// As [`buffer_or_stream`], but also bounded in time: at `deadline` the
/// buffering is abandoned and the body is handed back as
/// [`Buffered::TimedOut`] (prefix + remainder), so a body that trickles — or
/// stalls outright — costs the client at most the caller's budget rather than
/// however long the upstream cares to take.
pub async fn buffer_or_stream_within(
    resp: reqwest::Response,
    limit: usize,
    deadline: Instant,
) -> Buffered {
    buffer_bounded(resp.bytes_stream(), limit, Some(deadline)).await
}

/// Buffer a client *request* body up to `limit` bytes so the identical bytes can
/// be sent to the primary and replayed to the shadow (spec §6.1). Same bound and
/// same fallback as [`buffer_or_stream`]: over the limit the body is never fully
/// held in memory, and the returned [`Buffered::TooLarge`] streams prefix +
/// remainder to the primary unchanged while the caller skips shadowing.
pub async fn buffer_request_or_stream(body: Body, limit: usize) -> Buffered {
    buffer_bounded(body.into_data_stream(), limit, None).await
}

/// One turn of the bounded read: the stream produced something (or ended), or
/// the deadline won the race. Carried out of the `select!` below rather than
/// returned from inside it, because the losing branch's borrow of the stream is
/// still live in there and the demotion has to *move* the stream.
enum Step<T> {
    Yielded(Option<T>),
    Expired,
}

/// The shared bounded read behind every entry point: buffer while the running
/// total stays within `limit` and (when given) the clock stays within
/// `deadline`, otherwise hand back the untouched byte sequence as a stream
/// (already-read prefix chained with the rest).
///
/// The deadline is raced against the `next()` await itself rather than checked
/// between chunks, because the case that matters most is the stream that never
/// yields again: per-chunk arithmetic would never run to notice. It also has to
/// live *inside* this function — an outer `tokio::time::timeout` around the
/// whole call would drop the future that owns both the prefix and the stream,
/// leaving nothing to hand the client.
async fn buffer_bounded<S, E>(stream: S, limit: usize, deadline: Option<Instant>) -> Buffered
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Into<BoxError> + 'static,
{
    // Boxed so the (possibly `!Unpin`) source stream can be polled here and
    // still be moved into the passthrough fallbacks below.
    let mut stream = Box::pin(stream);
    let mut chunks: Vec<Bytes> = Vec::new();
    let mut total = 0usize;

    // One timer for the whole read, pinned before the loop so every iteration
    // polls the *same* deadline rather than restarting a per-chunk budget.
    let expiry = deadline.map(tokio::time::sleep_until);
    tokio::pin!(expiry);

    loop {
        let step = match expiry.as_mut().as_pin_mut() {
            // One timer, polled *first* on every turn — both halves matter.
            // `timeout_at` gets neither: it builds a fresh `Sleep` per chunk
            // and polls the inner future ahead of it. A fresh `Sleep` is never
            // ready on its first poll (it has to register with the timer driver
            // first), so an upstream whose chunks are always immediately ready
            // means the inner future never yields, the timer is never reached,
            // and the read runs on past the deadline with the size cap as its
            // only remaining bound. Holding one registered timer and giving it
            // the `biased` first look means that once it fires, the very next
            // turn demotes — no matter how eagerly the stream is producing.
            Some(expiry) => tokio::select! {
                biased;
                () = expiry => Step::Expired,
                next = stream.next() => Step::Yielded(next),
            },
            None => Step::Yielded(stream.next().await),
        };
        // Out of budget: the client gets the prefix and the rest of the live
        // stream, exactly as it would for an over-limit body.
        let Step::Yielded(next) = step else {
            return Buffered::TimedOut(prefix_then_rest(chunks, stream));
        };
        match next {
            Some(Ok(chunk)) => {
                total += chunk.len();
                chunks.push(chunk);
                if total > limit {
                    // Over the limit: hand the client the buffered prefix
                    // chained with the rest of the still-open stream.
                    return Buffered::TooLarge(prefix_then_rest(chunks, stream));
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

/// The already-read prefix chained with the rest of the still-open stream — the
/// one body shape every demotion hands back, so the client is served the
/// complete byte sequence whichever bound was hit.
fn prefix_then_rest<S, E>(chunks: Vec<Bytes>, rest: S) -> Body
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Into<BoxError> + 'static,
{
    let prefix = stream::iter(chunks.into_iter().map(Ok::<Bytes, E>));
    Body::from_stream(prefix.chain(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    type Chunk = Result<Bytes, std::io::Error>;

    fn one(data: &'static str) -> Chunk {
        Ok(Bytes::from_static(data.as_bytes()))
    }

    #[tokio::test]
    async fn a_stream_that_never_yields_again_still_trips_the_deadline() {
        // The other half: no chunk ever arrives, so anything that checked the
        // budget per chunk would never run at all.
        let stalled = stream::pending::<Chunk>();
        let deadline = Instant::now() + std::time::Duration::from_millis(20);
        let out = buffer_bounded(stalled, 1024, Some(deadline)).await;
        assert!(matches!(out, Buffered::TimedOut(_)));
    }

    #[tokio::test]
    async fn a_body_that_completes_inside_the_deadline_is_buffered_whole() {
        // The false-demotion control at the unit level: having a deadline must
        // not cost a body that arrives in time.
        let quick = stream::iter(vec![one("ab"), one("cd")]);
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        match buffer_bounded(quick, 1024, Some(deadline)).await {
            Buffered::Full(bytes) => assert_eq!(&bytes[..], b"abcd"),
            _ => panic!("a completing body must buffer whole"),
        }
    }
}
