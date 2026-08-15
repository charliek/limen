# Limen — Legacy-to-New Service Migration Proxy

**A production-grade Rust reverse proxy for safely migrating HTTP traffic from a legacy service to a new implementation, through shadowing, response comparison, deterministic percentage rollout, and fail-safe fallback.**

---

## 0. About This Document

This is the implementation specification for **Limen**, intended to be handed to a coding agent (Claude Code, Codex, etc.) and to human engineers. It defines the goals, scope, architecture, configuration model, phased build plan, acceptance criteria, and test plan in enough detail to implement the project from a clean checkout with high test coverage and confidence.

Limen is one of two complementary projects:

- **Limen** (this project, Rust): the runtime migration proxy. Handles live traffic routing, read-path shadowing, response comparison, gradual rollout, and safe fallback in development, staging, and production.
- **Pharos** (separate project, TypeScript/Vitest): a black-box functional test suite that validates a new service against a legacy service deterministically, before and during rollout.

The two share a **behavioral contract** (Section 4) but are independently deployable and have no build-time dependency on each other. See Section 13 for the relationship.

> **Naming note:** *Limen* is the Latin word for "threshold" — the liminal state in which the old and new implementations coexist and traffic crosses safely from one to the other, with the ability to step back. This directionality is deliberate: unlike a one-way crossing, every Limen route can fail back to legacy.

---

## 1. Goals and Non-Goals

### 1.1 Primary goals

1. **Safe traffic migration.** Sit in front of two upstreams — `legacy` (current source of truth) and `new` (the replacement) — and move traffic between them without changing user-facing behavior.
2. **Risk-free validation in production.** Shadow eligible read traffic to the new service, compare responses against legacy, and emit correctness signals **without ever letting the new service or comparison affect the client response**.
3. **Deterministic, controllable rollout.** Shift traffic from legacy to new by percentage, deterministically per assignment key, controllable at runtime via feature flags without redeploying.
4. **Fail safe under uncertainty.** Default to legacy whenever anything is wrong — new upstream unhealthy, circuit open, flags stale, config ambiguous.
5. **Production-grade performance and observability.** Sub-millisecond proxying overhead on the streaming path, bounded resource use, Prometheus metrics, structured logs, health endpoints, and graceful shutdown.
6. **Consume the shared behavioral contract.** Apply the same normalization and comparison vocabulary that Pharos validates, so the rules refined during functional testing drive production shadow comparison unchanged.

### 1.2 Non-goals (MVP)

- Unit/integration testing of either upstream's internals (that is Pharos's and the services' own job).
- TLS **termination** at the proxy (the MVP makes TLS calls *to* upstreams; terminating client TLS is a documented post-MVP expansion — see Section 11.4).
- Long-term analytics, dashboards, or historical trend storage.
- Stateful load testing or large-scale performance testing.
- Protocols beyond HTTP/1.1 and HTTP/2 over TCP. No gRPC, no WebSockets, no GraphQL-specific handling in MVP.
- Dual-writing or reconciling production *data*. Limen shadows reads (and, only where a route explicitly opts a method in, replays a write to the new upstream for comparison); it never reconciles data between the two implementations.
- Hot-reloading of behavioral comparison rules mid-run (flag *values* hot-reload; comparison *semantics* are fixed for the duration of a run — see Section 4.4).
- A web UI. `limen report --format html` is not one: it renders a single self-contained static page from artifacts that already exist on disk — no server, no JavaScript, no external references, nothing live — so it is a report artifact in a second format, not a dashboard and not a UI.

### 1.3 Assumed migration pattern

Limen is designed for the common, lowest-risk migration shape: **legacy and new share the same backing datastore**, and the migration is a **re-implementation of request-handling logic** (e.g. a framework or language change), not a data migration. A write through either implementation is immediately visible to the other, so correctness reduces to **behavioral parity over shared data** — exactly what the shared contract (Section 4) expresses.

This assumption is why shadowing reads is safe (both read the same data) and why writes route to exactly one implementation unless a route explicitly opts them into shadowing (Section 6.1) — an opt-in that only makes sense once the operator has affirmed the endpoint tolerates being handled twice. Migrations that do **not** share a datastore (separate stores requiring synchronization) move into dual-write/reconciliation territory, which is explicitly out of scope; Limen's safety properties are not designed for that case.

---

## 2. Personas and Core Use Cases

### 2.1 Personas

- **Migrating engineer:** runs Limen locally (often in Docker Compose) against a legacy and an in-progress new service, watches shadow diffs, fixes the new service, repeats.
- **Service owner / SRE:** operates Limen in staging/production, controls rollout percentage via flags, watches correctness and circuit-breaker metrics, owns the fail-safe posture.
- **Platform engineer:** maintains Limen itself, its CI, config conventions, and the shared-contract integration.
- **AI coding agent:** drafts behavioral contracts from service docs/traffic, and (in the broader workflow) implements new-service behavior guided by Pharos failures. Limen consumes the resulting contract.

### 2.2 Core use cases

1. **Local development proxy.** Engineer runs Limen in front of legacy + new in Docker Compose; route is in `shadow_legacy_primary`; engineer iterates on the new service until shadow diffs are clean.
2. **Production shadow observation.** Limen runs at the edge or as a sidecar, returns legacy to clients, shadows reads to new, and reports a per-route mismatch rate to decide rollout readiness.
3. **Gradual rollout.** Once mismatch rate is acceptable, the route moves to `percentage_split`; operators raise the rollout flag (0 → 1 → 5 → 25 → 100) at runtime; deterministic hashing keeps a given tenant/user stable.
4. **Fail-safe under incident.** New upstream degrades; circuit breaker opens; traffic returns to legacy automatically; metrics and logs make the transition observable.
5. **Cutover and regression watch.** Route reaches 100% new; Limen can remain in `failover_to_legacy` to keep legacy as a safety net, or in `new_only` once legacy is decommissioned.

---

## 3. Architecture

### 3.1 Technology choice

**Primary stack (recommended and assumed by this spec):**

| Concern | Choice | Why |
|---|---|---|
| Async runtime | `tokio` | De facto standard; mature, performant. |
| HTTP server + middleware | `axum` + `tower` on `hyper` | `tower`'s `Service`/`Layer` model maps directly onto the proxy's middleware concerns (routing decision, rollout, shadow dispatch, circuit breaking, timeouts, metrics) as composable layers rather than ad-hoc code. `axum` gives ergonomic routing for the control-plane endpoints. `hyper` keeps the data plane low-level enough to control buffering and streaming. |
| Upstream HTTP client | `reqwest` | Connection pooling, timeouts, TLS, redirects handled. Pragmatic for both primary and shadow calls. A raw `hyper` client is the documented fallback if tighter control over shadow-path resource use is needed later. |
| Config / serde | `serde`, `serde_json`, `serde_yaml` | Config in YAML; contract in YAML or JSON. |
| Logging / tracing | `tracing`, `tracing-subscriber` | Structured logs, span context. |
| Metrics | `metrics` + `metrics-exporter-prometheus` | Prometheus endpoint, low-cardinality. |
| CLI | `clap` | Subcommands and config validation. |
| Errors | `thiserror` (library errors) + `anyhow` (binary/top-level) | Typed errors where they cross boundaries; ergonomic propagation at the top. |
| Hashing | `blake3` | Fast, stable hash for normalized-response comparison. |
| Redis client | `redis` (with `tokio` features) or `fred` | For the Redis flag provider. Pick one; `fred` has stronger async ergonomics and connection management, `redis` is more ubiquitous. Either is acceptable; document the choice. |
| Test HTTP servers | `wiremock` and/or lightweight `axum` test servers | Integration tests. |

**Documented fallbacks (only if the primary stack proves insufficient during implementation):**

- **Pure `hyper` (no `axum`)** for the data plane if `axum`'s extractors get in the way of streaming control. More boilerplate, more control.
- **Raw `hyper` upstream client** instead of `reqwest` if the shadow path needs finer resource control.

**Pingora sidebar (read before reaching for it):**

