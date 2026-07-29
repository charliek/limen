//! `X-Forwarded-*` header injection on upstream requests (spec §6.3).
//!
//! Limen sets `X-Forwarded-For` and `X-Forwarded-Proto` on **every** upstream
//! request — primary and shadow alike — so both `legacy` and `new` see the
//! same forwarding context an operator would expect from any reverse proxy.
//! `X-Limen-Shadow` is different: it marks the shadow copy only, and is
//! inserted where the shadow request is built ([`crate::http::shadow::plan`]),
//! not here.
//!
//! Neither header is hop-by-hop, so [`super::proxy`]'s `filter_headers` never
//! strips them from the *request* leg. They are only ever written onto
//! *outbound-to-upstream* request headers; `filter_headers` explicitly strips
//! all three of `X-Forwarded-For`, `X-Forwarded-Proto`, and `X-Limen-Shadow`
//! from the *response* leg, so an upstream that reflects them back can't leak
//! them to the client. `filter_headers` also strips a client-sent
//! `X-Limen-Shadow` from the *request* leg unconditionally, so a client can
//! never spoof shadow status on the primary request it sends.

use std::net::IpAddr;

use axum::http::{HeaderMap, HeaderValue};

/// Chain of client addresses this request has passed through (RFC 7239-adjacent
/// de-facto convention). Limen appends its own view of the client's address to
/// any existing value; a value already present — set by a fronting load
/// balancer — is preserved and extended, never replaced (standard proxy
/// semantics).
pub const X_FORWARDED_FOR: &str = "x-forwarded-for";

/// The scheme the client used to reach the front of the proxy chain. Limen
/// sets this **only when absent**: an existing value came from a proxy
/// upstream of Limen (e.g. a TLS-terminating load balancer) and is
/// authoritative — Limen never overwrites it.
pub const X_FORWARDED_PROTO: &str = "x-forwarded-proto";

/// The scheme Limen reports on `X-Forwarded-Proto` when the client didn't
/// already supply one. This is the scheme of Limen's *own* data-plane
/// listener, which is always plain HTTP today — Limen has no listener-TLS
/// config to source anything else from (TLS termination, if any, happens in
/// front of Limen; `upstream_tls` config governs calls *to* upstreams, not
/// this listener). If a listener-scheme config field is ever added, this
/// constant becomes that field's default and `apply` takes the resolved value
/// instead.
const LIMEN_LISTENER_SCHEME: &str = "http";

/// Marks a request as Limen's fire-and-forget shadow copy to the new
/// upstream. Never present on the primary request that serves the client.
/// Inserted in [`crate::http::shadow::plan`] (not by [`apply`]), since only
/// the shadow half of a shadowed request gets it.
pub const X_LIMEN_SHADOW: &str = "x-limen-shadow";

