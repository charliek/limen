# Flags & rollout

Once a route's shadow parity is clean, you move it to `percentage_split` and
raise traffic to the new service in steps — at runtime, via a feature flag, with
no redeploy. This page covers how Limen reads flags and assigns traffic.

## Feature-flag providers

Flags sit behind a provider trait so the source is swappable (spec §8). All
providers cache values and refresh them out of band; every read is served from
the cache. Crucially, all of them **keep the last known good values** on a
failed refresh and **fail safe** when values go stale.

| Provider | Source | Refresh | Staleness |
|---|---|---|---|
| `static` | values in config | never | never stale |
| `file` | a YAML `key: value` file | polled (`refresh_interval_ms`) | stale after `stale_ttl_ms` |
| `redis` | keys under `key_prefix` | polled (`refresh_interval_ms`) | stale after `stale_ttl_ms` |

```yaml
flags:
  provider: "file"
  file:
    path: "./flags.local.yaml"
    refresh_interval_ms: 1000
  stale_ttl_ms: 30000
  fail_safe_mode: "legacy_only"
```

A flags file is a flat map; values are numbers, booleans, or strings:

```yaml
# flags.local.yaml
migration.get-device.rollout_percentage: 25
migration.get-device.shadow_enabled: true
```

File and Redis changes take effect **without restarting** Limen — the next poll
picks them up. An invalid file update or a Redis connection failure leaves the
previous values in place (last known good) and increments a failure count; once
the values are older than `stale_ttl_ms`, the provider reports stale.

!!! warning "Stale flags fail safe to legacy"
    If the provider is stale (or has never successfully refreshed — an
    unreachable Redis at startup, a missing file), Limen applies `fail_safe_mode`
    (`legacy_only`): every `percentage_split` route routes to **legacy**,
    regardless of the configured percentage. A stale rollout flag therefore never
    silently shifts traffic to new — it falls back to the proven path.

## Deterministic percentage rollout

A `percentage_split` route resolves its percentage from a flag and assigns each
request deterministically (spec §6.4):

```yaml
routes:
  - id: "get-user"
    match: { methods: ["GET"], path_prefix: "/users/" }
    legacy_upstream: "https://legacy.internal"
    new_upstream: "https://new.internal"
    mode: "percentage_split"
    rollout:
      percentage_flag: "migration.get-user.rollout_percentage"
      default_percentage: 0
      assignment_key:
        header: "x-tenant-id"
        fallback: "request_random"
```

For each request Limen:

1. Reads the percentage from `percentage_flag` (falling back to
   `default_percentage` if unset), clamped to 0–100.
2. Derives the **assignment key** — the value of `assignment_key.header`, or a
   per-request random key if the header is absent (`request_random`).
3. Hashes `route_id + ':' + assignment_key` into a bucket `0..10000` (`blake3`).
4. Chooses **new** if the bucket is below `percentage × 100`, else **legacy**.

Because the hash is deterministic, **a given tenant (or user) is stable** for a
route across requests — they don't flip back and forth as unrelated traffic
flows. Hashing the route id in means the same tenant can be assigned differently
on different routes, so rollouts are independent. `0%` routes everyone to legacy;
`100%` routes everyone to new.

**Raising the flag only ever adds keys.** Because the bucket a key falls in
(`blake3(route_id + ':' + assignment_key) % 10000`) never changes, the new-side
key set at one percentage is a strict subset of the new-side key set at any
higher percentage for the same route — `newkeys(p1) ⊆ newkeys(p2)` whenever
`p1 <= p2`. This isn't a statistical tendency to expect on average; it's exact
arithmetic for a fixed key set, which is what makes the operator reasoning
sound: raising the flag can only *add* traffic to new, never silently move a
key that was already there back to legacy, and lowering the flag back down
returns **exactly** the prior key set, not a merely similarly-sized one. The
rollout simulation (below) proved both directions against real traffic —
monotone nesting at every ramp rung, and exact set-equality on a rollback.

## Rollout target gauge