> Pingora (Cloudflare's Rust proxy framework, open-sourced 2024) is purpose-built for very-high-volume edge proxying and would be a strong fit for the standalone-edge deployment at large scale. This MVP deliberately chooses `axum`/`tower`/`hyper` because: (1) the `tower` `Service`/`Layer` model maps cleanly onto our middleware concerns and is widely understood; (2) the ecosystem and documentation surface is larger, which matters for a codebase that AI agents and multiple engineers will extend; (3) Pingora is more opinionated and would shape the entire architecture around itself. **Migration path:** because routing, comparison, flags, and resilience all sit behind traits and the data plane is isolated, the proxying core could later be re-hosted on Pingora for the edge deployment **without rewriting** the comparison, flags, or rollout logic. Revisit Pingora only if single-instance edge throughput becomes the binding constraint.

### 3.2 Data plane vs. control plane

Limen runs **two listeners**:

- **Data plane** (`server.listen_addr`, e.g. `:8080`): the actual proxy. Receives client traffic, matches routes, decides upstream, proxies, optionally shadows and compares.
- **Control plane** (`metrics.listen_addr`, e.g. `:9090`): `/metrics`, `/health/live`, `/health/ready`. Never serves proxied traffic. Bound separately so it can be firewalled off from public exposure.

### 3.3 Two body-handling paths (performance-critical)

This is a central design decision. Limen has **two deliberately separate code paths** for handling bodies:

1. **Streaming path (default).** Used when a route has comparison disabled, or when a request is not selected for comparison sampling. Request and response bodies are **streamed** between client and upstream without full buffering. Limen observes only status, headers, and latency. Lowest overhead; unbounded body size is fine. This is the path most production traffic should take.

2. **Buffer-for-compare path.** Used only when comparison is enabled for the route **and** this request is selected by sampling **and** the body is within `max_body_bytes`. Both relevant responses are buffered, normalized, hashed, and (if hashes differ and sampling selected this request for detailed diffing) diffed. Bounded by `max_body_bytes`; over the limit → comparison is skipped with reason `response_too_large`, and the primary response is still streamed to the client. It is bounded in **time** by the same `primary_ms` budget as the send that preceded it — one absolute per-request deadline covering send-to-headers *and* this buffering — so on expiry the response demotes to streaming with comparison skipped (`response_buffer_timeout`) rather than holding the client's first byte for a body that trickles. A `text/event-stream` response skips comparison eagerly (`event_stream`) before a byte is buffered, since an event stream never completes and buffering one could only ever end at that deadline. The *request* body is buffered under the same bound only for a write the route opted into shadowing (Section 6.1), so the shadow can replay identical bytes; over that limit → shadowing is skipped with reason `request_too_large` and the request body streams to the primary unchanged.

The sampling decision is made **per request**, before buffering, so that on a route with `sample_rate: 0.1` you pay buffering cost on ~10% of traffic and stream the other ~90%.

### 3.4 Request lifecycle (data plane)

For each incoming client request:

1. **Match route** by method + path, narrowed by any query conditions the route declares (longest path-prefix wins, then a query-conditioned route over an unconditioned one; see 5.2). No match → configured not-found response.
2. **Resolve route mode** and, for `percentage_split`, resolve the rollout percentage from the flag provider.
3. **Decide primary upstream** (legacy or new) per mode + rollout + circuit-breaker state.
4. **Decide shadow eligibility** (see 6.1).
5. **Dispatch:**
   - Send the **primary** request to the chosen upstream (streamed unless this request will be compared and buffered).
   - If shadowing, dispatch the **shadow** request fire-and-forget; it must **never** delay or fail the client response.
6. **Return the primary response to the client** as soon as it is available.
7. **Comparison (off the client path):** if eligible and sampled, normalize both responses, hash, compare, optionally diff, emit metrics/logs/sampled diff.
8. **Record metrics** for the request regardless of comparison.

Circuit-breaker state is updated based on primary-upstream outcomes (and shadow outcomes where configured) per Section 9.

### 3.5 Module structure

```text
limen/
  Cargo.toml
  README.md
  LICENSE
  config/
    limen.example.yaml
    flags.example.yaml
    contracts/
      example-service.contract.yaml
  src/
    main.rs                 # bootstrap, signal handling, listener wiring
    cli.rs                  # clap subcommands: run, validate-config, print-routes,
                            #   check-contract, report, verdict, suggest-routes
    error.rs                # top-level error types
    verdict.rs              # `limen verdict`: drain, floors, sink integrity, canary (§12.1)
    suggest.rs              # observe-profile → per-route classification
    draft.rs                # `limen suggest-routes`: draft config emission
    report_html.rs          # `limen report --format html`: fail-closed status page (§10.4)
    config/
      mod.rs
      model.rs              # serde structs for limen.config.yaml
      load.rs               # load + layer (defaults < file < env < CLI)
      validate.rs           # semantic validation (URLs, percentages, timeouts, refs)
    contract/
      mod.rs
      model.rs              # serde structs for the shared contract (YAML/JSON)
      load.rs               # load contract files, resolve `path#routeId` references
      merge.rs              # merge contract comparison rules with route operational config
    http/
      mod.rs
      server.rs             # data-plane listener (hyper/axum)
      client.rs             # upstream client (reqwest), TLS, timeouts, pooling
      proxy.rs              # streaming proxy core
      body.rs               # bounded buffering helpers, body-limit enforcement
      forwarded.rs          # X-Forwarded-For/Proto + X-Limen-Shadow injection (§3.6)
      shadow.rs             # shadow eligibility + fire-and-forget dispatch
    routing/
      mod.rs
      matcher.rs            # method + longest-prefix matching
      decision.rs           # mode + rollout + circuit-breaker → upstream choice
      rollout.rs            # deterministic hashing, bucket assignment
    flags/
      mod.rs
      provider.rs           # FlagProvider trait
      static_provider.rs
      file_provider.rs      # polling refresh, last-known-good, staleness
      redis_provider.rs     # polling/pub-sub refresh, last-known-good, staleness
      health.rs             # provider health + staleness tracking
    compare/
      mod.rs
      normalize.rs          # JSON normalization (sort keys, ignore/redact paths, sort arrays, timestamps, enums)
      jsonpath.rs           # the supported JSONPath subset
      hash.rs               # blake3 over normalized representation
      diff.rs               # JSON-aware structural diff, bounded + redacted
      redact.rs             # header + JSON-path + query redaction
      result.rs             # ComparisonResult, Mismatch types
      headers.rs            # set_cookie/location comparison dimensions (§4.2)
    observability/
      mod.rs
      metrics.rs            # observer traits + metric event vocabulary
      prometheus.rs         # metric definitions, labels, exposition rendering
      logging.rs            # tracing setup, structured fields
      request_id.rs         # request/trace id extraction + propagation
      observe.rs            # observe mode: passive per-route traffic profiling
      sink.rs               # durable mismatch diff sink + `limen report` (§10.4)
    resilience/
      mod.rs
      circuit_breaker.rs    # per-route, per-upstream breaker state machine
      timeouts.rs           # primary/shadow timeout layers
      concurrency.rs        # shadow concurrency limiting
    health/
      mod.rs
      endpoints.rs          # /health/live, /health/ready
      readiness.rs          # readiness evaluation (config valid, providers usable/fail-safe)
  tests/
    integration/
      legacy_only.rs
      new_only.rs
      shadow_match.rs
      shadow_mismatch.rs
      shadow_timeout.rs
      percentage_rollout.rs
      circuit_breaker.rs
      flag_reload.rs
      stale_flag_failsafe.rs
      graceful_shutdown.rs
    common/
      mod.rs                # test server helpers, fixtures
  benches/
    proxy_overhead.rs       # criterion benchmarks for SLO validation
  examples/
    docker-compose.yaml     # legacy + new mock + limen, for local trial
```

### 3.6 Forwarded headers

Limen sets three headers on every upstream request (`http/forwarded.rs`; injected in `http/proxy.rs::dispatch` before the primary send and before the shadow is planned, so both carry the same values):

- **`X-Forwarded-For`**: the client's address is appended to any existing value, never replacing it — standard proxy semantics, matching a fronting load balancer or CDN. If the incoming request already carries the header as more than one field line, every line is preserved and combined (in order) with the client's address into one comma-joined output line — no hop in the chain is dropped. Set on **both** the primary and shadow requests. Limen learns the client's address from the accepted TCP connection (`axum`'s `ConnectInfo`); if that context is unavailable — e.g. the proxy embedded and driven directly against its router rather than through a bound listener — the header is **omitted entirely** rather than sent with a fabricated value. `X-Forwarded-Proto` is unaffected by this; it never depends on the client address. The value is the bare client IP (no port; an IPv6 address is rendered without brackets, unlike a URI authority).
- **`X-Forwarded-Proto`**: set to `http` — the scheme of Limen's own data-plane listener, which is plain HTTP in the MVP (Section 11.4; TLS, if any, terminates in front of Limen, and `upstream_tls` config governs calls *to* upstreams, not this listener) — but **only when the client's request doesn't already carry the header**. A value already present came from a proxy upstream of Limen (e.g. a TLS-terminating load balancer) and is authoritative; Limen never overwrites it. Set on **both** the primary and shadow requests.
- **`X-Limen-Shadow: 1`**: set **only** on the shadow request (never the primary), so an upstream — or its access logs — can distinguish Limen's fire-and-forget comparison traffic from real client traffic (Section 6.1). A client-supplied `X-Limen-Shadow` on an incoming request is **unconditionally stripped** before either the primary or the shadow request is built — a client must never be able to spoof shadow status on a request that actually hits the real upstream as primary traffic.

None of the three is hop-by-hop, so the header-copy step that strips `Connection`-listed and framing headers (Section 3.4, step 5) leaves `X-Forwarded-For`/`X-Forwarded-Proto` untouched on the request leg. That same step, however, explicitly strips all three of `X-Forwarded-For`, `X-Forwarded-Proto`, and `X-Limen-Shadow` on the **response** leg — they are only ever written onto the *outbound-to-upstream* request headers, and an upstream that happens to reflect request headers back must not be able to leak them onto the client-facing response.

`X-Forwarded-Host` is deliberately **not** set — upstreams are expected to pin their own base URL rather than trust a forwarded host.

---

## 4. The Shared Behavioral Contract

### 4.1 Purpose and lifecycle

The behavioral contract is the artifact that flows through the migration workflow and gets more trustworthy at each stage:

```text
   AI investigation                Pharos                       Limen
 (docs, OpenAPI, traffic,   (deterministic functional      (production shadow
  code, logs)                validation + refinement)        comparison + rollout)
        │                            │                              │
        ▼                            ▼                              ▼
   DRAFTS the contract  ──▶  VALIDATES & REFINES it  ──▶  CONSUMES it unchanged
   (ignore/redact paths,     (catches over-normalization,  (same normalization
    timestamp & enum          missed real diffs,             vocabulary applied to
    normalizations,           tightens rules)                live shadow traffic)
    expectations)
```

The contract describes **observable behavior and how to compare it** — never service internals. It is framework-agnostic by design, because the migration pairs vary (e.g. Ratpack→Spring Boot, Python→Rust, TypeScript→Go). It is the **single source of truth for comparison semantics** shared between Pharos and Limen.

### 4.2 Contract format

A contract file is YAML or JSON (Limen detects by extension; AI tooling tends to emit JSON, humans review YAML — both parse to the same structure).

```yaml
# config/contracts/device-service.contract.yaml
version: 1
service: device-service
description: >
  Behavioral contract for device-service migration. Drafted from OpenAPI +
  captured traffic, refined by Pharos runs, consumed by Limen for shadow comparison.

# Service-wide defaults. Per-route `comparison` blocks merge on top of these.
defaults:
  compare_status: true
  compare_body: true
  compare_headers: []          # header names compared only if listed
  json:
    ignore_paths:
      - "$.metadata.requestId"
      - "$.metadata.generatedAt"
    redact_paths:
      - "$.user.email"
      - "$.token"
    sort_arrays:
      - path: "$.devices"
        key: "id"
    unordered_arrays:
      - path: "$.permissions"
    normalize_timestamps:
      - path: "$.createdAt"
        precision: seconds       # e.g. legacy emits millis, new emits seconds
    enum_aliases:
      - path: "$.status"
        aliases: { ACTIVE: enabled, INACTIVE: disabled }
  set_cookie:                    # optional; omitted = not compared (see below)
    compare: true
    ignore_cookies: []
    ignore_attributes: []
    compare_values: exact        # exact | presence
  location:                      # optional; omitted = not compared (see below)
    compare: true
    ignore_query_params: []
    origin: exact                # exact | ignore

routes:
  - id: "get-device"
    match:
      methods: ["GET"]
      path_template: "/devices/{id}"
    comparison:
      json:
        ignore_paths: ["$.device.lastSeenAt"]   # merged with defaults
    expectations:
      typical_status: 200
      notes: >
        Legacy returns 200 with empty body on soft-deleted devices; new returns
        404. This is an intentional change — see the intentional-change tag.
    tags: [read, migration-ready]

  - id: "list-devices"
    match:
      methods: ["GET"]
      path_template: "/devices"
    comparison:
      json:
        sort_arrays:
          - path: "$.devices"
            key: "id"
    tags: [read, migration-ready]
```

#### Timestamp precision spellings

`normalize_timestamps[].precision` accepts both `milliseconds` and `millis`
(Limen's historical spelling) in **both** tools — a deliberate lockstep
accommodation so one contract file parses in either. The two spellings resolve
to the same precision.

Write `milliseconds` in authored contracts by convention; it is the spelling
Pharos's documentation uses, so a hand-written or AI-drafted contract reads the
same on both sides. Limen's own serializer emits `millis` — an implementation
detail rather than a canonicalization, and harmless because Pharos accepts that
spelling too, so a Limen-generated contract is still valid input to both tools.

The other values are `seconds`, `minutes`, `hours`, `days`.

#### `set_cookie` and `location` comparison

Two additional, **optional** comparison dimensions, read from every
`Set-Cookie` response header and from the `Location` response header — not
from the single-value header map that `compare_headers` uses. Both are legal
at the `defaults` and per-route `comparison` levels, exactly like `json`; both
are omitted from the example routes above purely for brevity. Omitted at every
layer, the dimension is **not compared at all** (today's behavior).

**`set_cookie` semantics:** each side's `Set-Cookie` values are parsed into
`(name, value, attribute map)` tuples. Cookies are paired across sides **by
name**; duplicate names on one side pair **positionally** within the name
group. Attribute names are compared case-insensitively; attribute values are
compared exactly, except for attributes listed in `ignore_attributes`. Cookies
named in `ignore_cookies` are excluded entirely. A cookie present on one side
only is a mismatch. `compare_values: presence` compares only that a value
exists on both sides (plus the attribute map) without comparing the value
itself; `exact` also compares the value.

**`location` semantics:** the `Location` header is parsed as a URL on both
sides. A **relative** `Location` value is resolved against the URL of the
request that produced the response (RFC 9110 §10.2.2) before any part-wise
comparison, so a legacy `/next?x=1` and a new `https://new.example/next?x=1`
compare as the same target when each is resolved against its own request URL.
Query params named in `ignore_query_params` are removed from both sides before
comparing. `origin: exact` compares scheme+host+port as well as path and
remaining query; `origin: ignore` compares only path and remaining query — for
cases where legacy and new intentionally redirect to different hosts for the
same logical destination.

**Both:** a value that cannot be parsed — a malformed Set-Cookie, or a
`Location` whose resolution against the request URL fails — falls back to
**exact string comparison** and counts as a mismatch if the sides differ. A
`Location` that resolves successfully is always compared part-wise, never as a
raw string. Redaction (Section 7.5) still applies to rendered values — a
`set_cookie` mismatch never renders a raw cookie value (name and attribute diff
only), per the no-secret-value invariant.

**Comparison details (normative, lockstep).** Both engines resolved these while
implementing the dimensions; they are as binding as the field names:

- **Case sensitivity.** Cookie names — and therefore `ignore_cookies` — are
  compared **case-sensitively** (RFC 6265). Cookie *attribute* names — and
  therefore `ignore_attributes` — are compared **ASCII-case-insensitively**;
  attribute *values* are compared exactly. Query parameter names — and therefore
  `ignore_query_params` — are compared case-sensitively.
- **Malformed Set-Cookie** means the name/value pair has no `=`, or the name is
  empty (the values RFC 6265 §5.2 discards). Unparseable entries are paired with
  each other **positionally**, never with parsed cookies, and take the
  exact-string fallback. A duplicated attribute inside one `Set-Cookie` keeps its
  **last** occurrence, as RFC 6265 §5.2 prescribes.
- **`compare_values: presence`** compares only whether the two sides *agree*
  that a value exists: an empty value counts as no value, so `sid=` against
  `sid=abc` is a value mismatch, while `sid=` on **both** sides matches — that
  is the cookie-deletion shape (`session=; Max-Age=0`) legacy and new both emit
  on logout, and it is agreement, not a failure.
- **Location query.** After `ignore_query_params` removal, the remaining query is
  compared as a `name -> values` map, so parameter **order never matters**;
  repeated names compare as an ordered list of values.
- **Location parts.** `origin: exact` compares the `(scheme, host, effective
  port)` triple and nothing more — *effective* port, so `https://a` and
  `https://a:443` are one origin. It is computed from those three parts
  explicitly rather than from a URL library's `origin` accessor: those return an
  opaque, never-equal origin for non-special schemes (and disagree between Rust
  and JavaScript), which would make two identical `mailto:` Locations mismatch.
  Neither mode compares the URL **fragment** or **userinfo**, which are outside
  the enumerated parts.
- **Rendering.** A cookie value is never rendered — a value difference shows
  `<redacted>` (`<empty>` when the value is empty), a one-sided cookie shows
  `<present>`, and an unparseable entry shows `<redacted>` because it cannot be
  masked selectively. Attribute values, `Location` origins, and paths are
  rendered verbatim; `Location` query values are masked for the standard
  secret-bearing parameter names (Section 7.5) — which include the OAuth
  authorization `code`. A rendered `Location` is origin + path only, so a
  `user:password@` userinfo is never emitted, and an unresolvable `Location`
  renders `<redacted>` for the same reason as an unparseable cookie.
- **Bounds.** The cookie and `Location` mismatch lists are each capped at the
  same `max_differences` bound as the body diff, and the result's
  `diff_truncated` flag covers all three surfaces — no single response can grow
  an unbounded log line.

**`compare_headers` conflict:** because these are separate dimensions rather
than `compare_headers` entries, naming them in a `compare_headers` list is a
**load-time validation error** — asymmetrically, because the two headers differ
on the wire. Listing `set-cookie` (in any case) is *always* an error, block or
no block: `compare_headers` reads the single-value header map, so a response
carrying several `Set-Cookie` headers would be compared on one value with the
rest silently dropped; the `set_cookie` block is the only way to compare
cookies. Listing `location` is an error only while a `location` block is
present anywhere in a route's resolved rules — `Location` is genuinely
single-valued, so the generic path compares it faithfully and only the
duplicated intent is ambiguous; there the block wins conceptually.

**Lockstep:** this vocabulary — field names, parsing, merge (Section 4.4), and
validation semantics — must remain **identical** between Limen and Pharos, the
same obligation as the JSONPath subset (Section 7.4). The shared fixture in
`tests/lockstep/` (a byte-identical twin of the Pharos copy) plus its
`decisions.json` table pin the resolution rules in both engines: `merge_cases`
pins contract resolution and `verdict_cases` pins the comparison itself (a
response pair and its rules resolve to one verdict plus a **set** of mismatch
kinds — `status`, `body`, `header`,
`set_cookie.presence|value|attribute|malformed`,
`location.presence|origin|path|query|raw`). The set is deliberately
order-independent: the engines must agree on *which* mismatches exist, not on
the order in which they find them.

### 4.3 What lives in the contract vs. in Limen config

This split is the rule that keeps the two artifacts from ever conflicting — they occupy **distinct key namespaces**:

| Concern | Lives in | Rationale |
|---|---|---|
| **What** to compare and **how** (`ignore_paths`, `redact_paths`, `sort_arrays`, `unordered_arrays`, `normalize_timestamps`, `enum_aliases`, `compare_status`, `compare_body`, `compare_headers`, `set_cookie`, `location`) | **Contract** | Behavioral truth, shared with and refined by Pharos. |
| **Whether / how often / how much** to compare operationally (`enabled`, `sample_rate`, `max_body_bytes`) | **Limen route config** | Runtime cost/volume policy, deployment-specific. |
| Routing, upstreams, rollout, timeouts, circuit breaker, flags, server | **Limen route config** | Pure operational concern; Pharos has no equivalent. |

Because behavioral keys never appear in Limen route config and operational keys never appear in the contract, there is **no possible key-level conflict** — the merge (Section 4.4) is a union, not a reconciliation.

### 4.4 How Limen consumes the contract (MVP behavior — "Option B")

- Each Limen route **references** a contract route by file + fragment: `contract: "./contracts/device-service.contract.yaml#get-device"`.
- At **startup**, Limen loads the contract, resolves the reference, and **merges** the contract's behavioral comparison rules (service `defaults` + per-route `comparison`, with per-route overriding/merging onto defaults) with the route's operational comparison policy.
- The merged comparison configuration is what the comparison engine uses at runtime.
- **Contracts are loaded once at startup and are NOT hot-reloaded.** Only flag *values* hot-reload (Section 8). Comparison *semantics* stay fixed for the duration of a run, so a contract edit cannot change how in-flight comparisons behave. To pick up a contract change, restart (or trigger a controlled reload — a documented post-MVP enhancement).
- A route **without** a `contract` reference may specify its comparison rules inline using the **same vocabulary** (this keeps a single-file quickstart possible and is the documented fallback authoring style). If both a contract reference and an inline `comparison.json` behavioral block are present, that is a **validation error** — pick one source of behavioral truth per route.

### 4.5 `check-contract` CLI command

Limen ships a `check-contract` subcommand that validates a contract file against the schema and reports the supported-JSONPath compliance of every path it contains. This lets the AI→Pharos→Limen loop verify a freshly drafted contract is Limen-consumable before wiring it into a route.

---

## 5. Configuration

### 5.1 Sources and precedence

Config is layered, later sources overriding earlier:

1. Built-in defaults.
2. Config file (`--config ./limen.config.yaml`).
3. Environment variables.
4. CLI arguments.

Example environment variables:

```bash
LIMEN_LISTEN_ADDR=0.0.0.0:8080
LIMEN_METRICS_ADDR=0.0.0.0:9090
LIMEN_FLAGS_PROVIDER=redis
LIMEN_REDIS_URL=redis://localhost:6379
LIMEN_FAIL_SAFE_MODE=legacy_only
LIMEN_CONFIG=./limen.config.yaml
```

### 5.2 Config model

```yaml
# limen.config.yaml
server:
  listen_addr: "0.0.0.0:8080"
  graceful_shutdown_timeout_ms: 10000
  request_body_limit_bytes: 1048576       # hard cap on buffered request bodies

metrics:
  listen_addr: "0.0.0.0:9090"
  path: "/metrics"

upstream_tls:
  # applies to reqwest client used for upstream calls
  verify_certificates: true
  # optional custom CA bundle for internal PKI
  ca_bundle_path: null
  # optional per-upstream overrides may be added post-MVP

flags:
  provider: "redis"                         # static | file | redis
  static:
    values: {}                              # used when provider = static
  file:
    path: "./flags.local.yaml"
    refresh_interval_ms: 1000
  redis:
    url: "redis://localhost:6379"
    key_prefix: "limen:flags:"
    refresh_interval_ms: 1000
  stale_ttl_ms: 30000                       # after this, apply fail_safe_mode
  fail_safe_mode: "legacy_only"             # behavior when flags are stale/unavailable

# Optional durable mismatch sink (Section 10.4). Omit to keep mismatches in
# metrics and logs only.
diff_sink:
  dir: "./limen-diffs"                      # daily mismatches-<UTC date>.jsonl files

routes:
  - id: "get-device"
    match:
      methods: ["GET"]
      path_prefix: "/devices/"
      query_present: []                     # all of these must be in the query (default [] = no condition)
      query_absent: []                      # none of these may be in the query (default [] = no condition)
    legacy_upstream: "https://legacy-device.internal"
    new_upstream: "https://new-device.internal"
    mode: "shadow_legacy_primary"           # see Section 6
    contract: "./contracts/device-service.contract.yaml#get-device"
    failover_safe: false                      # may this route auto-failover? (see 6.5)
    rollout:
      percentage_flag: "migration.get-device.rollout_percentage"
      default_percentage: 0
      assignment_key:
        header: "x-tenant-id"
        fallback: "request_random"          # request_random (MVP) | ...
    timeouts:
      primary_ms: 2000
      shadow_ms: 2000
    comparison:
      enabled: true                         # operational gate
      sample_rate: 0.1                       # fraction of eligible requests to compare
      max_body_bytes: 262144                 # skip comparison above this
    circuit_breaker:
      enabled: true
      failure_rate_threshold: 0.25
      min_requests: 20
      open_duration_ms: 30000
      half_open_max_requests: 5
    # budget: forward-looking, see Section 12.1 — NOT enforced by MVP, documented
    # here so rollout go/no-go gates have a home. Emitted-metric thresholds the
    # operator checks manually today; candidate for enforcement in a later version.
    budget:
      max_new_p95_latency_ratio: 1.0          # new p95 / legacy p95 ceiling (1.0 = no worse)
      max_new_error_rate_ratio: 1.0           # new 5xx rate / legacy 5xx rate ceiling
      max_mismatch_rate: 0.001                # parity ceiling (fraction of compared requests)
```

**Shadowing a write.** `get-device` above only reads, so it never opts into
`comparison.shadow_methods`. Shadowing a write is a separate, explicit choice
(Section 6.1): the method must be listed in **both** `match.methods` and
`comparison.shadow_methods`, or validation rejects it as inert. A minimal
route opting `create-device` into write-shadowing:

```yaml
  - id: "create-device"
    match:
      methods: ["POST"]
      path_prefix: "/devices"
    legacy_upstream: "https://legacy-device.internal"
    new_upstream: "https://new-device.internal"
    mode: "shadow_legacy_primary"
    contract: "./contracts/device-service.contract.yaml#create-device"
    comparison:
      enabled: true
      shadow_methods: ["POST"]              # must also appear in match.methods above
```

**Query-aware matching.** A route's `match` may narrow beyond method + path with two optional presence conditions over the request's query parameters:

- `query_present: [name, …]` — the route matches only if **every** named parameter is present (AND semantics). Presence only: `?prompt=` counts exactly like `?prompt=login`.
- `query_absent: [name, …]` — the route matches only if **none** of the named parameters is present.

Both default to `[]`, and a route declaring neither behaves exactly as it did before the fields existed. Parameter names are compared after the same percent-decoding the comparison engine applies to query parameters, and values never participate — there are no value predicates and no regex. Decoding is one-directional: the *request's* names are decoded, the configured names are literals, so a route naming `login_verifier` matches a request spelling it `login%5Fverifier` but a config name written `login%5Fverifier` matches nothing — which is why validation refuses it (Section 5.3).

The motivating case is a path whose hops are not equally safe to shadow. Shadow-comparing `/oauth2/auth` works for the initial authorize bounce, but the `login_verifier` / `consent_verifier` hops replay one-time tokens, so the shadow's copy deterministically fails at the shared authorization server ("The consent verifier has already been used", recorded in slauth's dual-lens campaign v1). Splitting the path into a conditioned route that relays the verifier hops and an unconditioned one that keeps comparing the bounces recovers the comparison without touching the tokens.

