# Quickstart

This page walks through the shape of a Limen deployment: a configuration file,
the control commands, and the lifecycle of a migrating route. A fully runnable
Docker Compose example (mock legacy + new + Limen) ships under `examples/`.

## 1. Describe your routes

Limen is configured with a single YAML file. A minimal route that shadows reads
from a legacy service to a new one looks like this:

```yaml
# limen.config.yaml
server:
  listen_addr: "0.0.0.0:8080"          # data plane (proxied traffic)

metrics:
  listen_addr: "0.0.0.0:9090"          # control plane (/metrics, /health/*)

flags:
  provider: "static"
  static:
    values: {}

routes:
  - id: "get-device"
    match:
      methods: ["GET"]
      path_prefix: "/devices/"
    legacy_upstream: "https://legacy-device.internal"
    new_upstream: "https://new-device.internal"
    mode: "shadow_legacy_primary"      # serve legacy, shadow new, compare
    comparison:
      enabled: true
      sample_rate: 0.1                  # buffer + compare ~10% of reads
      max_body_bytes: 262144
```

The full set of fields is in the
[configuration reference](../reference/config-reference.md); the behavioral
comparison rules live in a separate, portable
[contract](../reference/contract-reference.md).

## 2. Validate before you run

Limen validates configuration *semantically*, not just syntactically — URL
shapes, percentage ranges, route-ID uniqueness, contract references, and
JSONPath compliance:

```bash
limen validate-config -c limen.config.yaml
limen print-routes    -c limen.config.yaml   # the resolved routing table
```

If a contract is referenced, check it independently — the same verdict Pharos
would produce:

```bash
limen check-contract ./contracts/device-service.contract.yaml
```

## 3. Run the proxy

```bash
limen run -c limen.config.yaml
```

Limen binds two listeners: the **data plane** on `server.listen_addr` for
proxied client traffic, and the **control plane** on `metrics.listen_addr` for
`/metrics`, `/health/live`, and `/health/ready`. The control plane is bound
separately so it can be firewalled off from public exposure.

## 4. The migration lifecycle

A route typically moves through these modes as confidence grows
([route modes](../reference/architecture.md#route-modes) explain each):

| Stage | Mode | Client is served | New service receives |
|---|---|---|---|
| Observe | `shadow_legacy_primary` | legacy | shadowed copies of reads |
| Roll out | `percentage_split` | legacy or new, by percentage | its share of live traffic |
| Cut over | `failover_to_legacy` / `new_only` | new | all traffic |

Traffic only advances when the parity, error-rate, and latency budgets hold —
and any breach trips the circuit breaker or lowers the rollout flag, returning
traffic to legacy. The [migration runbook](../runbook.md) defines those gates
in detail.

## Next steps

- [Architecture](../reference/architecture.md) — the two planes and two body
  paths.
- [CLI](../reference/cli.md) — every subcommand and flag.
