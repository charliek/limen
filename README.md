# Limen

[![CI](https://github.com/charliek/limen/actions/workflows/ci.yml/badge.svg)](https://github.com/charliek/limen/actions/workflows/ci.yml)
[![Docs](https://github.com/charliek/limen/actions/workflows/docs.yml/badge.svg)](https://github.com/charliek/limen/actions/workflows/docs.yml)

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

Limen implements the full MVP from the [spec](docs/limen_spec.md): all five
route modes, the shadow comparison engine (including `set_cookie` and
`Location` as first-class comparison dimensions), a per-route opt-in to shadow
write methods, deterministic percentage rollout, the circuit breaker and
`failover_to_legacy`, Prometheus metrics, structured logs, an optional durable
mismatch diff sink with the `report` command, health endpoints, and bounded
graceful shutdown. See the spec's Section 14 for the phased build and
`CLAUDE.md` for conventions.

## Quickstart

### Docker Compose — no toolchain needed

Bring up the proxy in front of two mock upstreams with one command:

```bash
docker compose -f examples/docker-compose.yaml up --build
```

Then, in another shell:

```bash
curl localhost:8080/                                      # served by legacy
curl -s localhost:9090/metrics | grep limen_comparisons   # shadow comparison counts
curl localhost:9090/health/ready                          # -> ready
```

`legacy` and `new` return different bodies on purpose, so the shadow comparison
records a *mismatch* — Limen detecting a behavioral difference while the client
still gets legacy's response. That is the core loop, end to end.

### From a checkout

With [mise](https://mise.jdx.dev) installed (it reads `.mise.toml` to pin the
Rust toolchain):

```bash
mise install                                              # install pinned Rust
mise exec -- cargo build --release                        # -> target/release/limen
./target/release/limen validate-config -c config/limen.example.yaml
./target/release/limen print-routes    -c config/limen.example.yaml
```

`validate-config` and `print-routes` work against the example as-is. `run` also
expects reachable upstreams (the example uses `*.internal` placeholders) and the
flag file named by `flags.file.path` — copy `config/flags.example.yaml` to
`./flags.local.yaml`, or flag-driven routes simply fail safe to legacy. For a
self-contained *running* proxy, use the Compose demo above.

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
limen report --dir ./limen-diffs             # summarize a diff_sink directory
```

Configuration is layered (defaults < file < environment < CLI). See the
[configuration reference](docs/reference/config-reference.md) and the example
config under `config/`. `report` needs no config file — it reads the
[`diff_sink`](docs/reference/config-reference.md#diff_sink) directory directly;
see the [CLI reference](docs/reference/cli.md#report).

## Performance

Limen's per-request CPU work is microsecond-scale. `cargo bench` runs the
criterion suite (`benches/proxy_overhead.rs`), which microbenchmarks the two
dominant added costs against the spec's SLO budgets (§12). These are component
measurements, not end-to-end added latency, but they show the work sits far
inside the budget — representative figures from a developer laptop:

| Component | Time | SLO budget (added latency) |
|---|---|---|
| Route match (longest prefix) | ~5 ns | streaming p50 < 1 ms |
| Compare ~2 KB JSON bodies | ~35 µs | buffer-compare p50 < 3 ms |
| Compare ~40 KB JSON bodies | ~0.7 ms | buffer-compare p50 < 3 ms |

```bash
mise exec -- cargo bench
```

Production traffic that isn't sampled for comparison takes the zero-copy
streaming path; only the sampled fraction pays the buffer-and-compare cost, and
even that stays well inside budget for bodies up to the configured limit.

## Documentation

The full site lives under `docs/` and builds with [Zensical](https://zensical.org)
(`mise exec -- make docs-serve` → http://127.0.0.1:7071):

- **Getting started** — installation, quickstart
- **Guides** — comparison & contracts, flags & rollout, resilience & failover,
  observability & operations, deployment
- **Reference** — architecture, CLI, config, contract
- **Specifications** — the full Limen spec, the migration runbook, and the
  PR/FAQ that motivate the design

`CLAUDE.md` at the repo root captures the project conventions for contributors
and coding agents.

## License

MIT — see [LICENSE](LICENSE).
