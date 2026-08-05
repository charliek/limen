# Development setup

## Toolchain

Limen pins Rust **1.97.1** via both `.mise.toml` and `rust-toolchain.toml`.
Install [mise](https://mise.jdx.dev), then:

```bash
mise install                 # install the pinned toolchain
mise exec -- cargo --version # cargo 1.97.1
```

`.cargo/config.toml` sets `resolver.incompatible-rust-versions = "fallback"` so
the dependency graph resolves against the pinned compiler even when newer
transitive crates would otherwise demand a newer rustc.

## Everyday commands

All via the `Makefile` (run with `mise exec -- make <target>`):

| Target | What it runs |
|---|---|
| `build` / `release` | `cargo build` / `cargo build --release` |
| `run ARGS="…"` | `cargo run -- …` |
| `fmt` / `fmt-check` | `cargo fmt --all` (with/without `--check`) |
| `lint` | `cargo clippy --all-targets -- -D warnings` |
| `test` | `cargo test --all` |
| `bench` | `cargo bench` (criterion) |
| `check` | `fmt-check` + `lint` + `test` |
| `docs` / `docs-serve` | build / live-serve the docs site |

The quality gate before any commit is `mise exec -- make check`. CI enforces the
identical checks plus a release build.

## Repository layout

```
limen/
  Cargo.toml            # crate + dependency manifest
  src/                  # library modules + thin binary (see Architecture)
  tests/                # integration tests (wiremock / test servers)
  benches/              # criterion SLO benchmarks
  config/               # example config, flags, and contract files
  examples/             # docker-compose local trial
  docs/                 # this documentation site (Zensical)
```

## How the build is structured

Limen is built in phases (spec §14), each ending with a green quality gate and a
runnable artifact. Submodules are introduced in the phase that implements them,
so the module tree grows with the feature set rather than starting as empty
stubs.

| Phase | Scope |
|---|---|
| 0 | Scaffold, toolchain, CI, docs skeleton |
| 1 | Config + contract loading and validation |
| 2 | HTTP core: routing + `legacy_only` + `new_only` |
| 3 | Comparison engine |
| 4 | Shadowing: `shadow_legacy_primary` |
| 5 | Flags + rollout: `percentage_split` |
| 6 | Resilience: circuit breaker + `failover_to_legacy` |
| 7 | Observability hardening + graceful shutdown |
| 8 | Performance validation + examples + docs |

## Conventions

See `CLAUDE.md` at the repo root for the full set. The load-bearing rules:
default to legacy when uncertain; never block the client on shadow/comparison;
never shadow writes by default; never replay a non-idempotent in-flight request;
never log secrets; bound all buffers; validate at startup.
