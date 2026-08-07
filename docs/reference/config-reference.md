# Configuration reference

Limen is configured with one YAML file (`limen.config.yaml` by default).
Configuration is layered, later sources overriding earlier ones (spec §5.1):

1. Built-in defaults
2. Config file (`--config`)
3. Environment variables (`LIMEN_*`)
4. CLI flags

Every field has a built-in default, so a minimal file parses and the rest fall
back to safe values. Validate a file before running it:

```bash
limen validate-config -c limen.config.yaml
```

## Top-level structure

```yaml
server: { … }          # data-plane listener + request limits
metrics: { … }         # control-plane listener
upstream_tls: { … }    # TLS for upstream calls
flags: { … }           # feature-flag provider + fail-safe
diff_sink: { … }       # optional: persist comparison mismatches to JSONL
routes: [ … ]          # the routing table
debug: { … }           # optional: debug-only affordances (never in production)
```

## `server`

| Field | Type | Default | Notes |
|---|---|---|---|
| `listen_addr` | string | `0.0.0.0:8080` | Data-plane bind address (`IP:port`). |
| `graceful_shutdown_timeout_ms` | int | `10000` | Drain window on shutdown; must be > 0. |
| `request_body_limit_bytes` | int | `1048576` | Hard cap on buffered request bodies; must be > 0. |
| `shadow_concurrency_limit` | int | `100` | Max concurrent in-flight shadow requests across all routes; excess shadows are skipped, not queued (`0` = no limit). |

## `metrics`

| Field | Type | Default | Notes |
|---|---|---|---|
| `listen_addr` | string | `0.0.0.0:9090` | Control-plane bind address (`IP:port`). |
| `path` | string | `/metrics` | Prometheus exposition path. |

## `upstream_tls`

| Field | Type | Default | Notes |
|---|---|---|---|
| `verify_certificates` | bool | `true` | Verify upstream certificates. |
| `ca_bundle_path` | path | `null` | Optional custom CA bundle for internal PKI; must exist if set. |

## `flags`

| Field | Type | Default | Notes |
|---|---|---|---|
| `provider` | enum | `static` | `static` \| `file` \| `redis`. |
| `static.values` | map | `{}` | Flag values for the static provider. |
| `file.path` | path | `./flags.local.yaml` | YAML flags file (file provider). |
| `file.refresh_interval_ms` | int | `1000` | Poll interval; must be > 0. |
| `redis.url` | string | `redis://localhost:6379` | `redis://` or `rediss://` URL (redis provider). |
| `redis.key_prefix` | string | `limen:flags:` | Key prefix under which flags live. |
| `redis.refresh_interval_ms` | int | `1000` | Poll interval; must be > 0. |
| `stale_ttl_ms` | int | `30000` | After this staleness, apply `fail_safe_mode`; must be > 0. |
| `fail_safe_mode` | enum | `legacy_only` | Behavior when flags are stale/unavailable. |

Only the *selected* provider's settings are validated. Providers and their
runtime behavior are specified in the [Limen spec](../limen_spec.md) (§8); a
dedicated guide lands with the flags phase.

## `diff_sink`

Optional. Present = on; there is no separate `enabled` switch.

```yaml
diff_sink:
  dir: "./limen-diffs"
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `dir` | path | — | Directory for the daily `mismatches-<UTC date>.jsonl` files. Must be non-empty; need not exist (created on the first mismatch). Relative paths resolve against the process working directory, like `flags.file.path`. |

Every comparison **mismatch** is appended as one JSON line — already redacted by
the comparison engine — alongside the usual metrics and mismatch log, which are
unaffected. Read the files back with
[`limen report`](cli.md#report); the record shape and the retention stance are
specified in the [Limen spec](../limen_spec.md) (§10.4) and explained in the
[observability guide](../guides/observability.md#durable-mismatch-diffs).

## `routes[]`

Each route declares exactly one mode and the upstreams it needs.

| Field | Type | Default | Notes |
|---|---|---|---|
| `id` | string | — | Unique; also a metric label. |
| `match.methods` | list | — | HTTP methods; at least one, all known verbs. |
| `match.path_prefix` | string | — | Must start with `/`; longest prefix wins. |
| `match.query_present` | list | `[]` | Query parameter names that must **all** be present (see below). |
| `match.query_absent` | list | `[]` | Query parameter names of which **none** may be present. |
| `legacy_upstream` | url | — | Required unless mode is `new_only`. |
| `new_upstream` | url | — | Required unless mode is `legacy_only`. |
| `mode` | enum | — | `legacy_only` \| `new_only` \| `shadow_legacy_primary` \| `percentage_split` \| `failover_to_legacy`. |
| `contract` | string | `null` | `path#routeId` reference; conflicts with inline behavioral rules. |
| `failover_safe` | bool | `false` | Allow replaying a failed in-flight request to legacy (see below). |
| `rollout` | block | `null` | Required for `percentage_split`. |
| `timeouts.primary_ms` / `shadow_ms` | int | `2000` / `2000` | Must be > 0. |
| `comparison` | block | disabled | Operational gate + optional inline behavioral rules. |
| `circuit_breaker` | block | disabled | Per-route breaker (spec §9.1). |
| `budget` | block | `null` | Forward-looking rollout budget (validated, not enforced in MVP). |

