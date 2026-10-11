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
| `limen_shadow_in_flight` | gauge | — |
| `limen_shadow_requests_total` | counter | `route` |
| `limen_shadow_skipped_total` | counter | `route`, `reason` |
| `limen_shadow_failed_total` | counter | `route`, `reason` |
| `limen_comparisons_total` | counter | `route`, `result` |
| `limen_comparison_skipped_total` | counter | `route`, `reason` |
| `limen_diff_sampled_total` | counter | `route` |
| `limen_diff_sink_enqueued_total` | counter | — |
| `limen_diff_sink_written_total` | counter | — |
| `limen_diff_sink_dropped_total` | counter | `reason` (`queue_full`\|`io_error`\|`writer_gone`) |
| `limen_circuit_breaker_state` | gauge | `route`, `upstream` (0 closed, 1 half-open, 2 open) |
| `limen_flag_provider_stale` | gauge | — |
| `limen_flag_provider_staleness_seconds` | gauge | — |
| `limen_flag_provider_consecutive_failures` | gauge | — |

`limen_shadow_in_flight` counts shadow tasks in flight right now (dispatch
through comparison) — distinct from `limen_in_flight_requests`, which only
gauges client-facing requests. The three `limen_diff_sink_*` series track the
diff sink's async pipeline: `enqueued_total` counts every mismatch record
**offered** to the writer's bounded queue, incremented *before* the
`try_send` call — not just the ones accepted — so the balance
`enqueued == written + dropped` holds even when the queue refuses a record;
counting only accepted records would leave `enqueued` permanently short and
make a finished pipeline look like one still draining. `written_total`
increments after a record is durably appended to its daily file;
`dropped_total` increments per drop reason.

