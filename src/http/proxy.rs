//! The streaming proxy core: match a route, choose the primary upstream, relay
//! the request, and stream the response back.
//!
//! This is the default zero-copy path (spec §3.3): neither the request nor the
//! response body is buffered. Limen observes only method, path, status, and
//! headers. Shadowing and comparison (Phase 4) layer on top without touching
//! this client-facing relay.

use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tracing::warn;
use url::Url;

use super::server::AppState;
use crate::routing::{decision, Upstream};

/// Hop-by-hop headers (RFC 7230 §6.1) that must not be forwarded across a proxy.
/// Compared lowercased, which is how `HeaderName::as_str` renders them.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The data-plane fallback handler: every client request flows through here.
pub async fn handle(State(state): State<AppState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let method = parts.method;
    let uri = parts.uri;
    let path = uri.path();

    // Match a route by method + longest path prefix.
    let Some(route) = state.routes().match_route(method.as_str(), path) else {
        return not_found();
    };

    let upstream = decision::primary_upstream(route.mode);
    let base = match upstream {
        Upstream::Legacy => route.legacy_upstream.as_ref(),
        Upstream::New => route.new_upstream.as_ref(),
    };
    let Some(base) = base else {
        warn!(
            route = %route.id,
            upstream = upstream.as_str(),
            "selected upstream has no configured URL",
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "limen: upstream not configured\n",
        )
            .into_response();
    };

    // Build the upstream URL, refusing to forward a path we cannot represent
    // byte-for-byte (dot-segments would be silently rewritten — see
    // `build_upstream_url`). Copy out what we need so the `state` borrow ends
    // before the await.
    let Some(url) = build_upstream_url(base, path, uri.query()) else {
        warn!(route = %route.id, path, "refusing to forward a path that requires normalization");
        return (
            StatusCode::BAD_REQUEST,
            "limen: request path cannot be forwarded unchanged\n",
        )
            .into_response();
    };
    let route_id = route.id.clone();
    let timeout = Duration::from_millis(route.timeouts.primary_ms);

    let headers = filter_headers(&parts.headers, Direction::Request);
    let upstream_body = reqwest::Body::wrap_stream(body.into_data_stream());

    // The timeout bounds time-to-response (connect + send + first byte), not the
    // body transfer — `send()` resolves once headers arrive, then the body
    // streams without a total deadline, preserving the unbounded streaming path.
    let send = state
        .client()
        .inner()
        .request(method, url)
        .headers(headers)
        .body(upstream_body)
        .send();

    match tokio::time::timeout(timeout, send).await {
        Ok(Ok(resp)) => relay_response(resp),
        Ok(Err(error)) => {
            // Phase 6 adds failover; for now an upstream failure is a 502.
            warn!(route = %route_id, upstream = upstream.as_str(), %error, "upstream request failed");
            bad_gateway()
        }
        Err(_elapsed) => {
            warn!(
                route = %route_id,
                upstream = upstream.as_str(),
                timeout_ms = timeout.as_millis(),
                "upstream did not respond before the primary timeout",
            );
            gateway_timeout()
        }
    }
}

/// Build the upstream URL from the upstream origin + the request's path/query.
///
/// The upstream is expected to be an origin (`scheme://host[:port]`). Returns
/// `None` if setting the request path would change it (dot-segment collapse such
/// as `/a/../b`, or a path the URL parser re-encodes) — Limen refuses to forward
/// a rewritten path rather than risk sending the upstream a different resource
/// than the client asked for.
fn build_upstream_url(base: &Url, path: &str, query: Option<&str>) -> Option<Url> {
    let mut url = base.clone();
    url.set_path(path);
    if url.path() != path {
        return None;
    }
    url.set_query(query);
    Some(url)
}

/// Whether headers are being forwarded on the request or response leg.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Request,
    Response,
}

