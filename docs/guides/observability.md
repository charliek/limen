# Observability & operations

A migration is only as safe as your ability to see it. Limen exposes its
behaviour through a **separate control plane** — health checks and Prometheus
metrics on their own listener — plus structured, correlated logs on the data
plane (spec §10).

## Two planes

Limen binds two listeners so operational traffic never competes with client
traffic:

| Plane | Address | Serves |
|---|---|---|
| Data | `server.listen_addr` | the proxied client requests |
| Control | `metrics.listen_addr` | `/health/live`, `/health/ready`, `metrics.path` |

Keep the control plane on an internal-only interface; it carries health and
metrics, not client data.

## Health endpoints

- **`/health/live`** — the process is up and handlers respond. Use it as the
  liveness probe; a failure means restart.
- **`/health/ready`** — whether Limen should receive traffic. It **degrades
  rather than hard-fails**: a stale flag provider has fallen back to the
  fail-safe mode (legacy), which is still safe to serve, so readiness reports
  `degraded` with a `200`. Only a genuinely unsafe state returns `503`.

```text
$ curl -s localhost:9090/health/ready
degraded
```

Wire `/health/ready` to your load balancer; treat `200` (ready **or** degraded)
as "send traffic", `503` as "drain". Alert on sustained `degraded` — it means a
rollout is pinned to legacy because flags went stale.

## Metrics

The control plane renders Prometheus exposition at `metrics.path` (default
`/metrics`). Point a scraper at it:

```yaml
scrape_configs:
  - job_name: limen
    static_configs:
      - targets: ["limen-host:9090"]
```

The metric set (spec §10.1):

| Metric | Type | Labels |
|---|---|---|
| `limen_requests_total` | counter | `route`, `method`, `upstream`, `status_class` |
| `limen_request_duration_seconds` | histogram | `route`, `upstream` |
| `limen_upstream_errors_total` | counter | `route`, `upstream` |
| `limen_upstream_timeouts_total` | counter | `route`, `upstream` |
| `limen_in_flight_requests` | gauge | — |
| `limen_shadow_requests_total` | counter | `route` |
| `limen_shadow_skipped_total` | counter | `route`, `reason` |
| `limen_shadow_failed_total` | counter | `route`, `reason` |
| `limen_comparisons_total` | counter | `route`, `result` |
| `limen_comparison_skipped_total` | counter | `route`, `reason` |
| `limen_diff_sampled_total` | counter | `route` |
| `limen_circuit_breaker_state` | gauge | `route`, `upstream` (0 closed, 1 half-open, 2 open) |
| `limen_flag_provider_stale` | gauge | — |
| `limen_flag_provider_staleness_seconds` | gauge | — |
| `limen_flag_provider_consecutive_failures` | gauge | — |

!!! warning "Bounded labels only"
    Every label is low-cardinality: a route id, an HTTP method, the upstream
    (`legacy`/`new`), a status *class* (`2xx`…), or a small enum. Limen **never**
    puts a tenant id, user id, request id, or raw path in a label — those would
    explode cardinality and can carry secrets. Per-request identifiers live in
    logs, not metrics.

A few queries to start from:

```promql
# New-vs-legacy traffic split for a route
sum by (upstream) (rate(limen_requests_total{route="get-device"}[5m]))

# New-side error rate
sum(rate(limen_upstream_errors_total{upstream="new"}[5m]))

# p99 latency by upstream
histogram_quantile(0.99, sum by (le, upstream) (rate(limen_request_duration_seconds_bucket[5m])))

# Comparison mismatch ratio (shadow parity)
sum(rate(limen_comparisons_total{result="mismatch"}[5m]))
  / sum(rate(limen_comparisons_total[5m]))
```

## Structured logs & request correlation

Logs go through `tracing`. The default formatter is human-readable; set
`LIMEN_LOG_FORMAT=json` for line-delimited JSON, and `RUST_LOG` to adjust the
level (e.g. `RUST_LOG=limen=debug`). Each proxied request emits a `limen.request`
event with the route, mode, method, selected upstream, status, and latency; a
shadow mismatch emits a `limen.response_mismatch` event with **pre-redacted**
differences (spec §7.3).

