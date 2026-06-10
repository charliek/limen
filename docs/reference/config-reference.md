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
routes: [ … ]          # the routing table
```

## `server`

| Field | Type | Default | Notes |
|---|---|---|---|
| `listen_addr` | string | `0.0.0.0:8080` | Data-plane bind address (`IP:port`). |
| `graceful_shutdown_timeout_ms` | int | `10000` | Drain window on shutdown; must be > 0. |
| `request_body_limit_bytes` | int | `1048576` | Hard cap on buffered request bodies; must be > 0. |

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

## `routes[]`

Each route declares exactly one mode and the upstreams it needs.

| Field | Type | Default | Notes |
|---|---|---|---|
| `id` | string | — | Unique; also a metric label. |
| `match.methods` | list | — | HTTP methods; at least one, all known verbs. |
| `match.path_prefix` | string | — | Must start with `/`; longest prefix wins. |
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
  sample_rate: 0.1               # 0–1; fraction of eligible reads to buffer & compare
  max_body_bytes: 262144         # skip comparison above this size
  # Inline behavioral rules (only if NOT referencing a contract):
  # compare_status / compare_body / compare_headers / json: { … }
```

`enabled`, `sample_rate`, and `max_body_bytes` are **operational** — they live
here. The *behavioral* rules (`json.ignore_paths`, etc.) belong in a
[contract](contract-reference.md); a route may reference a contract **or** inline
those rules, never both (a validation error).

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
and the `failover_safe` gate — collecting **all** problems and naming the
offending field and route. A full valid example lives at
`config/limen.example.yaml`.