`limen_rollout_resolved_target_percentage{route}` exports, per `percentage_split`
route, the percentage Limen resolved from the flag at scrape time — the same
stale/flag/default/clamp chain the router itself uses, so the gauge and the
routing decision can never quietly disagree. It is deliberately the **target**,
not the *effective* share: an open circuit breaker steers traffic away from new
without moving this gauge, because the flag's resolved value hasn't changed —
only where it's safe to send that traffic has. When the flag provider is stale,
the gauge reads `0` (the `fail_safe_mode: legacy_only` value), with the
`limen_flag_provider_stale` / `limen_flag_provider_staleness_seconds` gauges
saying why. Use it to confirm a flag change actually took before trusting
anything measured downstream of it — see the [runbook's monitoring
section](../runbook.md#10-monitoring-validating-the-proxy-itself).

## Raising the rollout

Raise the flag in steps, pausing at each to recheck the budget on real traffic
(see the [migration runbook](../runbook.md)):

```
0%  →  1%  →  5%  →  25%  →  50%  →  100%
```

This is no longer a pointer to a plausible-sounding procedure — it's what the
rollout simulation ran live, 2026-08-16, against slauth's two real
backends (Python legacy, Rust new) behind a real limen process, under 1000
keyed clients (`x-rollout-key`) making two passes per route at every rung.
Every stage's verdict checked exact stage boundaries, exact central-binomial
bounds on the observed split, two-pass stickiness, and the monotone-nesting
property above — and the drills went further than the happy path: killing the
new backend at 50% opened the breaker on every split route and proved both
arms of the failover-safe/not distinction live (client-invisible replay vs.
visible failure — see [resilience & failover](resilience.md)); a restart
walked the breaker back through half-open to closed and recovered the exact
pre-kill key set; a rollback from 50% to 5% returned exactly the earlier 5%
set; and stopping flag updates altogether held the last-known-good split until
`stale_ttl_ms`, then routed every split route to legacy — fail-safe, observed
by the actual traffic shift, not just asserted in logs.

Because traffic shifting is flag-driven, rollback is fast and reversible — lower
the flag to the last-green percentage (no redeploy), or let the
[circuit breaker](resilience.md) trip. Writes move via `percentage_split` too — each
request goes to exactly one implementation, so a side effect is never doubled.

**Residuals.** The simulation ran a synthetic keyed workload, not an organic
traffic mix, through one limen process on one host with no load balancer in
front — a production topology's flag-propagation timing and traffic shape will
differ from what's measured here even though the underlying mechanisms are the
same ones proven live.

## Tuning

Two knobs from `flags:` (spec §8) trade responsiveness against blast radius,
and the [rollout simulation](https://charliek.github.io/limen/runbook/#8-stages-67-shadow-then-roll-out-with-budgets)
is the source for the numbers below, not a guess:

| Setting | Sim value | Production default | Why they differ |
|---|---|---|---|
| `flags.file.refresh_interval_ms` | `500` | `1000` | The sim wanted flag flips to land inside a short ladder run without weakening any stage's assertions (the evaluator's bounds are computed from `n`/`p` at run time, never hand-tuned to the poll rate). A production poll every second is frequent enough that no rollout step is meaningfully delayed by it, and it's a quarter of the load on the flag source. |
| `flags.stale_ttl_ms` | `8000` | `30000` | A short TTL makes the staleness drill fast to run locally. In production a longer TTL tolerates a brief flag-source hiccup (a deploy, a network blip) without dumping traffic back to legacy on every transient failure — the cost of a longer TTL is a longer window riding last-known-good values if the source is genuinely down, which is still fail-safe, just slower to notice. |

Both directions are legitimate: tighten `stale_ttl_ms` toward `refresh_interval_ms`
when a stale rollout decision is expensive to leave uncaught, loosen it when the
flag source itself is less reliable than the traffic you're protecting.

**Soak per stage.** The ladder's pause at each percentage ([runbook
§8.4](../runbook.md#84-stage-7-percentage-rollout)) is not a fixed
duration — hold each rung long enough to accumulate a sample the budget's
tolerances can actually resolve. At 1%, a route doing ten requests a second
needs materially longer to say anything about new-side error rate than the
same route at 50%; the simulation's own per-stage verdicts size their sample
this way rather than against a wall-clock timer.

**Sizing `primary_ms` — a worked example.** The simulation's first run
(`run-accept-1`) hit this directly: Limen's default `timeouts.primary_ms`
(`2000`) had **zero headroom** over one proxied route's own healthy tail under
16-way driver concurrency — the route's p99 reached within single-digit
milliseconds of the deadline, and one request in 11,000 crossed it, correctly
producing a `504` that then failed that stage's validity guard. The fix was
not to raise the number blindly; it was to measure the route's real p99 under
representative concurrency and set an explicit timeout with deliberate margin
above it (`10000` for that harness) rather than trust the default, which is a
generic starting point, not a promise about any particular upstream's tail
latency. Do this per route before a rollout campaign, not after the first
false-alarm timeout.
