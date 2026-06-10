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

## Raising the rollout

Raise the flag in steps, pausing at each to recheck the budget on real traffic
(see the [migration runbook](../runbook.md)):

```
0%  →  1%  →  5%  →  25%  →  50%  →  100%
```

Because traffic shifting is flag-driven, rollback is fast and reversible — lower
the flag to the last-green percentage (no redeploy), or let the
[circuit breaker](resilience.md) trip. Writes move via `percentage_split` too — each
request goes to exactly one implementation, so a side effect is never doubled.