/// Set `X-Forwarded-For` (appending to any existing value) and
/// `X-Forwarded-Proto` (only if absent) on outbound request headers. Called
/// once on the shared request headers, before both the primary send and the
/// shadow plan, so primary and shadow carry identical values (spec §6.3).
///
/// `client_addr` is `None` when the peer address is unavailable. Real serving
/// always has one — the data-plane listener is bound via
/// `Router::into_make_service_with_connect_info` (`src/http/server.rs`), which
/// populates a `ConnectInfo<SocketAddr>` request extension per accepted
/// connection. Integration tests drive the router directly via
/// `tower::ServiceExt::oneshot` with no real accepted connection, so they see
/// `None` unless a test inserts a `ConnectInfo` extension itself. In that case
/// `X-Forwarded-For` is omitted entirely rather than sent with a fabricated
/// value; `X-Forwarded-Proto` is unaffected (it never depends on the client
/// address).
pub fn apply(headers: &mut HeaderMap, client_addr: Option<IpAddr>) {
    if let Some(addr) = client_addr {
        // A client (or an intermediary ahead of Limen) may have sent
        // `X-Forwarded-For` as more than one header line (`HeaderMap` keeps
        // every line under one name); `get` alone would only see the first
        // and `insert` would then discard the rest. Collect every existing
        // line first so no hop in the chain is lost, then replace them all
        // with one combined line (`insert` removes every prior line for the
        // name) — one field-line output is standard XFF practice and keeps
        // the header simple to append to again downstream.
        let mut chain: Vec<String> = headers
            .get_all(X_FORWARDED_FOR)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .collect();
        chain.push(addr.to_string());
        let combined = chain.join(", ");
        if let Ok(value) = HeaderValue::from_str(&combined) {
            headers.insert(X_FORWARDED_FOR, value);
        }
    }
    if !headers.contains_key(X_FORWARDED_PROTO) {
        headers.insert(
            X_FORWARDED_PROTO,
            HeaderValue::from_static(LIMEN_LISTENER_SCHEME),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_both_headers_when_absent_and_addr_known() {
        let mut headers = HeaderMap::new();
        apply(&mut headers, Some("203.0.113.9".parse().unwrap()));
        assert_eq!(headers.get(X_FORWARDED_FOR).unwrap(), "203.0.113.9");
        assert_eq!(headers.get(X_FORWARDED_PROTO).unwrap(), "http");
    }

    #[test]
    fn omits_forwarded_for_when_addr_unknown() {
        let mut headers = HeaderMap::new();
        apply(&mut headers, None);
        assert!(headers.get(X_FORWARDED_FOR).is_none());
        assert_eq!(headers.get(X_FORWARDED_PROTO).unwrap(), "http");
    }

    #[test]
    fn appends_to_an_existing_forwarded_for() {
        let mut headers = HeaderMap::new();
        headers.insert(X_FORWARDED_FOR, HeaderValue::from_static("198.51.100.1"));
        apply(&mut headers, Some("203.0.113.9".parse().unwrap()));
        assert_eq!(
            headers.get(X_FORWARDED_FOR).unwrap(),
            "198.51.100.1, 203.0.113.9"
        );
    }

    #[test]
    fn preserves_an_existing_forwarded_proto() {
        let mut headers = HeaderMap::new();
        headers.insert(X_FORWARDED_PROTO, HeaderValue::from_static("https"));
        apply(&mut headers, Some("203.0.113.9".parse().unwrap()));
        assert_eq!(headers.get(X_FORWARDED_PROTO).unwrap(), "https");
    }

    #[test]
    fn preserves_every_line_of_a_repeated_forwarded_for_header() {
        // A `HeaderMap` can hold the same header name as multiple field
        // lines (distinct from a single comma-joined line); `get` alone only
        // sees the first, so this proves `apply` walks every line via
        // `get_all` before combining, rather than silently dropping hops.
        let mut headers = HeaderMap::new();
        headers.append(X_FORWARDED_FOR, HeaderValue::from_static("198.51.100.1"));
        headers.append(X_FORWARDED_FOR, HeaderValue::from_static("198.51.100.2"));
        apply(&mut headers, Some("203.0.113.9".parse().unwrap()));
        let combined: Vec<&str> = headers
            .get_all(X_FORWARDED_FOR)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(
            combined,
            vec!["198.51.100.1, 198.51.100.2, 203.0.113.9"],
            "no hop may be dropped, and the combined chain is a single line"
        );
    }

    #[test]
    fn renders_an_ipv6_client_address_without_brackets() {
        // XFF carries a bare IP (unlike a `Host`/URI authority, which would
        // need `[…]` around an IPv6 literal); `IpAddr::to_string` already
        // omits brackets, and this pins that rendering against regression.
        let mut headers = HeaderMap::new();
        apply(&mut headers, Some("2001:db8::1".parse().unwrap()));
        assert_eq!(headers.get(X_FORWARDED_FOR).unwrap(), "2001:db8::1");
    }
}
