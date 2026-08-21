# Resilience & failover

Migration is the riskiest time for a service, so Limen's defaults all lean one
way: **when anything is uncertain, serve legacy.** This page covers the two
mechanisms that protect a rollout when the new service misbehaves — the
**circuit breaker** and **`failover_safe` replay** (under `failover_to_legacy`
and, now, `percentage_split` too) — and the one safety distinction that
governs both.

## The distinction that matters

There are two different things people call "failover", and only one of them is
always safe (spec §6.5):

- **Routing the *next* request to legacy** — because the circuit is open, or a
  rollout flag was lowered. No request is ever executed twice; this is always
  safe.
- **Replaying an *in-flight* request** that already reached new — because new
  just returned an error. This re-sends the request, so it is only safe when the
  operation is idempotent.

Limen treats these separately. The circuit breaker governs the first (safe)
case. The `failover_safe` flag governs the second (dangerous) case — and it is
opt-in, never defaulted.

!!! danger "Non-idempotent writes are never auto-replayed"
    A `POST` that may have already created a resource on new must **not** be
    retried against legacy — you would create it twice. Limen refuses to replay
    an in-flight request unless the route is explicitly `failover_safe: true`,
    and validation *requires* that flag on any `failover_to_legacy` route whose
    methods include `POST`/`PATCH`. The safety call is always conscious.

## Circuit breaker

A per-route breaker guards the **new** upstream (spec §9.1). It is **closed** by
default and tracks the new-side failure rate; failures are 5xx responses,
connection failures, and timeouts.

```yaml
routes:
  - id: "get-device"
    match: { methods: ["GET"], path_prefix: "/devices/" }
    legacy_upstream: "https://legacy.internal"
    new_upstream: "https://new.internal"
    mode: "failover_to_legacy"
    failover_safe: true
    circuit_breaker:
      enabled: true
      failure_rate_threshold: 0.5   # open above 50% failures…
      min_requests: 20              # …once at least 20 requests have been seen
      open_duration_ms: 30000       # stay open 30s before testing recovery
      half_open_max_requests: 3     # then admit 3 trial requests
```

The breaker moves through three states:

| State | Behaviour |
|---|---|
| **Closed** | Traffic flows to new; failures are counted in a rolling window. |
| **Open** | New is avoided — requests route to **legacy**. After `open_duration_ms`, the next request becomes a half-open trial. |
| **Half-open** | Up to `half_open_max_requests` *trial* requests are admitted to new. All succeed → **closed**; any fails → back to **open**. |

```
        failure rate > threshold
 ┌────────┐  (after min_requests)   ┌──────┐
 │ Closed │ ─────────────────────▶  │ Open │
 └────────┘                          └──────┘
     ▲                                   │  open_duration_ms elapsed
     │ trials all succeed                ▼
     │                            ┌───────────┐
     └─────────────────────────── │ Half-open │
              any trial fails ───▶ └───────────┘  (reopens)
```

Opening the breaker is the *routing* kind of failover: it steers **subsequent**
requests to legacy without retrying any in-flight request, so it is safe for
every mode and method. A `failover_to_legacy` route gets breaker steering even
when `failover_safe` is `false` — the breaker never replays the failed request,
it only changes where the *next* one goes.

!!! note "Trial slots are accounted exactly once"
    A half-open trial reserves one of the `half_open_max_requests` slots. Limen
    settles that slot exactly once — recording the outcome if the request
    reaches new, or releasing it untouched if the request is rejected locally
    first (an unforwardable path, an over-limit body). A rejected request can
    therefore never wedge the breaker open or consume a trial it didn't use.

### Tuning for traffic volume

The four fields trade **how fast the breaker reacts** against **how much
traffic it costs to decide**. There's no universal answer — a route doing
thousands of requests a second can afford a strict, fast-tripping breaker;
a low-volume route needs `min_requests` high enough that a handful of
correlated failures (a deploy blip, one client retrying hard) doesn't trip
it on noise:

