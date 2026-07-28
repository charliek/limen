# Architecture

Limen is a single binary crate split into a library (`src/lib.rs`) and a thin
binary (`src/main.rs`). Keeping the logic in the library makes the proxy
testable without binding sockets or driving real upstreams — `main.rs` only
parses the CLI, initializes logging, and dispatches.

## Two planes

Limen runs **two listeners** (spec §3.2):

| Plane | Address | Serves |
|---|---|---|
| **Data plane** | `server.listen_addr` (e.g. `:8080`) | proxied client traffic |
| **Control plane** | `metrics.listen_addr` (e.g. `:9090`) | `/metrics`, `/health/live`, `/health/ready` |

The control plane never serves proxied traffic and is bound separately so it can
be firewalled off from public exposure.

## Two body paths

Body handling is the central performance decision (spec §3.3). There are two
deliberately separate paths:

- **Streaming path (default).** When comparison is disabled for a route, or a
  request isn't selected for comparison sampling, request and response bodies are
  streamed between client and upstream without full buffering. Limen observes
  only status, headers, and latency. Lowest overhead; unbounded body size is
  fine. Most production traffic takes this path.
- **Buffer-for-compare path.** Used only when comparison is enabled **and** the
  request is sampled **and** the body is within `max_body_bytes`. The relevant
  responses are buffered, normalized, hashed, and (on a hash mismatch) diffed.
  Over the limit, comparison is skipped (`response_too_large`) and the primary
  response still streams to the client. The *request* body is buffered under the
  same bound only for a write the route opted into shadowing, so both upstreams
  receive identical bytes; over the limit, shadowing is skipped
  (`request_too_large`) and the request streams to the primary unchanged.

The sampling decision is made **per request, before buffering**, so a route with
`sample_rate: 0.1` pays the buffering cost on ~10% of traffic and streams the
rest.

## Request lifecycle (data plane)

For each incoming request (spec §3.4):

1. **Match route** by method + path (longest path-prefix wins).
2. **Resolve route mode** and, for `percentage_split`, the rollout percentage
   from the flag provider.
3. **Decide the primary upstream** (legacy or new) given mode + rollout +
   circuit-breaker state.
4. **Decide shadow eligibility.**
5. **Dispatch:** send the primary request to the chosen upstream; if shadowing,
   dispatch the shadow request fire-and-forget.
6. **Return the primary response** to the client as soon as it is available.
7. **Compare** off the client path: if eligible and sampled, normalize both
   responses, hash, compare, optionally diff, emit metrics/logs.
8. **Record metrics** regardless of comparison.

The shadow path and comparison **never** delay or fail the client response.

## Route modes

Each route declares exactly one of five modes (spec §6):

| Mode | Primary | Behavior |
|---|---|---|
| `legacy_only` | legacy | legacy serves everything; no new traffic. |
| `new_only` | new | new serves everything (post-cutover, or no legacy equivalent). |
| `shadow_legacy_primary` | legacy | legacy serves the client; eligible reads are shadowed to new and compared. |
| `percentage_split` | legacy/new | deterministic per-key split by rollout percentage; breaker/fail-safe can override toward legacy. |
| `failover_to_legacy` | new | new is primary; fall back to legacy on failure — **only retrying the in-flight request when `failover_safe: true`**. |

**Shadow eligibility** (all must hold): method is `GET`/`HEAD` — or a write the
route opted into `comparison.shadow_methods`; comparison is enabled; the body is
within the buffer limit; shadow concurrency isn't exceeded; shutdown isn't in
progress. Writes are never shadowed by default; an opted-in write replays a
bounded, buffered body to both upstreams.

!!! warning "Failover and idempotency"
    *Routing* the next request to legacy because the circuit is open is always
    safe — no request runs twice. *Retrying an in-flight request* that already
    hit `new` is only safe when the operation is idempotent. `failover_safe`
    governs the second, dangerous case; the circuit breaker governs the first.

## Module map

The module layout mirrors spec §3.5. Submodules are added in the phase that
implements them rather than created empty up front.

| Module | Responsibility |
|---|---|
| `config` | Operational config model, layered loading, semantic validation. |
| `contract` | The shared behavioral contract: model, loading, merge. |
| `http` | Data-plane server, upstream client, streaming proxy core, bounded buffers. |
| `routing` | Route matching, upstream decisioning, rollout hashing. |
| `compare` | Normalization, JSONPath subset, blake3 hashing, diffing, redaction. |
| `flags` | The `FlagProvider` trait and static / file / Redis providers. |
| `resilience` | Circuit breaker, timeouts, shadow concurrency limiting. |
| `observability` | Metrics, structured logging, request-id propagation. |
| `health` | `/health/live`, `/health/ready`, and readiness evaluation. |

## Technology

`tokio` + `axum`/`tower` on `hyper` for the server; `reqwest` (with rustls) for
upstream calls; `serde` for config and contracts; `tracing` for logs; `metrics`
+ `metrics-exporter-prometheus` for metrics; `blake3` for normalized-response
hashing. The proxying core sits behind clear module boundaries so it could later
be re-hosted on a higher-throughput data plane (e.g. Pingora) without rewriting
the comparison, flags, or rollout logic.
