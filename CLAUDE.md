# CLAUDE.md — Limen project conventions

Conventions and context for working in this repository (for contributors and
coding agents). Read this before making changes.

## What Limen is

A production-grade reverse proxy that safely migrates HTTP traffic from a
`legacy` upstream to a `new` one: read-path shadowing + response comparison,
deterministic percentage rollout via feature flags, and fail-safe fallback to
legacy. The authoritative design is `docs/limen_spec.md` (Section numbers
referenced throughout the code refer to it). `docs/runbook.md` and
`docs/prfaq.md` give operational and motivational context; `docs/pharos_spec.md`
describes the companion test suite that shares the behavioral contract.

## Toolchain

- Rust is pinned to **1.97.1** via both `.mise.toml` and `rust-toolchain.toml`.
- Cargo is reached through mise: `mise exec -- cargo …` (or
  `mise exec -- make …`). `.cargo/config.toml` sets
  `resolver.incompatible-rust-versions = "fallback"` so the dependency graph
  resolves against the pinned compiler.
- Docs tooling is Python via `uv` (`pyproject.toml`, `mkdocs-material`).

## Quality gate (run before every commit)

```bash
mise exec -- cargo fmt --all
mise exec -- cargo clippy --all-targets -- -D warnings   # warnings are errors
mise exec -- cargo test --all
```

`mise exec -- make check` runs fmt-check + clippy + test together. CI
(`.github/workflows/ci.yml`) enforces the same, plus a release build.

## Architecture (one screen)

- **Library + thin binary.** All logic lives in `src/lib.rs`'s modules; `main.rs`
  only parses the CLI, initializes logging, and dispatches. This keeps the proxy
  testable without binding sockets.
- **Two listeners** (Section 3.2): a *data plane* that proxies client traffic
  and a *control plane* (`/metrics`, `/health/live`, `/health/ready`) bound
  separately so it can be firewalled off.
- **Two body paths** (Section 3.3): a default zero-copy *streaming* path, and a
  bounded *buffer-for-compare* path used only when a request is sampled for
  comparison and within `max_body_bytes`.
- **Module map** mirrors `docs/limen_spec.md` Section 3.5:
  `config` · `contract` · `http` · `routing` · `compare` · `flags` ·
  `resilience` · `observability` · `health`. Submodules are added in the phase
  that implements them rather than created empty up front.

## Load-bearing safety invariants

These are the point of the project — never regress them:

1. **Default to legacy when uncertain** (unhealthy new upstream, open circuit,
   stale flags, ambiguous config).
2. **Never block the client response on shadow or comparison work** — shadowing
   is fire-and-forget and off the client path.
3. **Never shadow writes by default**; only `GET`/`HEAD` reads are eligible.
4. **Never replay a failed in-flight request against legacy** unless the route
   is explicitly `failover_safe: true` (idempotent). Routing *subsequent*
   requests to legacy via the circuit breaker is fine; retrying *the same*
   request that may already have hit `new` is not.
5. **Never log secret values.** Redaction (headers, JSON paths, query params)
   applies to every output surface — logs and diffs alike.
6. **Bound all buffers**; over-limit bodies fall back to streaming with
   comparison skipped.
7. **Validate config and contracts at startup**; refuse to start on invalid
   input.

## Behavioral contract vs. operational config

The shared contract owns *what to compare and how* (`ignore_paths`,
`redact_paths`, `sort_arrays`, `unordered_arrays`, `normalize_timestamps`,
`enum_aliases`, `compare_*`). Limen route config owns *whether/how often/how
much* (`enabled`, `sample_rate`, `max_body_bytes`) plus all routing, rollout,
timeout, breaker, and flag concerns. The namespaces are disjoint, so merging is
a union — never a reconciliation. A route may reference a contract **or** inline
behavioral rules, never both (validation error).

## Conventions

- Use the supported JSONPath subset only: `$.field`, `$.nested.field`,
  `$.items[*].field`. Anything else is a load-time validation error and must
  stay in lockstep with Pharos.
- Typed errors (`thiserror`) where they cross boundaries; `anyhow` at the
  binary top level.
- Avoid high-cardinality metric labels (no user/tenant/request IDs or raw
  paths) — use route IDs and path templates.
- Match the surrounding code's comment density and idiom; comments explain
  *why*, not *what*.

## Commits

The build proceeds phase by phase (spec Section 14). Each phase: implement →
quality gate green → `/simplify` → `/codex:rescue` review → quality gate green
→ commit to `main`. Keep commits scoped to a phase or a coherent slice of one.