**Precedence.** Among the routes whose method, path prefix, and query conditions all match:

1. Longest `path_prefix` wins — unchanged, and it outranks every query condition, so a longer unconditioned prefix still beats a shorter conditioned one.
2. At an **equal** prefix, a query-**conditioned** route (declaring either field) beats an unconditioned one. This is what lets a narrow exception sit alongside the general route for a path, in either config order.
3. Config order remains the final stable tiebreak.

Two conditioned routes that could both match one request are rejected at load time (Section 5.3), so this ordering never has to choose between them.

Local flags file:

```yaml
# flags.local.yaml
migration.get-device.rollout_percentage: 0
migration.get-device.shadow_enabled: true
```

### 5.3 Validation

`validate-config` performs **semantic** validation, not just parse:

- Upstream URLs are well-formed absolute URLs with a supported scheme (`http`/`https`).
- Rollout percentages are within 0–100.
- Timeouts are positive and sane.
- Route IDs are unique.
- Route modes are known enum values.
- `contract` references resolve to an existing file and route fragment.
- A route does not declare **both** a `contract` reference and an inline behavioral `comparison.json` block.
- All JSONPath expressions (in contract or inline) are within the supported subset (Section 7.4).
- `fail_safe_mode` is a valid mode.
- `match.query_present` / `match.query_absent` names are non-empty and unique within their field, and no name appears in **both** on one route (it could never match).
- Those names are **literal decoded names**: no `%`, no `+`, no leading or trailing whitespace. The request's query is percent-decoded before comparison and config names are not, so an encoded spelling could never match — and a condition that matches nothing fails *open*, letting the traffic it was meant to except fall through to a sibling route. Rejected at startup rather than normalized (safety invariant: refuse invalid config).
- Two query-conditioned routes sharing a `path_prefix` and at least one method must be **provably disjoint**: some parameter appears in one route's `query_present` and the other's `query_absent`, so no single request can satisfy both. The check is deliberately conservative — anything not provably disjoint (two `query_present` sets a request could carry together; a `query_present` / `query_absent` pair over unrelated names) fails validation, even where a cleverer analysis might prove it safe. Routes on different prefixes never need this: longest prefix still decides.
- A route in `failover_to_legacy` mode whose `match.methods` include non-idempotent methods (POST, and PATCH unless declared idempotent) **must** set `failover_safe: true` explicitly, or validation fails. This forces an operator to consciously affirm that auto-failover is safe for that route (Section 6.5).
- Validation refuses `shadow_methods` entries that could not take effect: a method other than `POST`, a mode that does not shadow, `comparison.enabled: false`, or a method `match.methods` does not carry (Section 6.1).
- `budget` ratios, if present, are positive numbers; `max_mismatch_rate` is within 0–1.
- `diff_sink.dir`, if the block is present, is non-empty. The directory (and its parent) need **not** exist — it is created on the first mismatch, so a fresh deploy is not failed for a directory nothing has written to yet.