!!! note "These series are zero-registered at startup"
    `limen run` touches `limen_shadow_in_flight` and the three
    `limen_diff_sink_*` counters to zero immediately after installing the
    Prometheus recorder — before any traffic has produced a sample. The three
    families that record uncompared sampled work —
    `limen_shadow_skipped_total`, `limen_comparison_skipped_total` and
    `limen_shadow_failed_total` — are registered the same way, but **per
    configured route and per reason**, because a verdict reads them per route:
    a family that exists somewhere tells you nothing about the route you are
    asking about. A
    lazily-registered metric renders "nothing happened" and "this binary has
    no such instrumentation" identically: an absent series. Zero-registering
    them means their **absence from a scrape is itself meaningful** — it
    signals missing instrumentation (an older binary, or a renderer
    regression), not a quiet run. [`limen verdict`](../reference/cli.md#verdict)
    relies on exactly this: it treats a required series that never appears in
    the scrape as a typed input-unavailable failure (exit 50), never as zero.

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

# New-side transport errors (connection failure, no response). A 5xx response
# is not one of these: it is counted as a status_class="5xx" request.
sum(rate(limen_upstream_errors_total{upstream="new"}[5m]))

# New-side 5xx ratio per route new serves (percentage_split and later). The
# `or 0 * …` arm reports 0 for a route new served without a 5xx (status-class
# series are created lazily); a route new served nothing stays absent. The
# trailing `and … > 0` drops a window in which new served nothing on a route
# that has served before (0/0 would otherwise be NaN).
(
    sum by (route) (rate(limen_requests_total{upstream="new",status_class="5xx"}[5m]))
  or
    0 * sum by (route) (rate(limen_requests_total{upstream="new"}[5m]))
)
  / sum by (route) (rate(limen_requests_total{upstream="new"}[5m]))
and
  sum by (route) (rate(limen_requests_total{upstream="new"}[5m])) > 0

# p99 latency by upstream
histogram_quantile(0.99, sum by (le, upstream) (rate(limen_request_duration_seconds_bucket[5m])))

# Comparison mismatch ratio (shadow parity)
sum(rate(limen_comparisons_total{result="mismatch"}[5m]))
  / sum(rate(limen_comparisons_total[5m]))
```

The `upstream="new"` queries above return nothing for a route in
`shadow_legacy_primary` — see the next section.

### What a shadow route records about `new`

`limen_requests_total` and `limen_request_duration_seconds` describe the
upstream that **served the client**. `limen_upstream_errors_total` and
`limen_upstream_timeouts_total` count failed primary-path **attempts** per
upstream. On a `failover_safe` route, for example, a failed new attempt that
Limen then replays to legacy still increments new's counter. In
`shadow_legacy_primary` legacy is the only upstream the primary path ever
tries. So on a shadow route, all four of these families carry only
`upstream="legacy"`, however much shadow traffic `new` answered.

One `upstream="new"` series can still appear on a shadow route:
`limen_circuit_breaker_state`. It is published for any route with
`circuit_breaker.enabled`, but shadow outcomes never reach the breaker, so on
a shadow route it stays closed. The shadow leg shows up only in the shadow and
comparison families:

| What happened to the shadow request | Where it shows |
|---|---|
| Dispatched | `limen_shadow_requests_total{route}` |
| No response headers within `timeouts.shadow_ms` | `limen_shadow_failed_total{route,reason="timeout"}` |
| Connection failure, or the body errored mid-read. This includes `timeouts.shadow_ms` expiring during the body read, since that deadline bounds the whole exchange | `limen_shadow_failed_total{route,reason="error"}` |
| Body over `max_body_bytes` | `limen_comparison_skipped_total{route,reason="response_too_large"}` (the same series an oversized legacy body increments) |
| Answered and compared | `limen_comparisons_total{route,result}`. New's status appears only on a mismatch, as `new_status` in the [sink record](#durable-mismatch-diffs) and the `limen.response_mismatch` log |

So Limen does not measure new's latency in this mode, and you see new's
status only through the comparison: a `5xx` from new where legacy answered
`2xx` is a status mismatch, not a `status_class="5xx"` sample. Here is how
to judge the [runbook §8.1](../runbook.md#81-the-budget-define-before-shadowing)
latency and error rows during shadowing:

- **Errors.** With `compare_status` on (the default), the verdict covers the
  compared sample. A status difference is a mismatch (exit `10`), and a shadow
  `timeout` or `error` undermines a floored route (exit `20`). So a verdict of
  `0` means new returned legacy's status on every compared request. That is
  per-request equality on a sample, not a rate. On a floored route
  (comparison enabled, `min_comparisons` above `0`), it also means no shadow
  failed in transport. On an unfloored route, shadow failures are
  informational and do not change the exit code. Outside the compared sample,
  Limen cannot give you new's 5xx rate. That covers unsampled
  requests, an unfloored route, and a route with `compare_status: false`,
  where a new `500` whose body matches legacy's counts as a match and writes
  no sink record. The sink holds only mismatches, so its `new_status` values
  come from a selected population. `limen_shadow_failed_total` still counts
  new's transport failures. For the rate itself, probe new directly or read
  new's own metrics.
- **Latency.** Measure it outside Limen, before the first non-zero
  `percentage_split` rung. Drive the same corpus directly at both upstreams,
  from the same place, against the same data, timed at the same boundary, and
  compare p95/p99 per route. Until then the row is unmeasured, not green. From
  the first non-zero rung, `limen_request_duration_seconds{route,upstream="new"}`
  appears beside the `upstream="legacy"` series for rechecks. On a
  `failover_safe` route those two histograms measure different boundaries. A
  successful new response within `server.request_body_limit_bytes` is recorded
  only after it is fully buffered. One over that limit is recorded when it
  starts streaming, so new's boundary varies with response size.
  Legacy-selected responses are recorded once their headers arrive. The
  only latency fact the shadow leg gives you is a ceiling: every compared
  shadow finished within `timeouts.shadow_ms`. A shadow that overran it is
  recorded as `reason="timeout"` if its headers were late, but as
  `reason="error"` if its body was. Zero `timeout` alone therefore does not
  mean nothing overran.

Under `percentage_split` both sides have these series, but they are created
**lazily**. The `upstream="new"` series appears with the first request new
serves, and `limen_upstream_errors_total{upstream="new"}` with its first
transport failure. An absent series there means "nothing yet", not a broken
scrape. One exception applies on a `failover_safe` route. When new answers
`5xx` with enough of the `primary_ms` budget left to replay, Limen replays the
request to legacy, and the request is recorded under `upstream="legacy"` with
legacy's status. That `5xx` lands in no `upstream="new"` series at all ([the
cost of `failover_safe`](resilience.md#the-cost-of-turning-failover_safe-on)).
A `5xx` that arrives with less than 10 ms of the budget left is not replayed.
The client gets Limen's own `502`, recorded as an `upstream="new"` `5xx`.

## Observe mode

[`observe:`](../reference/config-reference.md#observe) adds one more
control-plane surface, present only while the block is configured:
`GET /observe/profile` (registered only when `observe:` is present — with the
block absent the path does not exist at all, the same way `/debug/canary`
exists only under `debug.sink_canary`) serves the whole per-route traffic
profile as one JSON document. It is a passive record of response metadata —
status, a handful of headers, method, and query-parameter *names* — never a
request or response body, and it obeys the same bounded-label doctrine as
every metric above: no raw path, no header value, no query value, nothing a
client controls the length of without a cap. See the [observe mode
guide](observe-mode.md) for what it can and cannot tell you and how [`limen
suggest-routes`](../reference/cli.md#suggest-routes) consumes it.

The one new metric is `limen_observe_observations_total{route}`, a counter
incremented once per recorded observation. It is **zero-registered per
configured route** at startup — the same "these four series are
zero-registered" reasoning noted above, applied to a per-route series this
time — so a route reading absent from a scrape means observe mode is off (or
the binary predates it), never that the route saw zero traffic. Consult
[classifying routes](classifying-routes.md) for how the profile's fields
become a suggested disposition.

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

Each `differences` entry is one location in the *normalized* bodies (so an
`ignore_paths` field never appears), written `$`-rooted with `.key` and
`[index]` steps (`$.items[2].nickname`), and its `kind` is one of three:

| `kind` | Means | Carries |
|---|---|---|
| `changed` | Present on both sides with different values. | `legacy` and `new` |
| `added` | Present only in `new`'s body. | `new` only |
| `removed` | Present only in `legacy`'s body. | `legacy` only |

A key that is `null` on one side and absent on the other is `added`/`removed`,
not `changed` — the null-versus-absent difference, reported as such. Arrays
are walked by index, so an element inserted mid-array reads as a run of
`changed` entries plus one `added` at the tail. A non-JSON body mismatch
produces a single `changed` entry at `$` whose values are byte counts
(`"<1234 bytes, non-JSON>"`), never the bytes. Values are bounded — a string
past 512 bytes is truncated with `…`, a larger object or array becomes
`"<N bytes elided>"` — and `redact_paths` values render as `"<redacted>"`;
past 100 entries the list stops and `diff_truncated` is `true`. Status,
headers, cookies, and `Location` never appear here: they are
`legacy_status`/`new_status` and the three `*_mismatches` lists, so a
status-only mismatch has an empty `differences`.

**`path` is the request path without its query string.** The same holds in
the `limen.response_mismatch` log line and in `limen report`'s examples: both
take the path from the same field, and neither surface carries the query. So
`?page=1` and `?page=2` on a paginated route produce records with identical
`path` values. That field never carries a query value. Query parameters are
in redaction scope (spec §7.5) because they can carry credentials such as
`access_token`.

That guarantee covers the `path` field only, not every surface. A `Location`
query mismatch renders the differing values of any parameter not on the
built-in sensitive list. A response body that echoes a request parameter
carries it into `differences`. And the `upstream request failed` WARN for a
transport failure includes the HTTP client's error text, which contains the
full upstream URL.

To find the request behind a record, join on `request_id`. It is the request's
[`x-request-id`](#structured-logs-request-correlation): Limen reuses the
client's own value when it is 1–128 bytes of printable ASCII with no spaces.
Otherwise, including when the value is empty, Limen mints one. Either way, the id is echoed on the
client response and forwarded to both upstreams. So have your driver send a
unique `x-request-id` per request, or log the echoed response header, next to
the full URL it requested. That log is what maps a record back to its page,
filter, or other query.

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

!!! note "Accepted edge case: a panicked writer thread"
    The writer thread is written to never panic — IO failures are caught and
    counted, not `unwrap`ed. If it somehow did panic anyway, its
    already-dequeued records would never reach `written_total` or
    `dropped_total`, so the drain equation (`enqueued == written + dropped`)
    can never rebalance again for that run. `limen verdict` has no recovery
    path for this and does not need one: the drain loop simply times out and
    the verdict exits `40` (drain timeout) — fail-closed in the right
    direction, since a stuck drain is a more honest signal than a verdict that
    quietly under-counts. No special-casing was added for this because the
    ordinary timeout path already produces the correct typed outcome.

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