- **`min_requests`** — the sample floor before `failure_rate_threshold`
  is even consulted. Too low and the breaker opens on a coincidence; too
  high and a real outage runs longer before the breaker notices.
- **`failure_rate_threshold`** — how bad "bad" has to be. `0.5` (the
  default) tolerates a route that's flaky but still net-positive; tighten
  it for a route where any elevated failure rate is unacceptable.
- **`open_duration_ms`** — how long to avoid a sick upstream before probing
  it again. Short values recover fast from a transient blip but probe a
  genuinely down upstream more often (each probe is a real request);
  long values are gentler on a downed upstream but leave a recovered one
  parked on legacy longer than necessary.
- **`half_open_max_requests`** — how many trial requests it takes to trust
  a recovery. More trials give a more reliable signal at the cost of
  admitting more traffic to an upstream that might still be unhealthy.

The [rollout simulation](https://charliek.github.io/limen/guides/flags-and-rollout/#tuning)'s
kill drill is the worked example: at its traffic rate, the production
defaults (`min_requests: 20`, `open_duration_ms: 30000`,
`half_open_max_requests: 5`) would have made the drill slow without proving
anything more, so it ran with `min_requests: 10`, `open_duration_ms: 15000`,
`half_open_max_requests: 3`, and `failure_rate_threshold` left at the
default `0.5` — nothing about running locally motivated moving it. The
breaker still opened correctly on all five split routes when the new
backend was killed, and walked open→half_open→closed on restart, proven
from the transition counters below rather than a timer. Scale the
tuning to your own traffic, not to the sim's numbers directly.

**The observables are the transition counters and the state gauge, not a
log line you have to go looking for.** `limen_breaker_transitions_total{route,from,to}`
counts every state change; `limen_circuit_breaker_state` is the
scrape-time-sampled state. [`limen report --format
html`](https://charliek.github.io/limen/reference/cli/#the-html-status-page)
renders both per route in its Rollout & resilience section, so a breaker
that flapped between two scrapes is visible in the counters even when the
gauge caught it in either state.

## `failover_safe`: replay under `failover_to_legacy` and `percentage_split`

Two modes can put **new** in front of a live **legacy** and are therefore
eligible for `failover_safe` replay: `failover_to_legacy` (new is primary for
every request) and `percentage_split` (new is primary for whichever requests
the split assigned to it). `new_only` is excluded by construction — there is
no legacy leg to replay against.

In `failover_to_legacy` mode, two things can send a request to legacy instead
of new:

1. **Pre-flight steering** — if the breaker is open, the request goes straight
   to legacy and new is never contacted.
2. **Mid-flight replay** — new was contacted and failed. Only if the route is
   `failover_safe: true` does Limen replay the same request against legacy;
   otherwise it returns the new-side failure to the client. A "failure" here is a
   5xx response, a connection error or timeout, **or** a response whose body
   errors or times out mid-read — on the failover path Limen buffers the
   (bounded) new response before committing, so a `200`-then-broken-body is
   failed over rather than streamed to the client truncated.

Under `percentage_split`, mid-flight replay applies the same way to whichever
requests the split's key hash sent to new — a split-chosen-new key on a
`failover_safe: true` route replays to legacy on failure exactly as a
`failover_to_legacy` route would, client-invisibly. This is the widening the
[rollout simulation](https://charliek.github.io/limen/guides/flags-and-rollout/#tuning)
was built to exercise: a rollout ladder needs this behavior the moment the new
backend degrades at, say, 50% — and the kill drill proved both arms live,
`failover_safe` routes replaying while non-`failover_safe` routes failed
visibly, on the same split traffic.

```yaml
# Idempotent read: safe to replay, so a new-side failure is transparent.
- id: "get-order"
  match: { methods: ["GET"], path_prefix: "/orders/" }
  legacy_upstream: "https://legacy.internal"
  new_upstream: "https://new.internal"
  mode: "failover_to_legacy"
  failover_safe: true
```

To replay a request, Limen must first buffer its body (bounded by
`server.request_body_limit_bytes`). A body larger than that limit can't be
replayed faithfully, so the request is rejected with `413 Payload Too Large`
rather than sent un-replayable. The same bound caps the new *response* buffered
for body-level failover; a response past the bound is streamed to the client
as-is (header-level failover only), since a committed stream can't be replayed.

### One budget, not two

Both legs — the new attempt and the legacy replay — share **one**
`primary_ms` deadline; there is no separate clock for the replay. The
deadline is taken once, immediately before the new attempt is sent, and the
legacy replay (when it happens) gets only what is left of it, because it is
the second leg of *this* client request, not a fresh one — a client must
never wait roughly two timeouts deep for a route that declared one.

| `failover_safe` | New fails in-flight | Result |
|---|---|---|
| `true` | 5xx, connection refused/reset, or a body that errors mid-read — fails **before** the budget is spent | request replayed to legacy with whatever time is left; client sees legacy's response |
| `true` | timeout — the new attempt itself consumed the **whole** `primary_ms` budget | new's `504` returned to client; **not** replayed, because there is no time left to replay within |
| `false` (default) | 5xx / connection error / timeout | new-side failure returned to client; **not** replayed |

Replay buys resilience against failures that come back *fast* — a refused or
reset connection, a prompt 5xx — which is where nearly all of it lives. It
cannot buy resilience against a *slow* new upstream by doubling the latency
ceiling: a timeout has already spent the one budget the route declared, and
the [rollout simulation's kill drill](https://charliek.github.io/limen/guides/flags-and-rollout/#tuning)
is a live instance of the fast-failure case (connection refused after the
backend was killed), not the timeout case, so it doesn't exercise this
boundary directly — the boundary itself is fixture-proven.

In both cases the breaker still records the failure and steers later traffic.

### The cost of turning `failover_safe` on

`failover_safe: true` isn't free even on routes where it never fires. Know
what it costs before flipping it on a route mid-rollout:

1. **Request bodies buffer.** A route that used to stream its request body now
   buffers it (bounded by `server.request_body_limit_bytes`) so it can be
   replayed — an over-limit body that previously streamed through now gets a
   `413` instead.
2. **Responses buffer before commit.** New's response is held (bounded by the
   same limit) until it's known good, which costs latency-to-first-byte
   relative to streaming. An over-limit response still streams to the client,
   but the failover guarantee degrades to header-level only for it — the body
   can't be verified or replayed.
3. **A new-side `5xx` is masked by a legacy replay.** The client never sees
   it; only the breaker and the upstream-error metric do. Don't rely on
   client-visible errors to notice new degrading on a `failover_safe` route —
   watch the metrics instead.
4. **Worst-case latency stays within one `primary_ms`**, never two — the
   one-budget contract above — but that one budget is still the ceiling a
   failing-fast new upstream adds before the client sees legacy's response.

### Upgrading an existing config

A config that already sets `failover_safe: true` on a `percentage_split`
route gains replay semantics **on upgrade**, with no config change required —
the widening above applies to every existing route meeting the mode/flag
combination the moment the binary is upgraded. If a route was relying on
`percentage_split` returning new's raw failure to the client (the
`failover_safe: false` behavior, which is unchanged), nothing changes for it.
But a route that set `failover_safe: true` under `percentage_split` before
this behavior existed — when the flag had no effect there — starts replaying
for real. Audit `percentage_split` routes with `failover_safe: true` before
upgrading if that's a surprise you'd rather catch in review than in
production.

## Timeouts

Each route sets `timeouts.primary_ms` and `timeouts.shadow_ms` (spec §9.2):

- **`primary_ms`** is **one absolute deadline for the whole primary leg**, taken
  immediately before the request is sent. It bounds the *time to response*
  (connect + send + first byte) and — on a request sampled for comparison — the
  response buffering that follows it, from the same budget. A primary that never
  responds within it yields `504 Gateway Timeout` (or fails over, if
  `failover_safe`).
- **`shadow_ms`** bounds the entire shadow exchange. Because the shadow runs off
  the client path, **it can never extend client-visible latency** — a slow or
  hung shadow is abandoned without touching the client's response.

**Unsampled traffic keeps its unbounded stream.** On the default streaming path
the response body is relayed with no total deadline at all, so large or slow
downloads are never truncated. That is unchanged: the deadline exists only on
the *sampled* primary response leg, which is the only buffering that sits on the
client's response path.

**A sampled response that outlives the budget demotes; it is not failed.** When
buffering for comparison runs past what is left of `primary_ms`, Limen hands the
client the already-read prefix chained to the rest of the live stream and skips
the comparison
(`limen_comparison_skipped_total{reason="response_buffer_timeout"}`). The client
still receives the **complete** body — nothing is truncated and no error is
synthesized — and the worst-case time to first byte on a sampled route is back
to ≈ `primary_ms` rather than however long a trickling body cares to take. A
response declaring `text/event-stream` skips comparison *eagerly*, before a byte
is buffered (`reason="event_stream"`): an event stream never completes, so
buffering one could only ever stall the first byte and then skip anyway.

!!! note "What a dying body costs depends on when it dies"
    A body that errors **while still being buffered** has never had a byte sent
    to the client, so Limen replaces the upstream's `2xx` with its own `502` —
    a broken response is never passed off as a good one. A body that errors
    **after a demotion** cannot be recalled: the status and headers are already
    on the wire, so the client sees a truncated stream, exactly as it would on
    the ordinary streaming path. The demotion is the moment the response stops
    being retractable.

## Bounded buffers & shadow concurrency

Limen never buffers unbounded data (spec §9.3–9.4):

- **Request bodies** are buffered only when a route needs to replay them —
  `failover_safe`, up to `server.request_body_limit_bytes`, or a write method
  (`POST`, `PUT`, or `PATCH` — `DELETE` is not eligible) the route opted into
  shadowing via `comparison.shadow_methods`, up to `comparison.max_body_bytes`
  so the identical bytes reach both upstreams. The default streaming path
  buffers nothing. A shadow-eligible write's body over the limit is never
  fully buffered: it streams to the primary unchanged and shadowing is
  skipped, incrementing `shadow_skipped{reason="request_too_large"}`. Being
  eligible only means the buffering/replay path is method-agnostic and
  handles it identically to `POST` — it is not a claim that replaying any
  particular route's write twice is safe; that is the per-route idempotence
  analysis required before opting in (§6.1).
- **Comparison buffering** for shadows is bounded per route by
  `comparison.max_body_bytes`, **and in time** by the remainder of the route's
  `primary_ms` (above); an over-limit *or* out-of-budget response is streamed to
  the client with the comparison skipped. A size bound alone would still let a
  body that trickles under the limit hold the client's first byte indefinitely.
- **Concurrent shadows** are capped by `server.shadow_concurrency_limit`. Over
  the cap, shadows are **skipped** (never queued unboundedly), incrementing
  `shadow_skipped{reason="concurrency_limit"}` — checked *before* a
  write-shadowing route buffers its request body, too, so an already-saturated
  limit doesn't pay that buffering cost for a shadow it will refuse anyway.

These bounds protect the proxy's memory and shield the new upstream from a
shadow-traffic stampede while it's still warming up.

## Putting it together

A typical hardening sequence as a route matures:

1. **`shadow_legacy_primary`** — legacy serves; compare new in the background
   (see [Comparison & Contracts](comparison-and-contracts.md)).
2. **`percentage_split`** — shift real traffic in flag-driven steps (see
   [Flags & Rollout](flags-and-rollout.md)), with a breaker watching new.
3. **`failover_to_legacy`** — new is primary, with the breaker and (for
   idempotent routes) `failover_safe` as the safety net.

At every step, an unhealthy new service degrades to legacy rather than to the
client — the project's load-bearing fail-safe posture.