### `match.query_present` / `match.query_absent` (query-aware matching)

Two optional presence conditions that narrow a route beyond method + path
(spec §5.2). Presence only — values never participate, so `?prompt=` counts
exactly like `?prompt=login`, and names are compared after the same
percent-decoding the comparison engine applies.

Write names **as they decode**: `login_verifier`, never `login%5Fverifier` or
`a+b`, and with no leading or trailing whitespace. The decoding is
one-directional — the request side is decoded, the config side is a literal — so
an encoded name here would match nothing. Validation rejects those spellings
rather than normalizing them, because a condition that silently matches nothing
lets the traffic it was meant to except fall through to a shadowing sibling.

```yaml
# The verifier hops relay; everything else on the path stays compared.
- id: "oauth-login-hop"
  match:
    methods: ["GET"]
    path_prefix: "/oauth2/auth"
    query_present: ["login_verifier"]   # all of these must be present
  legacy_upstream: "https://legacy.internal"
  mode: "legacy_only"

- id: "oauth-authorize"
  match:
    methods: ["GET"]
    path_prefix: "/oauth2/auth"
    query_absent: ["login_verifier"]    # none of these may be present
  legacy_upstream: "https://legacy.internal"
  new_upstream: "https://new.internal"
  mode: "shadow_legacy_primary"
  comparison: { enabled: true, sample_rate: 0.1 }
```

Reach for this when one path carries traffic that is not uniformly safe to
shadow. The field case is an OAuth authorize endpoint: the initial bounce is
safely comparable, but the `login_verifier` / `consent_verifier` hops replay
one-time tokens, so the shadow's copy deterministically fails at the shared
authorization server ("The consent verifier has already been used"). Splitting
the route relays those hops uncompared and keeps the bounces compared.

**Precedence.** Longest `path_prefix` still wins outright; at an *equal* prefix
a query-conditioned route beats an unconditioned one (in either config order);
config order is the final tiebreak. Two conditioned routes on the same prefix
and method are rejected at load time unless **provably disjoint** — some
parameter appears in one route's `query_present` and the other's `query_absent`.
The check is conservative on purpose: anything it cannot prove disjoint is an
error, rather than letting config order silently decide.

A route declaring neither field matches exactly as it did before these fields
existed.

### `rollout`

```yaml
rollout:
  percentage_flag: "migration.list-devices.rollout_percentage"
  default_percentage: 0          # 0–100
  assignment_key:
    header: "x-tenant-id"        # optional
    fallback: "request_random"   # used when the header is absent
```

### `comparison`

```yaml
comparison:
  enabled: true                  # operational gate
  sample_rate: 0.1               # 0–1; fraction of eligible requests to buffer & compare
  max_body_bytes: 262144         # skip comparison above this size
  min_comparisons: 1             # limen verdict's per-route floor; 0 = explicit exemption
  shadow_methods: []             # writes opted into shadowing; default [] = GET/HEAD only
  # Inline behavioral rules (only if NOT referencing a contract):
  # compare_status / compare_body / compare_headers / json: { … }
```

`enabled`, `sample_rate`, `max_body_bytes`, `min_comparisons`, and
`shadow_methods` are **operational** — they live here. The *behavioral* rules
(`json.ignore_paths`, etc.) belong in a [contract](contract-reference.md); a
route may reference a contract **or** inline those rules, never both (a
validation error).