/// Copy headers, dropping:
/// - hop-by-hop headers ([`HOP_BY_HOP`]) and any header named in a `Connection`
///   header's token list (RFC 7230 §6.1);
/// - `transfer-encoding` (a hop-by-hop header) in both directions, since the
///   relay re-frames the body;
/// - on the **request** leg, `host` and `content-length` — the upstream client
///   sets Host and frames the streamed request body itself.
///
/// Response `content-length` is preserved: the body is relayed unchanged, so the
/// length still matches (and `HEAD`/`304` keep their meaningful length).
fn filter_headers(src: &HeaderMap, direction: Direction) -> HeaderMap {
    let connection_named = connection_tokens(src);
    let mut out = HeaderMap::with_capacity(src.len());
    for (name, value) in src {
        let n = name.as_str();
        let drop = HOP_BY_HOP.contains(&n)
            || connection_named.iter().any(|t| t == n)
            || (direction == Direction::Request && (n == "host" || n == "content-length"));
        if drop {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Lowercased header names listed in any `Connection` header's comma-separated
/// token list — these are connection-specific and must not be forwarded.
fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Turn the upstream response into a streamed client response.
fn relay_response(resp: reqwest::Response) -> Response {
    let status = resp.status();
    let headers = filter_headers(resp.headers(), Direction::Response);
    let body = Body::from_stream(resp.bytes_stream());
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "limen: no route matched\n").into_response()
}

fn bad_gateway() -> Response {
    (StatusCode::BAD_GATEWAY, "limen: upstream request failed\n").into_response()
}

fn gateway_timeout() -> Response {
    (StatusCode::GATEWAY_TIMEOUT, "limen: upstream timed out\n").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn upstream_url_combines_origin_path_and_query() {
        let base = Url::parse("https://legacy.internal").unwrap();
        let url = build_upstream_url(&base, "/devices/123", Some("verbose=1")).unwrap();
        assert_eq!(
            url.as_str(),
            "https://legacy.internal/devices/123?verbose=1"
        );
    }

    #[test]
    fn upstream_url_without_query() {
        let base = Url::parse("http://localhost:3001").unwrap();
        let url = build_upstream_url(&base, "/health", None).unwrap();
        assert_eq!(url.as_str(), "http://localhost:3001/health");
    }

    #[test]
    fn upstream_url_preserves_percent_encoding() {
        let base = Url::parse("http://h").unwrap();
        let url = build_upstream_url(&base, "/a%20b", Some("q=%2F")).unwrap();
        assert_eq!(url.as_str(), "http://h/a%20b?q=%2F");
    }

    #[test]
    fn upstream_url_refuses_dot_segment_paths() {
        let base = Url::parse("http://h").unwrap();
        // Both literal and percent-encoded dot segments would be rewritten.
        assert!(build_upstream_url(&base, "/a/../admin", None).is_none());
        assert!(build_upstream_url(&base, "/devices/%2e%2e/admin", None).is_none());
    }

    #[test]
    fn filter_drops_hop_by_hop_and_request_only_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("client.example"));
        headers.insert(
            "connection",
            HeaderValue::from_static("keep-alive, x-secret"),
        );
        headers.insert("x-secret", HeaderValue::from_static("leak"));
        headers.insert("content-length", HeaderValue::from_static("5"));
        headers.insert("x-tenant-id", HeaderValue::from_static("t-1"));

        let out = filter_headers(&headers, Direction::Request);
        assert!(out.get("host").is_none());
        assert!(out.get("connection").is_none());
        assert!(out.get("content-length").is_none());
        assert!(out.get("x-secret").is_none()); // named by Connection
        assert_eq!(out.get("x-tenant-id").unwrap(), "t-1");
    }

    #[test]
    fn filter_preserves_response_content_length_and_host() {
        let mut headers = HeaderMap::new();
        headers.insert("content-length", HeaderValue::from_static("12"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        let out = filter_headers(&headers, Direction::Response);
        assert_eq!(out.get("content-length").unwrap(), "12");
        assert_eq!(out.get("content-type").unwrap(), "application/json");
        assert!(out.get("transfer-encoding").is_none()); // hop-by-hop, re-framed
    }
}
