# Deployment

Limen is deployment-agnostic by design (spec §11): the same binary runs as a
co-located sidecar or as a standalone edge proxy, and builds as a single static
binary or a small container image.

## Local trial (Docker Compose)

The fastest way to see Limen work is the bundled Compose demo — the proxy in
front of two mock upstreams:

```bash
docker compose -f examples/docker-compose.yaml up --build
```

```bash
curl localhost:8080/                                      # served by legacy
curl -s localhost:9090/metrics | grep limen_comparisons   # shadow comparison counts
curl localhost:9090/health/ready                          # -> ready
```

The two mocks return different bodies, so the shadow comparison records a
mismatch (visible in `limen_comparisons_total{result="mismatch"}` and the logs)
while the client keeps getting legacy's response. Edit
`examples/compose-config.yaml` to experiment.

## Container image

The repo ships a multi-stage `Dockerfile` that produces a Debian-slim runtime
image with just the `limen` binary:

```bash
docker build -t limen .
docker run --rm -v "$PWD/deploy:/etc/limen:ro" -p 8080:8080 -p 9090:9090 \
  limen run -c /etc/limen/limen.yaml
```

Mount a directory containing your config **and every file it references** —
contracts (resolved relative to the config file) and any `flags.file.path` — and
publish both the data-plane and control-plane ports. Point the upstreams at
addresses the container can reach. For a self-contained, ready-to-run example,
use the Compose demo above rather than the placeholder example config.

## Deployment models

### Sidecar / co-located

Limen and one or both upstreams run on the same host — legacy and new processes
side by side, with upstreams reachable as `http://localhost:PORT`. Common in
staging and incremental production rollouts. Point `legacy_upstream` /
`new_upstream` at the local ports.

### Standalone edge proxy

Limen runs toward the edge and routes to legacy and new in **separate clusters**
behind internal DNS or load balancers, typically over **TLS**. This is the
higher-scale deployment; the streaming path is the one to keep fast (it does no
buffering for un-sampled traffic).

## TLS to upstreams

The MVP terminates TLS toward upstreams (HTTPS legacy/new) with certificate
verification **on by default**:

```yaml
upstream_tls:
  verify_certificates: true
  ca_bundle_path: "/etc/limen/internal-ca.pem" # optional, for internal PKI
```

Point `ca_bundle_path` at a custom CA bundle to trust an internal PKI. Client-
side TLS termination at Limen (serving HTTPS to clients) is a documented
post-MVP expansion, not part of the MVP.

## Forwarded headers

Both the primary and shadow upstream requests carry `X-Forwarded-For` (the
client address appended to any existing value) and `X-Forwarded-Proto`. The
shadow request additionally carries `X-Limen-Shadow: 1`, letting an upstream's
access logs (or the upstream itself) tell shadow traffic apart from real
client traffic (spec §3.6).

If you run Limen behind a TLS-terminating load balancer (the "standalone edge
proxy" model above), that LB should set `X-Forwarded-Proto: https` on the
request it forwards to Limen — Limen preserves an existing value rather than
overwriting it, and only sets `http` (its own listener's scheme; the MVP has
no listener TLS) when the header is absent. `X-Forwarded-Host` is intentionally
never set — point upstreams at their own base URL rather than relying on it.

## Operational limitations

Two properties surprise operators at deploy time rather than at config time, so
check both against the traffic you intend to put behind Limen.

**Protocol upgrades are stripped, not proxied.** `Connection` and `Upgrade` are
hop-by-hop headers (RFC 7230 §6.1), and Limen drops them — along with every
header a `Connection` token names — on both the request and response legs. There
is no `101 Switching Protocols` path in the proxy at all, so a WebSocket
handshake sent through Limen does not become a tunnel: the upstream sees an
ordinary HTTP request with the upgrade headers removed, and answers it as one.
This is not a gap to work around with configuration — protocols beyond HTTP/1.1
and HTTP/2 over TCP are an explicit spec non-goal (§1.2). Route WebSocket
traffic around Limen.

**`graceful_shutdown_timeout_ms` is a hard cap on open streams, not just on slow
requests.** On `SIGTERM`/`SIGINT` Limen stops accepting, waits for in-flight
requests up to that window (default **10 s**), and then forces exit, logging
that it did. A long-lived response — SSE, long-poll, a large download — gets no
special treatment: it is an in-flight request that will still be open when the
window closes, and it is dropped mid-stream. If streams like that must survive a
restart, raise `server.graceful_shutdown_timeout_ms` past the longest one you
intend to outlive (and the orchestrator's termination grace period past that).
Otherwise, expect stream drops on every deploy and make sure clients reconnect.

## Operating it

- Probe `/health/live` for liveness and `/health/ready` for readiness — the
  latter degrades (still `200`) when flags are stale and the proxy is serving
  legacy as a fallback. See [Observability & Operations](observability.md).
- Scrape `metrics.path` on the control-plane port for the full metric set.
- Give the orchestrator's termination grace period a little more than
  `server.graceful_shutdown_timeout_ms` so in-flight requests drain cleanly.
