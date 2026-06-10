# Limen

**A production-grade Rust reverse proxy for safely migrating HTTP traffic from
a legacy service to a new implementation** — through shadowing, response
comparison, deterministic percentage rollout, and fail-safe fallback.

> *Limen* is Latin for "threshold" — the liminal state in which the old and new
> implementations coexist and traffic crosses safely from one to the other,
> with the ability to step back. Every Limen route can fail back to legacy.

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

Limen sits in front of two upstreams — `legacy` (the current source of truth)
and `new` (the replacement) — and moves traffic between them without changing
user-facing behavior. It can:

- **Shadow** eligible read traffic to the new service and compare responses
  against legacy, emitting correctness signals **without ever affecting the
  client response**.
- **Roll out** deterministically by percentage, controllable at runtime via
  feature flags, with a given tenant/user kept stable across the split.
- **Fail safe** to legacy whenever anything is uncertain — new upstream
  unhealthy, circuit open, flags stale, config ambiguous.

It is the runtime half of a two-tool migration approach; the
[Pharos](docs/pharos_spec.md) functional test suite is the deterministic,
pre-production half. The two share a [behavioral contract](docs/limen_spec.md)
but have no build-time dependency on each other.

## Status

Limen is built in phases (see the [spec](docs/limen_spec.md), Section 14). This
repository tracks that build; consult `CLAUDE.md` for the conventions and the
[documentation site](#documentation) for usage.

## Build

Limen pins its Rust toolchain with [mise](https://mise.jdx.dev) (`.mise.toml`)
and `rust-toolchain.toml`. With mise installed:

```bash
mise install                 # install the pinned Rust toolchain
mise exec -- make check      # fmt-check, clippy (-D warnings), tests
mise exec -- make build      # debug binary at target/debug/limen
```

`make help` lists every target. CI runs the same `fmt --check`, `clippy -D
warnings`, `test`, and release build.

## Usage

```bash
limen run --config limen.config.yaml         # serve (data + control planes)
limen validate-config -c limen.config.yaml   # semantic validation
limen print-routes -c limen.config.yaml      # resolved routing table
limen check-contract path/to.contract.yaml   # validate a behavioral contract
```

Configuration is layered (defaults < file < environment < CLI). See the
[configuration reference](docs/reference/config-reference.md) and the example
config under `config/`.

## Documentation

The full site lives under `docs/` and builds with `mkdocs-material`
(`mise exec -- make docs-serve` → http://127.0.0.1:7071):

- **Getting started** — installation, quickstart, configuration
- **Guides** — route modes, comparison & contracts, flags & rollout,
  resilience, observability, deployment
- **Reference** — architecture, CLI, config, contract, metrics
- **Specifications** — the full Limen spec, the migration runbook, and the
  PR/FAQ that motivate the design

`CLAUDE.md` at the repo root captures the project conventions for contributors
and coding agents.

## License

MIT — see [LICENSE](LICENSE).
