# Limen

**A production-grade reverse proxy for safely migrating HTTP traffic from a
legacy service to a new implementation** — through shadowing, response
comparison, deterministic percentage rollout, and fail-safe fallback.

*Limen* is Latin for "threshold": the liminal state in which the old and new
implementations coexist and traffic crosses safely from one to the other, with
the ability to step back. The directionality is deliberate — every Limen route
can fail back to legacy.

## The problem it solves

Rewriting a service onto a supported framework is usually a *behavioral bet*:
the team reimplements the service, ships it, and hopes it behaves like the
original. Subtle differences — a changed error shape, a dropped field, a
timestamp format — surface in production as incidents, after the old
implementation is gone.

Limen removes the bet on the runtime side. It sits in front of two upstreams —
`legacy` (the current source of truth) and `new` (the replacement) — and lets
you prove parity against real traffic *before* users depend on the new service,
then shift traffic gradually and reversibly once it holds.

## What it does

- **Shadow** eligible read traffic to the new service and compare responses
  against legacy, surfacing divergence as a metric and a sampled diff —
  **without ever affecting the client response or its latency**.
- **Roll out** deterministically by percentage, controllable at runtime via
  feature flags (no redeploy), keeping a given tenant or user stable across the
  split.
- **Fail safe** to legacy whenever anything is uncertain: an unhealthy new
  upstream, an open circuit, stale flags, or ambiguous config.
- **Observe** everything: Prometheus metrics, structured logs, health
  endpoints, all with secrets redacted and labels kept low-cardinality.

```
            ┌──────────────── limen ────────────────┐
            │                                        │
 client ───▶│  match route → decide upstream         │───▶  legacy  (source of truth)
            │     │                                  │
            │     └─ shadow eligible reads ──────────│───▶  new     (the replacement)
            │        compare · hash · diff           │
            └────────────────────────────────────────┘
                 returns the primary response;
                 shadow + comparison never touch it
```

## How it fits the bigger picture

Limen is the **runtime** half of a two-tool migration approach:

- **[Pharos](pharos_spec.md)** (a separate TypeScript/Vitest project) is the
  deterministic, pre-production functional test suite. It validates the new
  service against legacy in development and CI, and *refines* the behavioral
  contract.
- **Limen** (this project) *consumes* that refined contract unchanged, applying
  the same normalization and comparison vocabulary to live shadow traffic.

The two share a [behavioral contract](limen_spec.md) — a portable YAML/JSON
description of what to compare and which incidental differences don't count —
but have **no build-time dependency** on each other.

The [migration runbook](runbook.md) is the operational procedure that ties them
together; the [PR/FAQ](prfaq.md) explains the motivation.

## Safety, by design

Limen's defaults all lean toward safety:

1. Default to **legacy** when uncertain.
2. **Never** block the client response on shadow or comparison work.
3. **Never** shadow writes by default — only where a route opts the method into
   `comparison.shadow_methods`, with a bounded body replayed to both upstreams.
4. **Never** replay a failed in-flight request against legacy unless the route
   is explicitly marked idempotent (`failover_safe: true`).
5. **Never** log secret values — redaction applies to every output surface.
6. **Bound** all buffers.
7. **Validate** config and contracts at startup; refuse to start on invalid
   input.

## Get started

- [Installation](getting-started/installation.md) — build with the pinned
  toolchain.
- [Quickstart](getting-started/quickstart.md) — proxy a single route end to end.
- [Architecture](reference/architecture.md) — how it's put together.
- [CLI](reference/cli.md) — the five subcommands.

For the full design, read the [Limen specification](limen_spec.md).