Validation failures must name the offending field and route.

---

## 6. Route Modes

Limen implements five modes. Each route declares exactly one.

### 6.1 `shadow_legacy_primary`

- Primary request → **legacy**; legacy response returned to client.
- For **eligible** read requests, a shadow request → **new** (fire-and-forget).
- Compare legacy vs. new after normalization; emit metrics/logs/sampled diffs.
- **Shadow or comparison failure never affects the client response.**
- The shadow request carries `X-Limen-Shadow: 1` (Section 3.6), which the primary request never does.

**Shadow eligibility (all must hold):**

- Method is `GET` or `HEAD`, **or** a write method the route explicitly opted in via `comparison.shadow_methods` (below).
- Comparison is enabled for the route.
- Request body is absent (reads) or buffered within `max_body_bytes` (opted-in writes).
- Shadow concurrency limit not exceeded (if configured).
- Shutdown is not in progress.

**Writes are never shadowed by default; a route may opt in per method.**

Reads are replayed bodyless, so a body-bearing `GET`/`HEAD` is never shadowed — its body could not be reproduced faithfully. To shadow a write, a route lists the method in `comparison.shadow_methods` (only `POST` is supported today):

```yaml
comparison:
  enabled: true
  sample_rate: 0.1
  max_body_bytes: 262144
  shadow_methods: ["POST"]     # absent/empty (the default) = reads only
```