| Field | Type | Default | Notes |
|---|---|---|---|
| `min_comparisons` | int | `1` | The minimum number of comparisons [`limen verdict`](cli.md#verdict) requires this route to have recorded for its floors check to pass. `0` opts the route out of the floor explicitly — a visible exemption, not a silent gap. **Read only by `limen verdict`**; the proxy itself ignores it, so it has no effect on `run`, `validate-config`, or `print-routes`. A campaign config in which no enabled route carries a non-zero floor fails the floors check outright — a verdict over a config that compares nothing proves nothing. |

### `comparison.shadow_methods` (shadowing a write)

Limen never shadows writes by default: only `GET`/`HEAD` are eligible. A route
that wants a write compared opts that method in explicitly:

```yaml
mode: shadow_legacy_primary
comparison:
  enabled: true
  sample_rate: 1.0
  max_body_bytes: 262144
  shadow_methods: ["POST"]
```

- Only `POST` may be listed today; `GET`/`HEAD` must **not** be listed (they are
  always eligible, and listing one suggests you expected the field to *restrict*
  eligibility, which it does not).
- The request body is buffered **once**, bounded by `max_body_bytes`, and the
  same bytes go to the primary and the shadow — identical payload and identical
  `Content-Length`. A body over the limit is never fully buffered: it streams to
  the primary unchanged and shadowing is skipped
  (`shadow_skipped{reason="request_too_large"}`).
- Only that bounded buffering is on the client path; the shadow request and the
  comparison stay fire-and-forget, as for reads. If `shadow_concurrency_limit`
  is already saturated, the body isn't buffered at all — the request is
  forwarded straight through and counted as
  `shadow_skipped{reason="concurrency_limit"}`.
- Validation rejects a listing that could never take effect: a non-`POST`
  method, a mode other than `shadow_legacy_primary`, `enabled: false`, or a
  method missing from the route's `match.methods`.

Opt in only where handling the request twice is acceptable — the new upstream
receives a *real* write.

### `budget`

```yaml
budget:
  max_new_p95_latency_ratio: 1.0   # positive
  max_new_error_rate_ratio: 1.0    # positive
  max_mismatch_rate: 0.001         # 0–1
```

## `failover_safe` and idempotency

A `failover_to_legacy` route whose methods include a non-idempotent verb (`POST`
or `PATCH`) **must** set `failover_safe: true`, or validation fails. This forces
the operator to consciously affirm that retrying a failed in-flight request
against legacy cannot double a side effect (spec §6.5). Routing *subsequent*
requests to legacy via the circuit breaker is always safe and never gated.

## `debug`

Optional. Absent (the normal case) means every debug affordance is off.
`limen run` logs a loud warning at startup for anything enabled here — these
switches exist to prove the comparison pipeline bites during a migration
campaign (see [Prove your lens bites](../guides/prove-your-lens-bites.md)),
never for production operation.

```yaml
debug:
  sink_canary: true
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `sink_canary` | bool | `false` | Exposes `POST /debug/canary` on the control plane, which injects one synthetic mismatch through the real compare → observer → sink pipeline under the reserved route id `__limen_canary__`. Drives [`limen verdict --canary`](cli.md#verdict). |

**Never enable in production.** `limen run` emits a `warn`-level log
(`"debug sink canary enabled — POST /debug/canary injects synthetic
mismatches into the live pipeline; never enable in production"`) whenever
`sink_canary` is true, so an accidental production enablement is loud rather
than silent.

**Why config-gated, not compile-gated.** The obvious alternative —
`cfg(debug_assertions)` or a Cargo feature — was rejected for a load-bearing
reason: campaign runners build limen with `--release`, where
`debug_assertions` is off. A compile-time gate would make the canary
unavailable exactly where the falsification story needs it. Config-gating
keeps the endpoint out of release builds' *default* behavior (off unless a
config explicitly turns it on) without making it unreachable in a release
binary.

Route IDs starting with `__` are reserved for limen-internal records (today:
the sink canary's `__limen_canary__`); config validation rejects any user
route declaring one, so a canary record can never be confused with a real
mismatch.

## Environment overrides

The documented `LIMEN_*` overrides (applied above the file, below CLI flags):

```bash
LIMEN_CONFIG=./limen.config.yaml
LIMEN_LISTEN_ADDR=0.0.0.0:8080
LIMEN_METRICS_ADDR=0.0.0.0:9090
LIMEN_FLAGS_PROVIDER=redis
LIMEN_REDIS_URL=redis://localhost:6379
LIMEN_FAIL_SAFE_MODE=legacy_only
```

The same knobs are available as CLI flags (`--listen-addr`, `--metrics-addr`,
`--flags-provider`, `--redis-url`, `--fail-safe-mode`), which take final
precedence.

## Validation

`validate-config` is semantic, not just a parse. It checks socket addresses,
upstream URL shapes, percentage and ratio ranges, timeout sanity, route-ID
uniqueness, known methods, per-mode required upstreams, contract reference
resolution, the contract-vs-inline conflict rule, JSONPath-subset compliance,
a non-empty `diff_sink.dir`, the query-condition rules (non-empty unique names,
written as **literal decoded names** — no `%`, `+`, or edge whitespace, since the
request's query is percent-decoded before comparison and an encoded spelling
could never match; no name in both fields; provably disjoint conditioned routes
on one prefix), and the `failover_safe` gate — collecting **all** problems and naming the offending
field and route. A full valid example lives at `config/limen.example.yaml`.
