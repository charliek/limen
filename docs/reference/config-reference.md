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
observe: { … }         # optional: passive per-route traffic profiling
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
| `match.path_prefix` | string | — | Must start with `/`; longest prefix wins. Exactly one of `path_prefix` / `path_template` required. |
| `match.path_template` | string | — | One exact path shape with `{param}` segments (see below). Exactly one of `path_prefix` / `path_template` required. |
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

**Precedence.** Every `path_template` route is tried **before** every
`path_prefix` route; within the template tier the fewest-parameters template
wins (more literal = narrower), within the prefix tier the longest prefix
still wins outright. At an *equal* key in either tier a query-conditioned
route beats an unconditioned one (in either config order); config order is the
final tiebreak. Two conditioned routes on the same prefix and method (or the
same path template) are rejected at load time unless **provably disjoint** —
some parameter appears in one route's `query_present` and the other's
`query_absent`. The check is conservative on purpose: anything it cannot prove
disjoint is an error, rather than letting config order silently decide.

That rule governs pairs of *equal* path rank only. Where the path itself
already orders the pair — a longer prefix, or a strictly narrower template —
the path decides first, conditioned or not: a conditioned narrower template
sitting over a conditioned broader one is ordinary refinement (the narrower
wins where it matches, and its condition narrows only its own shape) and needs
no disjointness. The one exception is the steal orientation in the overlap
table below — a narrower *unconditioned* template over a broader
query-conditioned one — which is rejected at load time.

A route declaring neither field matches exactly as it did before these fields
existed.

### `match.path_template` (route by shape, not just by subtree)

A route may name its paths with a template instead of a prefix — exactly one
of `path_prefix` / `path_template` per route, never both, never neither (spec
§5.2). A template is one exact shape: `{name}` spans exactly one non-empty
path segment, so `/conversations/{id}` matches `/conversations/42` but not
`/conversations/42/messages` or `/conversations`. Matching never
percent-decodes — `%2F` inside a segment stays one character of that segment —
and a request path with an empty segment (`//`) or a trailing slash never
matches a template; it falls through to the prefix tier instead.

```yaml
routes:
  - id: "get-conversation"
    match: { methods: ["GET"], path_template: "/conversations/{id}" }
    ...
  - id: "export-conversations"
    match: { methods: ["GET"], path_template: "/conversations/export" }
    ...
```

Reach for a template when one path under a prefix behaves differently from its
siblings and no prefix can carve it out — `/conversations/export` is a report,
every other `/conversations/<id>` is a fetch, and no prefix names that split.
The all-literal `export` template above is the narrower of the two shapes and
is matched first.

**Syntax rules**, enforced at `validate-config` time:

- Starts with `/`; no segment may be empty — no `//`, no trailing slash, and
  the bare `/` on its own is rejected, since a matching path's segments are
  never empty.
- A `{name}` parameter must span a whole segment (`/v{n}` and `/{a}b` are
  rejected, not silently read as literal text); `name` must be a valid
  identifier (`[A-Za-z_][A-Za-z0-9_]*`) and must not repeat within one
  template.
- At least one segment must be a literal — an all-parameter template would
  match every path of its length and be consulted ahead of every
  `path_prefix` route, which is a catch-all wearing a template's clothes.

**Overlap.** Because templates are matched before prefixes, an overlapping
pair could silently steal a sibling route's traffic, so every pair of routes
whose methods overlap is validated against one decision:

| Pair | Accepted | Rejected |
|---|---|---|
| Two templates that can never match the same path (different segment counts, or a literal clash somewhere) | always | — |
| Two templates where one is strictly narrower (every differing segment is a literal on the narrow side, a parameter on the broad side) | the broader is unconditioned, the narrower is itself conditioned, or the pair is provably query-disjoint | the broader carries a query condition, the narrower does not, and the two are not provably disjoint — the narrower would win on every path and steal exactly the requests the condition exists to except |
| Two templates of the identical shape (parameter names aside) | exactly one is query-conditioned, or both are and are provably disjoint | both unconditioned — a template pair has no prefix length left to order them by, so this is a typo, not a precedence — or both conditioned without disjointness |
| Two co-matchable templates where neither is narrower | never | always — which one would win is an accident of config order, so rewrite one narrower or disjoint |
| A template and an unconditioned prefix that intersect | every path the template matches lies under the prefix (the template refines the prefix's subtree) | the template takes only part of the prefix's traffic — the pair would split it on a boundary neither route states |
| A template and a query-conditioned prefix that intersect | the pair is provably query-disjoint | not provably disjoint — the template would take the requests the conditioned prefix exists to except |

"Provably disjoint" is the same rule used above for two conditioned prefixes:
some parameter in one route's `query_present`, the other's `query_absent`.
Error messages quote the two route ids and, for a template pair, a concrete
path that matches both, so you can see exactly what collides.

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
| `min_comparisons` | int | unset (= `1`) | The minimum number of comparisons [`limen verdict`](cli.md#verdict) requires this route to have recorded for its floors check to pass. `0` opts the route out of the floor explicitly — a visible exemption, not a silent gap. **Read only by `limen verdict`**; the proxy itself ignores it, so it has no effect on `run`, `validate-config`, or `print-routes`. A campaign config in which no enabled route carries a non-zero floor fails the floors check outright — a verdict over a config that compares nothing proves nothing. An explicit positive floor on a `enabled: false` route is a validation error — it could never be met, and silently dropping it from the floors check would fake coverage. |

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

## `observe`

Optional. Present = on; there is no separate `enabled` switch, like
[`diff_sink`](#diff_sink) — block presence alone is the trigger. This is
**unlike [`debug`](#debug)**: an empty `debug: {}` does *not* enable
`sink_canary`, since that field is a bool defaulting to `false` inside the
block; an empty `observe: {}` *does* enable observation, since `observe` has
no such inner switch. Turns on a strictly passive,
bounded per-route traffic profile — no shadow request, no second upstream
contact, no body byte read — served as JSON from `GET /observe/profile` on the
control plane and consumed by [`limen suggest-routes`](cli.md#suggest-routes).
See the [observe mode guide](../guides/observe-mode.md) for the end-to-end
workflow and [classifying routes](../guides/classifying-routes.md) for what
the profile can and cannot tell you.

```yaml
observe:
  sample_rate: 1.0      # 0-1, default 1.0
  max_query_names: 32   # per-route cap on distinct query-parameter names
  max_path_shapes: 32   # per-route cap on distinct observed paths
  max_fingerprints: 32  # per-route cap on stability fingerprints
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `sample_rate` | float | `1.0` | Fraction of relayed responses to observe (0–1). Below `1.0`, `limen suggest-routes` refuses to classify the resulting profile — sampling and classification are mutually exclusive, since the classifier's danger rules are existential and a dropped observation could have been the decisive one. |
| `max_query_names` | int | `32` | Cap on the distinct query-parameter *names* recorded per route (values are never read). Must be `> 0` and at most `1024`. |
| `max_path_shapes` | int | `32` | Cap on the set backing a route's distinct-read-path *count* (paths are counted, never recorded). Must be `> 0` and at most `1024`. |
| `max_fingerprints` | int | `32` | Cap on the request fingerprints a route keeps for the response-stability signal. Must be `> 0` and at most `1024`. |

**The floor and the ceiling are both enforced.** `0` is rejected rather than
read as "unlimited" — a zero-capacity map records nothing, and an operator
writing `0` means "narrower," not "empty." The ceiling (`1024`) matters just as
much: each bound caps a map keyed by live traffic, and a value an operator can
set arbitrarily high is not a bound at all. Past the ceiling there is nothing
left to learn — a route with more than a thousand distinct query names or path
shapes has already answered the question the field exists to answer, and an
`overflow` flag in the profile carries the rest.

**`metrics.path` may not be `/observe/profile` while this block is present.**
The control plane registers the operator-supplied `metrics.path` and the fixed
profile path on the same router, and axum panics at router *build* time on a
duplicate route; validating the collision turns that abort into a
refuse-to-start (invariant 7). The check is conditional on the block: an
operator who never enables `observe:` is never told their `metrics.path` is
wrong for a reason that does not apply to them, and the same path is fine with
`observe:` absent.

Like `debug`, an active `observe:` block makes `limen run` log a warning at
startup — the profile discloses route topology and query-parameter names on
the control plane, so an operator should know it is live and should bind
[`metrics.listen_addr`](#metrics) to loopback (see the [observe mode
guide](../guides/observe-mode.md#prerequisite-bind-the-control-plane-to-loopback)).

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
on one prefix), that `match` sets exactly one of `path_prefix` / `path_template`
and that a `path_template` parses (see [`match.path_template`](#matchpath_template-route-by-shape-not-just-by-subtree)
for the syntax and overlap rules), and the `failover_safe` gate — collecting
**all** problems and naming the offending field and route. A full valid
example lives at `config/limen.example.yaml`.