Every request is correlated by an `x-request-id`:

- if the client sends one (short, printable), Limen reuses it; otherwise it mints
  a fresh id;
- the id is attached to the request forwarded upstream, echoed on the client
  response, and recorded on the request's log span;
- standard upstream trace headers (`traceparent`, `b3`, …) are forwarded
  unchanged, so existing distributed traces are preserved.

## Durable mismatch diffs

Metrics tell you *that* a route is diverging; the `limen.response_mismatch` log
tells you *how* — but only until your log buffer rolls. For triage that outlives
the logs, turn on the **diff sink** (spec §10.4):

```yaml
diff_sink:
  dir: "./limen-diffs"
```

Every comparison mismatch is then appended as one JSON object to
`<dir>/mismatches-<UTC date>.jsonl`, *in addition to* the metrics and the log
line — the sink is fanned out alongside them, never in place of them. Matches
write nothing; a run with no mismatches never even creates the directory.

```json
{"timestamp":"2026-07-28T10:00:05Z","route_id":"get-device","request_id":"0f2c…",
 "method":"GET","path":"/devices/42","legacy_status":200,"new_status":200,
 "status_match":true,"body_match":false,"mismatch_kinds":["body"],
 "differences":[{"path":"$.device.name","kind":"changed","legacy":"A","new":"B"}],
 "header_mismatches":[],"cookie_mismatches":[],"location_mismatches":[],
 "diff_truncated":false}
```

!!! warning "Same redaction, one more surface"
    Sink records carry exactly what the mismatch log carries: values the
    comparison engine already redacted (`redact_paths`, sensitive headers,
    sensitive query params, and cookie values — which are *never* rendered, only
    names and attributes). It is still a file on disk holding response fragments,
    so treat the directory like a log directory: same permissions, same
    retention, same review.

Rotation is by date only — **retention is yours**. Point your existing
log-retention tooling (`logrotate`, a cron `find -mtime +N -delete`, a lifecycle
policy on the mounted volume) at the directory. The sink never blocks the
client: the shadow task only serializes a record and hands it to a bounded,
non-blocking channel; a single dedicated writer thread owns the file handle and
does the actual (synchronous) IO off any Tokio worker. A stalled volume backs
up that channel instead of the proxy — once it's full, further mismatches are
dropped and counted rather than queued unboundedly, and an IO failure logs one
`limen.diff_sink_write_failed` warning (then counts, not re-logs, until a write
succeeds again) rather than interfering with traffic.

Read the files back with `limen report` — no config file needed, so it runs
wherever the files ended up:

```bash
# What is diverging, and how, since this morning?
limen report --dir ./limen-diffs --since 2026-07-28T00:00:00Z

# One route, as JSON, for a dashboard or a cross-tool join
limen report --dir ./limen-diffs --route get-device --format json
```

```text
3 mismatch(es) across 2 route(s) (2 file(s) read)

ROUTE         COUNT  KINDS
get-device        2  body 2, set_cookie.value 1
list-devices      1  status 1

get-device — 2 most recent:
  2026-07-28T10:00:05Z  GET  /devices/42  0f2c…  body,set_cookie.value
  2026-07-28T10:00:00Z  GET  /devices/7   9ab1…  body
```

The per-kind counts use the same neutral vocabulary Pharos reports
(`status`, `body`, `header`, `set_cookie.*`, `location.*`), so a route's Limen
diffs and its Pharos verdicts can be read side by side. Full flag reference:
[CLI → `report`](../reference/cli.md#report).

## Graceful shutdown

On `SIGINT`/`SIGTERM` Limen drains rather than dropping connections:

1. stops accepting new connections on both planes;
2. lets in-flight requests finish, up to `server.graceful_shutdown_timeout_ms`;
3. stops starting new shadows (in-flight shadows are abandoned, never the
   client's request);
4. exits cleanly — or, if the drain deadline passes, forces exit and logs it.

Set `graceful_shutdown_timeout_ms` comfortably above your longest expected
request, and give orchestrators (e.g. Kubernetes `terminationGracePeriodSeconds`)
a little more than that so the drain isn't cut short.
