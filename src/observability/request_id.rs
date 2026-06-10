//! Request/trace id resolution and propagation (spec §10.2).
//!
//! Limen correlates a request across its logs (and the upstreams it forwards to)
//! by an `x-request-id`. If the client already sent one we reuse it; otherwise
//! we mint a fresh 128-bit id. The resolved id is attached to the forwarded
//! request, echoed on the client response, and recorded on the request's log
//! span. Standard upstream trace headers (`traceparent`, `b3`, …) are forwarded
//! unchanged by the proxy's header copy, so existing traces are preserved.

use axum::http::HeaderMap;

/// The header carrying the request/trace id.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The incoming `x-request-id` if present and reasonable, else a fresh id.
///
/// A client-supplied id is only reused when it is short and printable ASCII, so
/// a malicious client cannot smuggle control characters or an unbounded value
/// into logs and the echoed response header.
pub fn resolve(headers: &HeaderMap) -> String {
    headers
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|s| is_reasonable(s))
        .map(str::to_string)
        .unwrap_or_else(generate)
}

/// Whether a client-supplied id is safe to reuse: non-empty, bounded, printable.
fn is_reasonable(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_graphic())
}

/// Mint a fresh 128-bit hex id.
fn generate() -> String {
    let hi: u64 = rand::random();
    let lo: u64 = rand::random();
    format!("{hi:016x}{lo:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn reuses_a_reasonable_client_id() {
        let mut h = HeaderMap::new();
        h.insert(REQUEST_ID_HEADER, HeaderValue::from_static("abc-123"));
        assert_eq!(resolve(&h), "abc-123");
    }

    #[test]
    fn generates_when_absent() {
        let id = resolve(&HeaderMap::new());
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn rejects_unreasonable_client_ids() {
        let mut h = HeaderMap::new();
        // Control characters can't even be constructed as a HeaderValue here, but
        // an over-long id must be rejected and replaced with a generated one.
        let long = "x".repeat(200);
        h.insert(REQUEST_ID_HEADER, HeaderValue::from_str(&long).unwrap());
        let id = resolve(&h);
        assert_eq!(id.len(), 32, "over-long id replaced with a generated one");
    }
}
