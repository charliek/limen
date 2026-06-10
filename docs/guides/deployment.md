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

## Operating it

- Probe `/health/live` for liveness and `/health/ready` for readiness — the
  latter degrades (still `200`) when flags are stale and the proxy is serving
  legacy as a fallback. See [Observability & Operations](observability.md).
- Scrape `metrics.path` on the control-plane port for the full metric set.
- Give the orchestrator's termination grace period a little more than
  `server.graceful_shutdown_timeout_ms` so in-flight requests drain cleanly.