For such a request, the body is read **once, bounded by `max_body_bytes`**, and those exact bytes are sent to the primary and replayed to the shadow — identical payload and identical framing (a matching `Content-Length`; the client's own framing headers are hop-by-hop-stripped and re-derived). Only that bounded buffering is on the client path — the same cost the failover-safe path already pays; the shadow dispatch and comparison remain fire-and-forget. A body over the limit is **never fully buffered**: it streams to the primary untouched and shadowing is skipped entirely with reason `request_too_large`.

The buffering is also skipped up front when the shadow concurrency limit (Section 9.3) is *already* saturated — the shadow would be refused anyway, and shedding the preparation is the point of the limit under load. That pre-check is best-effort (the permit is still reserved authoritatively after the primary responds); a lost race costs at most one buffered body whose shadow is then refused, which is the behavior without the check.

Validation refuses `shadow_methods` that could not take effect: a method other than `POST`, a mode that does not shadow, `comparison.enabled: false`, or a method the route's `match.methods` does not even carry (Section 5.3).

The opt-in is deliberately per route and per method: shadowing a write sends a second, real request to the new upstream, so it is only safe where the operator has affirmed that handling it twice is acceptable (typically because the new implementation shares the legacy datastore, Section 2.3, and the endpoint is idempotent or the shadow's effects are inert).

### 6.2 `legacy_only`

- Request → legacy only. Return legacy response. No new traffic.

### 6.3 `new_only`

- Request → new only. Return new response. (Used post-cutover or for endpoints with no legacy equivalent.)

### 6.4 `percentage_split`

- Resolve rollout percentage from flags.
- Deterministically select legacy or new by hashing `route_id + ':' + assignment_key` into bucket 0–9999; if bucket `< percentage * 100`, choose new, else legacy.
- Missing assignment key → configured fallback (`request_random` in MVP).
- Optionally shadow the non-primary upstream if configured.
- Circuit-breaker / fail-safe can override the new selection toward legacy.

### 6.5 `failover_to_legacy`

- Prefer **new** as primary.
- Fall back to legacy when new fails, times out, or the circuit is open.
- **Non-idempotent writes are never auto-failed-over by default.** A request that may have already taken effect on the new service (a non-idempotent POST) must **not** be retried against legacy, because the side effect could be applied twice (e.g. a resource created on both). This is a load-bearing safety guarantee.
- Auto-failover for a route is gated by the route's **`failover_safe`** flag (Section 5.2):
  - `failover_safe: false` (default): on new-side failure, Limen returns the failure to the client rather than silently retrying on legacy. The circuit breaker may still *route subsequent* requests to legacy (that is a routing decision, not a retry of an in-flight request), but the **failed request itself is not replayed**.
  - `failover_safe: true`: the route's operations are affirmed idempotent (GET/HEAD/PUT/DELETE, or a write the team has made idempotent), and Limen may retry the failed request against legacy.
- Validation **requires** `failover_safe: true` to be set explicitly on any `failover_to_legacy` route whose methods include non-idempotent verbs (Section 5.3), so the safety decision is always conscious rather than defaulted.

> **Distinction that matters:** *routing* the next request to legacy because the circuit is open is always safe — no request is executed twice. *Retrying an in-flight request* that already hit new is only safe when the operation is idempotent. `failover_safe` governs the second, dangerous case; the circuit breaker governs the first, safe one.

---

## 7. Comparison Engine

### 7.1 Hybrid comparison strategy

1. Normalize legacy and new responses (Section 7.2).
2. Hash the normalized representations (`blake3`).
3. If hashes match → record a **match**, no diff generated.
4. If hashes differ **and** both bodies are JSON → generate a JSON-aware structural diff.
5. If bodies are non-JSON → record a **body mismatch** without structural diff.
6. Sample and **redact** detailed diffs before logging.

Default comparison dimensions: **HTTP status** and **normalized body**. Headers are compared **only** when explicitly listed in the contract's `compare_headers`. `Set-Cookie` and `Location` have their own optional dimensions, enabled by the contract's `set_cookie` / `location` blocks (Section 4.2).

### 7.2 Normalization (runs before hashing and diffing)

Supported transformations, all driven by the merged contract rules:

- Parse JSON to a value tree; **sort object keys canonically**.
- **Remove ignored JSON paths** (`ignore_paths`).
- **Redact configured JSON paths** for diff output (`redact_paths`).
- **Sort arrays** by a configured key (`sort_arrays`).
- **Treat configured arrays as unordered sets** (`unordered_arrays`).
- **Normalize timestamps** to a configured precision (`normalize_timestamps`; both `milliseconds` and `millis` spellings accepted — Section 4.2).
- **Map equivalent enum aliases** (`enum_aliases`).

Normalization must be deterministic and order-independent in its result.

### 7.3 Diff output

A mismatch log entry includes bounded, redacted differences:

```json
{
  "event": "limen.response_mismatch",
  "route_id": "get-device",
  "request_id": "...",
  "method": "GET",
  "path_template": "/devices/{id}",
  "primary_upstream": "legacy",
  "legacy_status": 200,
  "new_status": 200,
  "legacy_latency_ms": 23,
  "new_latency_ms": 31,
  "comparison": {
    "status_match": true,
    "body_hash_match": false,
    "diff_kind": "json",
    "differences": [
      { "path": "$.device.name", "legacy": "A", "new": "B" }
    ]
  }
}
```

Diff output must respect a **maximum difference count** and a **maximum value length**, and apply redaction **before** anything is logged.

### 7.4 Supported JSONPath subset (hard MVP boundary)

To keep normalization fast and predictable, Limen supports a **documented subset** of JSONPath, identical to what Pharos supports, so contracts are portable:

- `$.field`
- `$.nested.field`
- `$.items[*].field` (wildcard over array elements)

Anything outside this subset is a **validation error** at config/contract load time. The subset may be expanded later, in lockstep across Limen and Pharos.

### 7.5 Redaction scope

Redaction applies to **every** output surface: console logs, structured logs, JSON diff output, and (in the broader system) any artifact. Configurable targets:

- **Header names** (e.g. `authorization`, `cookie`, `x-api-key`).
- **JSON paths** (e.g. `$.token`, `$.password`, `$.user.email`).
- **Query parameters** (e.g. `access_token`).

No secret value may appear in any log or diff. A test explicitly proves this (Section 12).

---

## 8. Feature Flags

### 8.1 Provider trait

Flags sit behind a trait so providers are swappable and future providers (LaunchDarkly-style remote, etc.) drop in without touching routing:

```rust
#[async_trait]
pub trait FlagProvider: Send + Sync {
    /// Return the current value for a flag key, or None if unset.
    async fn get(&self, key: &str) -> Option<FlagValue>;
    /// Provider health, including last successful refresh time and staleness.
    fn health(&self) -> FlagProviderHealth;
}
```

### 8.2 MVP providers

- **Static** — values from config; never stale.
- **File** — polling refresh from a YAML file; keeps **last known good** on invalid update; tracks staleness.
- **Redis** — polling (and/or pub-sub) refresh from a Redis key space under `key_prefix`; keeps **last known good** on connection failure; tracks staleness. **Designed so a LaunchDarkly-style remote provider can be added later behind the same trait.**

### 8.3 Behavior

- Missing flag → default.
- Invalid/failed refresh → keep last known good; do **not** crash.
- Track last successful refresh time.
- If stale beyond `stale_ttl_ms` → apply `fail_safe_mode` (usually `legacy_only`).
- Emit provider-health and staleness metrics/logs.

---

## 9. Resilience

### 9.1 Circuit breaker

Per-route, per-(new-)upstream breaker:

- **Closed** by default.
- **Opens** when failure rate exceeds `failure_rate_threshold` after at least `min_requests`.
- While **open**, avoid the new upstream; route to legacy when fail-safe allows.
- After `open_duration_ms`, transition to **half-open**.
- **Half-open**: allow up to `half_open_max_requests` trial requests; success closes the circuit, failure reopens it.
- Failures = 5xx responses, connection failures, timeouts (configurable).
- Emit state-transition metrics and logs.

### 9.2 Timeouts

Per-route `primary_ms` and `shadow_ms`. The shadow timeout must **never** extend the client-visible latency — shadow work is off the client path.

### 9.3 Shadow concurrency limiting

A global and/or per-route limit on concurrent in-flight shadow requests. When exceeded, shadows are **skipped** (not queued unboundedly), incrementing `shadow_skipped{reason="concurrency_limit"}`. Protects the proxy and the new upstream under load.

### 9.4 Bounded buffers

All buffering (request bodies, comparison buffering) is bounded by configured limits. The proxy must never buffer unbounded data; over-limit bodies fall back to streaming with comparison skipped. Comparison buffering of the primary response is additionally bounded in **time**: it draws down the same absolute `primary_ms` budget as the send that preceded it (Section 3.3), and an expiry demotes to the same streaming fallback with reason `response_buffer_timeout` — a bound on size alone would still let a trickling body hold the client's first byte indefinitely.

---

## 10. Observability

### 10.1 Metrics (Prometheus)

Required metrics (avoid high-cardinality labels — **no** user IDs, tenant IDs, request IDs, or raw full paths in labels; use route IDs and path templates):

- Request count by route, method, primary upstream, status class.
- Request latency by route and upstream.
- Upstream error count by route and upstream.
- Timeout count by route and upstream.
- Shadow request count.
- Shadow skipped count by reason (`concurrency_limit`, `request_too_large`, …). A `request_too_large` body was never replayed to the new upstream, so no comparison is attempted — it is a shadow skip, like `concurrency_limit`.
- Comparison attempted count.
- Comparison match count.
- Comparison mismatch count.
- Comparison skipped count by reason (`response_too_large`, `event_stream`, `response_buffer_timeout`) — a shadow that was planned but whose comparison could not complete. A request the sampler never selected makes no shadow plan at all and is therefore counted in neither this series nor the shadow-skip one; comparison coverage follows from `sample_rate` and eligible request volume, not from a skip count.
- Diff sampled count.
- Circuit-breaker state by route and upstream.
- Feature-flag provider health.
- Feature-flag staleness age.
- Active in-flight request count.

### 10.2 Structured logs

Via `tracing`. Include: request/trace ID, route ID, route mode, primary upstream selected, rollout decision and assignment-key type, upstream status codes, latencies, comparison result, error reason when relevant. Propagate standard trace headers where practical. Redact per Section 7.5.

### 10.3 Health endpoints

- `/health/live` — process is running.
- `/health/ready` — config is valid **and** required providers are usable or in a safe fallback mode. Readiness should degrade (not just hard-fail) when a provider is stale-but-within-fail-safe.

### 10.4 Mismatch diff sink and `limen report`

Metrics tell you *that* a route is diverging; the mismatch log tells you *how*, but only until the log buffer rolls. The **diff sink** is the durable half: an optional top-level config block that persists every mismatch for later triage.

```yaml
diff_sink:
  dir: "./limen-diffs"    # relative to the process working directory
```

Behavior:

- When the block is present, a sink observer is installed **alongside** the metrics observer (fan-out) — metrics and logs are unchanged whether or not the sink is on.
- Every comparison that is **not** a match appends one JSON object to `<dir>/mismatches-<YYYY-MM-DD>.jsonl`, dated by **UTC**. Matches and non-comparison events write nothing; a clean run never even creates the directory.
- Record shape (one line, no pretty-printing):

```json
{
  "timestamp": "2026-07-28T10:00:05Z",
  "route_id": "get-device",
  "request_id": "0f2c…",
  "method": "GET",
  "path": "/devices/42",
  "legacy_status": 200,
  "new_status": 200,
  "status_match": true,
  "body_match": false,
  "mismatch_kinds": ["body", "set_cookie.value"],
  "differences": [ … ],
  "header_mismatches": [ … ],
  "cookie_mismatches": [ … ],
  "location_mismatches": [ … ],
  "diff_truncated": false
}
```

- Every value written is **already redacted** by the comparison engine (Section 7.5) — the sink adds no new rendering. A dedicated test proves a cookie/`Location` mismatch record contains no raw cookie value and no sensitive query value.
- The sink never does blocking file IO on the shadow task's Tokio worker. `SinkObserver::comparison` only serializes the record and hands it to a bounded (1024-deep) channel; a single dedicated OS thread owns the file handle, date rotation, and the actual write, so a stalled volume parks only that thread, never a Tokio worker (invariant 2). The channel is non-blocking to the producer: a full queue drops-and-counts (warn-once), exactly like an IO failure — a diagnostics sink must never degrade the proxy. On shutdown the observer is dropped, the channel closes, and the writer thread exits (best-effort flush; a diagnostic sink needn't guarantee its last line). It never panics: an IO failure logs one `warn!` (`limen.diff_sink_write_failed`), counts subsequent failures, and drops the record until a write succeeds again.
- Rotation is by date only. **Retention is the operator's** (standard log-retention tooling over the directory); an in-proxy retention policy is future work.

`limen report` aggregates a sink directory without needing the proxy's configuration:

```bash
limen report --dir ./limen-diffs [--route <id>] [--since <RFC3339>] [--format human|json] [--out <path>]
```

It reads every `mismatches-*.jsonl` file in the directory, applies the filters, and prints per-route mismatch counts (total and by `mismatch_kinds`) plus the most recent examples per route. Unparseable lines are **counted and reported**, never fatal — a record torn by a killed process must not cost you the rest of the report. Unknown fields are ignored, so a directory written by a newer Limen still reports against an older binary. Output goes to stdout unless `--out` names a file.

#### `--format html`: the campaign status page

The third format renders a self-contained HTML page over a whole campaign's artifacts rather than the sink alone:

```bash
limen report --dir ./limen-diffs --format html \
  [--config limen.config.yaml] [--verdict verdict.json] \
  [--profile profile.json] [--metrics metrics.txt] [--out report.html]
```

Each optional input is a file that already exists — the config the campaign ran under, a document captured from `limen verdict --format json`, a saved `GET /observe/profile` body, a saved `/metrics` scrape. The page runs nothing and contacts nothing; `--dir` alone still works, and everything not given is rendered as "not provided".

Its defining property is negative: **it must be unable to render a failure or a missing input as success.** The banner has three states — CLEAN, INCOMPLETE, FAILURE — and reaching CLEAN requires the sink directory, `--config` and `--verdict` all present and parsed, every *provided* optional input parsed, a self-consistent verdict that exited 0 online, and no disagreement between artifacts. Sink counts are reconciled against the verdict's per-route map, canary records against its `canary_records`, and verdict floors against the config's `effective_min_comparisons()`; any disagreement is a named finding and a FAILURE. Where the page reads an input `limen verdict` also reads, it takes the same position on it — including which metric families may legitimately be absent (§12.1's required series are required here too; the lazily-registered ones are not).

Two rules follow from that property:

- **`--route` and `--since` are refused with `--format html`** (exit 1). Both filter records *before* aggregation, so a filtered page could reconcile a dirty sink to zero and render green.
- **Producing the page is exit 0 even when every section of it is a failure.** A CI artifact that vanishes on a bad run is one nobody looks at. Only a page that could not be produced — an unwritable `--out`, an incoherent flag combination — is exit 1; an unreadable *input* is a section of the page, not a process failure.

---

## 11. Deployment Models

Limen is written deployment-agnostic and must support both first-class:

### 11.1 Sidecar / co-located (two processes on one box)

Limen and one or both upstreams run on the same host (e.g. legacy and new processes side by side). Upstreams may be `http://localhost:PORT`. Common in staging and incremental production rollouts.

### 11.2 Standalone edge proxy (targets different clusters)

Limen runs toward the edge and routes to legacy and new running in **separate clusters** behind internal DNS/load balancers, typically over **TLS**. This is the higher-scale deployment and the one to keep in mind for the streaming-path performance posture.

### 11.3 Local development (Docker Compose / static binary)

A `examples/docker-compose.yaml` brings up mock legacy + new + Limen for a day-one trial. Limen also builds as a single static binary for local use without Docker.

### 11.4 TLS posture

- **MVP: TLS to upstreams** (HTTPS legacy/new), with certificate verification on by default and an optional custom CA bundle for internal PKI.
- **Post-MVP: client-side TLS termination** at Limen (serving HTTPS to clients) — explicitly a future expansion, designed for but not implemented in MVP.
- Because Limen's own listener is plain HTTP in the MVP, `X-Forwarded-Proto` set by Limen is always `http` (Section 3.6). There is no listener-TLS config to source another value from; if client-side TLS termination lands post-MVP, that config's resolved scheme becomes this value's source instead of the hardcoded constant.

---

## 12. Performance Targets (SLOs)

All targets are **starting defaults**, measured in a controlled local benchmark (`benches/`), and **explicitly team-tunable per deployment environment**:

| Path | Metric | Target |
|---|---|---|
| Streaming path, comparison disabled | p50 added latency | < 1 ms |
| Streaming path, comparison disabled | p99 added latency | < 5 ms |
| Buffer-for-compare path (bodies < ~64 KB) | p50 added latency | < 3 ms |
| Buffer-for-compare path (bodies < ~64 KB) | p99 added latency | < 15 ms |
| Shadow dispatch | client-visible added latency | **0 by design** (fire-and-forget; asserted statistically unchanged with shadow on) |
| Throughput (single instance, comparison disabled) | RPS | No measurable ceiling below upstream saturation in local benchmark; **stretch:** > 10k RPS on commodity hardware |

The shadow-dispatch guarantee is **architectural, not a percentile**: the client must receive the primary response without waiting on shadow or comparison work. An acceptance test asserts client latency is statistically unchanged when shadowing is toggled on.

### 12.1 Rollout budget (forward-looking; not enforced by MVP)

The SLOs above govern **Limen's own overhead** — whether the proxy is healthy. A separate concern, owned by the migration process rather than the proxy, is whether the **new service** is performing acceptably enough to advance a rollout: its latency and error rate relative to legacy, and the response parity (mismatch) rate.

In the MVP these are **operator-checked gates against emitted metrics**, not enforced controls — the migration runbook defines the go/no-go thresholds and the operator reads them off Limen's metrics before raising a rollout percentage. To give those gates a home and signal the intended direction, the route config accepts an **optional `budget` block** (Section 5.2):

```yaml
budget:
  max_new_p95_latency_ratio: 1.0     # new p95 / legacy p95 ceiling
  max_new_error_rate_ratio: 1.0      # new 5xx rate / legacy 5xx rate ceiling
  max_mismatch_rate: 0.001           # parity ceiling (fraction of compared requests)
```

**MVP behavior:** Limen validates the block's shape (Section 5.3) and may surface budget status in logs/metrics, but does **not** automatically gate or roll back on it. A documented post-MVP enhancement is to have Limen *enforce* the budget — e.g. automatically refusing to honor a rollout-percentage increase, or auto-lowering the effective percentage, when a route is out of budget. The two distinct latency obligations — *proxy overhead within SLO* and *new-service latency within budget* — must not be conflated; both must hold for a route to be considered green.

---

## 13. Relationship to Pharos (Functional Test Suite)

- **Separate repositories, no build-time dependency.** Either project can be built and run alone.
- **Shared behavioral contract (Section 4) is the integration point**, as a documented schema and vocabulary — *not* shared code in the MVP.
- **Workflow:** AI drafts a contract from service docs/traffic → **Pharos** validates and refines it deterministically (catching over-normalization and missed real differences) → **Limen** consumes the refined contract unchanged for production shadow comparison and rollout.
- **Same JSONPath subset and same normalization vocabulary** on both sides, so a contract Pharos validated behaves identically in Limen.
- **Readiness signal:** passing Pharos scenarios for a route is a precondition for enabling Limen shadow mode, and a clean Limen shadow mismatch rate is a precondition for raising the rollout percentage.
- **Deferred:** a shared `normalization` crate/package extracted from both could replace the shared-contract-by-schema approach later, once both sides are stable. Designed toward, not built now.

---

## 14. Phased Build Plan

Each phase ends with passing tests and a runnable artifact. Build incrementally and run tests after each major component.

### Phase 0 — Scaffold
- Cargo project, module skeleton (Section 3.5), CI (build + test + clippy + fmt), README stub, license.
- `clap` CLI with `run`, `validate-config`, `print-routes`, `check-contract` (stubs that wire up).
- **Done when:** `cargo build`, `cargo test` (empty), `cargo clippy` all pass in CI.

### Phase 1 — Config + contract loading and validation
- Config model, layered loading, semantic `validate-config`.
- Contract model, loading (YAML/JSON), reference resolution, merge logic, `check-contract`.
- **Done when:** valid configs/contracts load; invalid ones fail with field-level messages; merge and conflict rules enforced; unit tests cover the validation matrix.

### Phase 2 — HTTP core: routing + legacy_only + new_only
- Data-plane listener, route matcher (longest-prefix), upstream client with TLS + timeouts, streaming proxy core, body limits.
- Control-plane listener with `/health/live`, `/health/ready` (basic).
- Implement `legacy_only` and `new_only`.
- **Done when:** integration tests prove legacy-only and new-only proxying preserve method/path/query/headers and return the correct upstream response; streaming works; body limit enforced.

### Phase 3 — Comparison engine
- Normalization (all transforms), supported JSONPath subset, `blake3` hashing, bounded+redacted JSON diff, redaction (headers/paths/query).
- **Done when:** unit tests cover normalization, hashing, diff, and redaction per Section 12 test plan; redaction-in-diff test passes.

### Phase 4 — Shadowing: shadow_legacy_primary
- Shadow eligibility, fire-and-forget dispatch, buffer-for-compare vs. streaming decision, per-request sampling, shadow concurrency limit, comparison wiring, mismatch logging.
- **Done when:** integration tests prove shadow match, shadow mismatch (with diff at sample_rate 1.0), shadow timeout (client unaffected), and that shadow/comparison failures never affect the client.

### Phase 5 — Flags + rollout: percentage_split
- `FlagProvider` trait, static + file + Redis providers, last-known-good, staleness, fail-safe.
- Deterministic rollout hashing + bucket assignment; assignment-key extraction + fallback.
- **Done when:** rollout determinism, distribution, missing-key fallback, file reload, Redis reload, and stale-flag fail-safe all tested (unit + integration).

### Phase 6 — Resilience: circuit breaker + failover_to_legacy
- Circuit-breaker state machine; `failover_to_legacy` mode; `failover_safe` gating with validation that rejects non-idempotent `failover_to_legacy` routes lacking the flag.
- **Done when:** breaker transition unit tests pass; integration test shows new-upstream failures open the circuit and traffic returns to legacy.

### Phase 7 — Observability hardening + graceful shutdown
- Full metric set, structured logging fields, trace-header propagation, readiness degradation logic.
- Graceful shutdown: stop accepting, drain in-flight primary up to timeout, cancel/skip shadow, clean exit.
- **Done when:** metrics-scrape test confirms metric names/labels and absence of high-cardinality/secret labels; graceful-shutdown integration test passes.

### Phase 8 — Performance validation + examples + docs
- `criterion` benchmarks for the SLO table; Docker Compose example; README that runs from a fresh checkout; example config, flags, and contract files.
- **Done when:** benchmarks run and report against SLO defaults; `docker-compose up` yields a working local proxy + mocks; README instructions verified from clean checkout.

---

## 15. Acceptance Criteria (Definition of Done)

The MVP is **done** when all of the following hold:

### 15.1 Functionality
- All five route modes work: `legacy_only`, `new_only`, `shadow_legacy_primary`, `percentage_split`, `failover_to_legacy`.
- Method, path, query string, and relevant headers are preserved through the proxy.
- No matching route returns a configured not-found response.

### 15.2 Configuration & contract
- Config loads from defaults < file < env < CLI.
- `validate-config` performs semantic validation with field-level errors.
- Contracts load from YAML/JSON, references resolve, behavioral rules merge with operational config, conflict rules enforced.
- `check-contract` validates a contract and its JSONPath compliance.
- Missing required config for the selected mode fails with an actionable error.

### 15.3 Comparison
- Hybrid hash-then-diff comparison works.
- Normalization: canonical key order, ignore paths, redact paths, sort arrays by key, unordered arrays, timestamp normalization, enum aliases — all work.
- Supported JSONPath subset enforced; out-of-subset paths rejected at load.
- JSON object key order never causes a false mismatch.
- Diffs are bounded (max count, max value length) and redacted; **no secret appears in any diff or log**.

### 15.4 Shadowing
- Client always receives the legacy (primary) response in shadow mode.
- Eligible reads are shadowed to new; writes are not shadowed unless the route opted the method into `comparison.shadow_methods`, in which case the buffered request body reaches both upstreams byte-identically.
- Shadow or comparison failure never affects the client request or latency.
- Per-request sampling gates buffering and detailed diffing.

### 15.5 Rollout & flags
- Static, file, and Redis providers work; missing flags use defaults; invalid refresh keeps last known good; staleness beyond TTL triggers fail-safe.
- Rollout is deterministic per assignment key; 0% → all legacy, 100% → all new (subject to breaker/fail-safe); intermediate splits distribute within tolerance.
- File and Redis flag changes take effect **without restart**.

### 15.6 Resilience
- Circuit breaker opens on threshold breach, routes to legacy while open, transitions through half-open, and reopens/closes correctly.
- `failover_to_legacy` falls back on new failure/timeout/open-circuit, and does not replay a failed in-flight request against legacy unless the route sets `failover_safe: true`; validation rejects a `failover_to_legacy` route with non-idempotent methods that omits the flag.

### 15.7 Observability & ops
- Prometheus `/metrics` exposes the full required metric set with low-cardinality labels.
- Structured logs include the required fields and are redacted.
- `/health/live` and `/health/ready` behave per spec, including readiness degradation under safe fallback.
- Graceful shutdown drains in-flight primary requests and exits cleanly.

### 15.8 Performance
- Benchmarks exist and report against the SLO defaults; shadow dispatch adds no client-visible latency (statistically asserted).

### 15.9 Quality gates
- `cargo build` and `cargo test` pass.
- `cargo clippy` is clean (deny warnings in CI) and `cargo fmt --check` passes.
- Example local config + Docker Compose run successfully.
- README instructions work from a fresh checkout.
- Limen fails safe to legacy under all configured uncertainty conditions.

---

## 16. Test Plan

### 16.1 Unit tests

**Route matching:** exact match; prefix match; longest prefix wins; method-specific over method-agnostic; header predicate (if implemented); no match; duplicate route IDs fail validation; query conditions (`query_present` AND semantics, `query_absent`, presence regardless of value, percent-decoded names); a conditioned route beats an unconditioned one at an equal prefix but not a longer prefix; a table with no conditions routes identically whatever the query.

**Config validation:** valid minimal config; invalid upstream URL; percentage out of range; missing required route fields; invalid timeouts; out-of-subset JSONPath; duplicate route IDs; unknown route mode; both contract-ref and inline behavioral block on one route fails; `failover_to_legacy` route with non-idempotent methods missing `failover_safe: true` fails; out-of-range `budget` values fail; empty/duplicate query-condition names fail; percent-encoded, `+`-bearing, or whitespace-padded query-condition names fail; a name in both `query_present` and `query_absent` fails; two query-conditioned routes on one prefix fail unless provably disjoint.

**Contract:** valid contract loads; reference resolves; merge of defaults + per-route comparison; conflict (contract + inline) rejected; `check-contract` flags out-of-subset paths.

**Rollout decision:** 0% → legacy; 100% → new; same key → same upstream; different keys distribute; missing key → fallback; open circuit overrides new → legacy.

**Flag provider:** static returns configured; missing → default; file loads; file refresh picks up changes; file keeps last known good on invalid update; Redis loads; Redis refresh; Redis keeps last known good on connection failure; stale TTL → fail-safe.

**JSON normalization:** key-order independence; ignore paths removed; redact paths masked in diff; nested ignore paths; arrays ordered by default; arrays sorted by key; unordered arrays compare as sets; timestamp precision normalization; enum alias mapping; invalid JSON falls back to raw/body comparison per config.

**Hash comparison:** equivalent normalized bodies → same hash; different → different; ignored fields don't change hash; status mismatch detected even if body matches.

**JSON diff:** added field; removed field; changed scalar; changed nested object; array differences; respects max difference count; respects max value length; redaction applied before logging.

**Circuit breaker:** consecutive failures open; successes keep closed; open → half-open after duration; half-open success closes; half-open failure reopens; metrics reflect transitions.

### 16.2 Integration tests (with `wiremock` / test servers)

- **legacy_only:** client gets legacy body; legacy gets one request; new gets zero; metrics show primary=legacy.
- **new_only:** client gets new body; new gets one request; metrics show primary=new.
- **shadow_match:** legacy and new return equivalent JSON with different key order; client gets legacy; both upstreams hit; comparison records match; no diff emitted.
- **shadow_mismatch:** legacy `{"name":"A"}`, new `{"name":"B"}`; client gets legacy; mismatch recorded; sampled diff includes changed path at sample_rate 1.0.
- **shadow_timeout:** legacy fast, new sleeps beyond shadow timeout; client gets legacy quickly; shadow-timeout metric increments; client latency unaffected.
- **percentage_rollout:** assignment header `x-tenant-id`, rollout 50; same tenant stable; many tenants ≈ 50/50 within tolerance; metrics reflect selection.
- **circuit_breaker:** new returns repeated 500s; circuit opens after threshold; subsequent requests route to legacy; state metric flips to open.
- **failover_safe:** in `failover_to_legacy`, a route with `failover_safe: true` and a failing new upstream replays the request against legacy and the client gets the legacy response; a route with `failover_safe: false` returns the new-side failure to the client (the in-flight request is **not** replayed), while the circuit breaker still routes *subsequent* requests to legacy.
- **flag_reload:** file provider starts rollout 0, file updated to 100; proxy observes change without restart; routing follows; refresh-success log/metric.
- **stale_flag_failsafe:** file provider valid, then file invalid; before TTL uses last known good; after TTL applies fail-safe; stale metrics/logs emitted.
- **graceful_shutdown:** start long-running primary request; send SIGTERM; proxy stops accepting; in-flight completes within timeout; clean exit.

### 16.3 Security & privacy tests

- Header redaction (`authorization`, `cookie`) — secrets absent from logs.
- JSON-field redaction (`$.token`, `$.user.email`) — masked in diff output.
- Metric cardinality — many unique path/header IDs do not appear as labels.
- (If debug endpoints exist) disabled by default; sensitive config not exposed.

### 16.4 Performance tests (`criterion`)

- Baseline streaming overhead vs. SLO defaults.
- Shadow overhead — client-visible latency statistically unchanged with shadow on.
- Large-body behavior — bodies above `max_body_bytes` skip comparison (`response_too_large`) or shadowing (`request_too_large`, on a write-shadowing route), no unbounded buffering, client still served.
- High concurrency — stable, bounded memory, shadows throttled/skipped, no panics or task leaks.

---

## 17. Implementation Guidance

Prioritize a clean MVP with strong tests over breadth or cleverness.

**Safe default choices:**
- Default to legacy when uncertain.
- Never block client responses on shadow/comparison.
- Never shadow writes by default; a write is shadowed only where a route opted its method into `comparison.shadow_methods`, and only with a bounded, replayed body.
- Never replay a failed in-flight request against legacy unless the route is explicitly `failover_safe: true` (idempotent). Routing *subsequent* requests to legacy via the circuit breaker is fine; *retrying the same request* that may already have hit new is not, unless idempotent.
- Never log sensitive values by default.
- Bound all buffers.
- Validate configuration and contracts at startup; refuse to start on invalid config.

**Keep abstractions modular** behind traits — `FlagProvider`, the comparison/normalization engine, route decisioning, and the upstream client — so future work (Redis→LaunchDarkly, a shared normalization crate, a Pingora data-plane, client TLS termination, advanced write migration) can land without reshaping the core.

**Prioritize, in order:** correctness and safe fallback; clear configuration and error messages; deterministic behavior; observability; performance on the streaming path.

**Before coding:** read this spec end to end. Then implement phase by phase (Section 14), running tests after each major component.
