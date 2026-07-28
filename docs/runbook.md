# Service Migration Runbook (Pharos + Limen)

**The operational procedure for taking a single HTTP service from a legacy implementation to a new one, safely and with AI assistance, using the Pharos functional test suite and the Limen migration proxy.**

---

## 0. About This Document

This runbook is written for two readers at once:

- **Engineers** performing a migration, who need a concrete, ordered procedure with commands, decision points, and go/no-go gates.
- **Claude Code (or another coding agent)**, which uses this document — together with a target service's source code and OpenAPI/Swagger definition — to generate the migration artifacts (behavioral contract, Pharos scenarios) and to drive the iteration loop. The structure (inputs → actions → outputs at each step) is designed so this runbook can seed a reusable skill.

It assumes familiarity with two companion specifications:

- **Pharos** — black-box functional test suite (TypeScript/Vitest). Validates new vs. legacy deterministically; refines the behavioral contract.
- **Limen** — migration proxy (Rust). Routes live traffic, shadows reads, compares responses, rolls out by percentage, fails back to legacy.

Both consume the **same shared behavioral contract**, in the same vocabulary and the same JSONPath subset, so a contract file is portable between them unchanged.

### 0.1 The assumed migration pattern

This runbook assumes the **most common and lowest-risk** migration shape:

- **Shared datastore.** Legacy and new read and write the **same** backing store. The migration is a **re-implementation of request-handling logic** (e.g. moving off an older framework), not a data migration. A write through either implementation is immediately visible to the other.
- **Behavioral parity over shared data** is therefore the whole correctness question: *given the same stored data and the same request, does the new implementation return what legacy returns?* The contract expresses exactly this.

Where a migration does **not** share a datastore (separate stores that must be kept in sync), the consistency problem is fundamentally harder and edges toward dual-write territory, which both tools treat as out of scope. Section 7.4 flags the cautions; treat that as the exception, not the path.

### 0.2 The per-service lifecycle

```
  1. DISCOVER     → inventory routes, classify read/write, risk-tag
  2. DRAFT        → AI proposes a behavioral contract from code + OpenAPI + samples
  3. GENERATE     → AI writes Pharos scenarios (success / error / edge) per route
  4. REVIEW (gate)→ human validates contract + scenarios before they are trusted
  5. REFINE       → run Pharos; converge contract and new-service behavior to green
  6. SHADOW       → wire route into Limen shadow_legacy_primary; watch parity + budgets
  7. ROLL OUT     → raise percentage 0→1→5→25→100, with a gate between each step
  8. CUT OVER     → new_only or keep failover; Pharos remains as regression
```

Each stage below states its **inputs**, **actions**, and **outputs**, with decision gates called out explicitly.

---

## 1. Stage 1 — Discover

**Goal:** produce a complete, classified inventory of the service's routes and the risk profile of each.

**Inputs:**
- The legacy service source code.
- The OpenAPI/Swagger definition (if present).
- Optionally, captured production traffic or access logs (for real-world payload shapes and traffic distribution).

**Actions:**
1. **Enumerate routes.** From the OpenAPI paths and/or the legacy router/controller code, list every `(method, path_template)` the service exposes.
2. **Classify each route** along three axes:
   - **Read vs. write.** GET/HEAD are reads. POST/PUT/PATCH/DELETE are writes. (A POST used only for complex queries is a *logical read* — note it; it may be safely shadowable, see Section 6.5.)
   - **Idempotency.** Is repeating the request safe? GET/HEAD/PUT/DELETE are typically idempotent; POST typically is not. This determines failover and replay eligibility.
   - **Risk.** Low (pure read, stable shape), medium (read with complex/derived fields, or idempotent write), high (non-idempotent write, money/auth/permissions, side effects beyond the shared store such as emails or third-party calls).
3. **Identify side effects beyond the datastore.** Any route that sends email, calls a third party, enqueues jobs, or charges money cannot be naively shadowed even if it's a "read" in HTTP terms. Mark these `has-external-side-effects`.
4. **Capture representative payloads.** For each route, collect a few real request/response examples (from OpenAPI examples, tests, or captured traffic). These drive contract drafting.

**Outputs:**
- A **route inventory table**: `id`, `method`, `path_template`, read/write, idempotent?, risk, side-effects, sample payloads.
- A migration **ordering**: low-risk reads first, then idempotent writes, then high-risk writes last.

**For the agent:** when reading OpenAPI, treat `paths.*.responses.*.content.application/json.schema` as the authority on response shape, and `examples`/`example` as the authority on concrete values. When reading legacy code, find the router/controller layer to enumerate routes and the serialization layer to find dynamic fields (timestamps, generated IDs, request IDs).

---

## 2. Stage 2 — Draft the Behavioral Contract

**Goal:** an AI-drafted contract that captures, per route, what to compare and which dynamic fields to normalize — expressed in the shared vocabulary.

**Inputs:** the route inventory and sample payloads from Stage 1; the OpenAPI schemas; the legacy serialization code.

**Actions — derive each contract element from observable signals:**

Use the **cross-framework difference catalog** (Section 9) as the lookup table. For each route, walk the response shape and decide, field by field:

1. **Is this field dynamic and non-meaningful?** (request IDs, trace IDs, server-generated timestamps that reflect "now", generated resource IDs when the test doesn't pin them) → add to `ignore_paths`.
2. **Is this field sensitive?** (tokens, emails, secrets, anything that must not appear in a log or diff) → add to `redact_paths`.
3. **Is this an array whose order is not contractually guaranteed?** → `sort_arrays` (by a stable key) if order is incidental but elements are identified, or `unordered_arrays` if it's a true set.
4. **Is this a timestamp whose precision or format may differ between frameworks?** (millis vs. seconds, `Z` vs. `+00:00`) → `normalize_timestamps` at the coarsest precision both can satisfy.
5. **Is this an enum whose spelling/casing differs between implementations but means the same thing?** → `enum_aliases`.
6. **Everything else is a real assertion.** Do **not** normalize it. The default posture is *compare everything*; each normalization rule is a deliberate, justified exception.

Set `compare_status: true` and `compare_body: true` by default. Leave `compare_headers: []` unless a specific header is part of the contract (e.g. `content-type`, `location` on creates).

Record **expectations and intentional changes** in the contract's per-route `expectations.notes`: if the new service is *meant* to differ from legacy (a bug fix, a new field, a changed status), say so here and tag the corresponding scenario `intentional-change` so reviewers know the difference is intended.

**Outputs:** a `*.contract.yaml` file (service `defaults` + per-route `comparison` overrides), validated with `check-contract` in either tool.

**Discipline — the over-normalization trap:** every entry in `ignore_paths`/`unordered_arrays`/etc. is a place where a real regression could hide. Keep paths **narrow** (`$.device.lastSeenAt`, not `$.device`), and attach a reason in a comment or in `expectations.notes`. Stage 5 (Refine) exists largely to catch rules that are too broad.

**For the agent:** prefer the **smallest** contract that makes legitimate dynamic differences disappear. When uncertain whether a field is dynamic-noise or meaningful, **leave it as a real assertion** and let Stage 5 reveal it — a false failure you investigate is safer than a real difference you silently ignored.

---

## 3. Stage 3 — Generate Pharos Scenarios

**Goal:** an executable scenario set per route covering success, error, and edge behavior, referencing the contract for normalization.

**Inputs:** the route inventory; the drafted contract; OpenAPI response definitions (which enumerate status codes and error shapes).

**Actions — for each route, generate the standard scenario family:**

1. **Success (happy path).** The primary 2xx case. `mode: compare_live`, `strategy: json_semantic`, `contract` reference for normalization.
2. **Not-found / missing resource.** The 404 (or equivalent) case for a resource that doesn't exist.
3. **Validation / bad input.** The 4xx case(s) the OpenAPI defines (missing field, bad type, out-of-range). Compare the error envelope shape, not just the status.
4. **Authorization / permission** (if the route is authenticated). The 401/403 cases.
5. **Edge cases by shape:**
   - List endpoints → **empty list**, **single item**, **many items** (exercises array sorting/ordering rules).
   - Endpoints with optional fields → **present** and **absent** variants (exercises null-vs-absent handling).
   - Pagination → first page, middle page, past-the-end.
6. **Multi-step flows** where behavior spans requests (create → read-back, see Section 6). Mark destructive flows `destructive` with a `safety` block and `cleanup` hooks.

**How to decide what each scenario asserts:**
- The **status** comes from the OpenAPI response definition.
- The **body assertions** are "everything in the response except what the contract normalizes."
- For error cases, assert the **error code and message shape** via `subset` or `explicit_expectations` — error envelopes are a common cross-framework difference (Section 9), so be precise about what's contractual.

**Tagging:** every scenario gets tags. At minimum `read`/`write`, a maturity tag (`smoke` for the few that gate CI, `regression` for the rest), and `migration-ready` once it's trusted. Use `intentional-change` and `legacy-bug-compatible` to mark deliberate deviations.

**Outputs:** a set of `*.yaml` scenarios per route; all validate with `pharos -- validate`.

**For the agent:** generate **small, focused** scenarios — one behavior each — rather than large scenarios that assert many things. A focused scenario produces a precise failure. Always generate both the success **and** the error/edge cases; a suite that only covers happy paths gives false confidence.

---

## 4. Stage 4 — Human Review Gate (mandatory)

**Goal:** a human validates the AI-generated design before it is trusted as a correctness gate or consumed by the proxy. This gate is **not optional** and does not disappear as confidence grows — what changes over time is how much scrutiny each item needs, not whether a human signs off.

**Inputs:** the drafted contract; the generated scenarios.

**Review checklist:**

*Contract:*
- [ ] Every `ignore_paths` / `unordered_arrays` / `sort_arrays` entry is **justified** and **narrow**. No broad subtree is ignored to make a test pass.
- [ ] No **meaningful** field is being normalized away (the over-normalization check).
- [ ] `redact_paths` covers everything sensitive; nothing secret can reach a log/diff/recording.
- [ ] Intentional differences are documented in `expectations.notes`.

*Scenarios:*
- [ ] Each scenario represents **real intended behavior**, not an accidental legacy quirk silently codified. Where a legacy bug is intentionally preserved, it's tagged `legacy-bug-compatible` with a reason.
- [ ] Success, error, and edge cases are present per route.
- [ ] Destructive scenarios are correctly marked and guarded with cleanup.
- [ ] Tags are correct, especially `smoke` (these will gate CI) and `intentional-change`.

*Design:*
- [ ] The chosen modes fit the routes (reads `compare_live`; writes handled per Section 6).
- [ ] The migration ordering is sane (low-risk first).

**Output:** an approved contract + scenario set, or change requests back to Stages 2–3. Only approved artifacts proceed.

**Why this gate exists:** the tools generate; humans validate. The entire safety argument rests on a person having confirmed that the tests encode the behavior the organization actually wants preserved, and that the normalization rules don't hide regressions. AI accelerates the drafting; it does not remove the need for judgment about what "correct" means.

---

## 5. Stage 5 — Refine to Green

**Goal:** converge the new service's behavior and the contract until the scenarios pass for the right reasons.

**Inputs:** approved contract + scenarios; a runnable legacy service and an in-progress new service (typically via Docker Compose; both pointed at the **same** datastore per the assumed pattern).

**The loop:**
1. Run the suite locally, focused on the route under work:
   ```bash
   npm run ftest -- run --scenario users.get-user-success
   # or a whole service by tag:
   npm run ftest -- run --include-tag migration-ready
   ```
2. For each failure, **diagnose which kind it is**:
   - **Real behavioral gap in the new service** → fix the new service. (This is the productive majority.)
   - **A legitimate dynamic difference not yet normalized** → add a *narrow* contract rule (and note why).
   - **An over-broad contract rule hiding something** → tighten the rule; the failure it now surfaces is real.
   - **A genuinely intended difference** → convert the scenario to assert the new behavior and tag `intentional-change`.
3. Re-run. Repeat until green.

**Convergence criteria (per route):**
- All required scenarios pass.
- Every normalization rule that was added during refinement has a recorded reason.
- No scenario is passing *only* because of an unjustified ignore rule.

**Outputs:** a green scenario set for the route; a contract whose rules are now **validated**, not just drafted. This validated contract is what Limen will consume in Stage 6 — unchanged.

**For the agent:** when a comparison fails, read the diff and classify it before acting. Resist the reflex to silence a failure by widening an ignore path — that is the failure mode the whole approach is designed to prevent. Prefer fixing the new service; normalize only genuinely incidental differences, narrowly.

**This is where the contract earns trust.** A contract that has survived Pharos refinement against real responses is materially more trustworthy than the AI's first draft — that's the entire point of doing functional validation *before* touching production traffic.

---

## 6. Reads vs. Writes — Different Playbooks

Reads and writes are handled **differently** throughout. This section is the reference; the stages above apply it.

### 6.1 Reads (GET/HEAD)

The straightforward, fully-supported path.

- **Pharos:** `compare_live` — call both, compare with the contract. Reads have no side effects on the shared store, so they are safe to run repeatedly and in any environment.
- **Limen:** `shadow_legacy_primary` — legacy serves the client; new is shadowed; responses compared live. This is the safe observation mode. Raise to `percentage_split` once parity and budgets are green.
- **Data consistency:** trivial under the shared-store pattern — both read the same data, so a passing response comparison *is* a consistency check.

### 6.2 Writes (POST/PUT/PATCH/DELETE)

Writes are **not shadowed by default** and are validated differently, because executing a write twice (once per implementation) against a shared store would double the side effect.

**Recommended path (a): validate writes via `new_only_assert` against controlled data.**
- Use Pharos `new_only_assert` to drive the new implementation's write and assert the response (status, returned resource shape, error envelopes) against explicit expectations derived from legacy behavior.
- Run against **seeded/controlled** test data so the write's effect is known and cleanable. Guard as `destructive` with `cleanup` hooks.
- Because the store is shared and the migration is a logic re-implementation, a write performed by the new service lands in the same place legacy would have put it — so asserting the response shape plus a read-back (below) gives high confidence.

**Advanced pattern (b): compare writes by reading back.**
- A multi-step scenario: perform the write through the new service, then **read the created/updated resource back** (via a GET that has *already* been validated as a read in 6.1) and compare that read against what legacy produces for the same resource.
- This validates that the new write produced a stored result indistinguishable from legacy's, using the already-trusted read path as the oracle. Mark `destructive`, seed and clean up.

**Out of scope (c): shadowing writes to an isolated store.** Running the write against both implementations with a separate store for "new" approaches dual-write and reconciliation, which both tools deliberately exclude. Do not attempt this as part of the standard runbook; if a migration truly needs it, it's a bespoke effort outside Pharos/Limen.

**Limen and writes:**
- Writes move via `percentage_split` (not shadow), so each request goes to exactly **one** implementation — no doubled side effect. Determinism by assignment key keeps a given tenant/user consistent.
- **Never auto-failover a non-idempotent write.** Limen's `failover_to_legacy` must not retry a POST on the new service after a failure unless the route is explicitly marked safe — a failed-but-applied write retried on legacy would double it. Idempotent writes (PUT/DELETE) can be configured for failover.

### 6.3 Routes with external side effects

A route marked `has-external-side-effects` (email, third-party call, payment, job enqueue) must **not** be shadowed even if it's an HTTP read, because the shadow would trigger the side effect a second time. Migrate these via `percentage_split` only, validate with `new_only_assert` against test doubles/sandboxes, and review with extra care.

### 6.4 Idempotency summary

| Method | Typical idempotency | Shadow? | Failover? |
|---|---|---|---|
| GET / HEAD | idempotent | yes (default) | yes |
| PUT / DELETE | idempotent | no | yes, if configured |
| POST | non-idempotent | no | **no** unless explicitly safe |
| POST-as-query (logical read, no writes) | idempotent | yes, if confirmed no side effects | yes |

### 6.5 Logical reads over POST

Some services use POST for complex queries that don't mutate state. If you can **confirm** (from code) that such a route is side-effect-free, it may be treated as a read for shadowing — but this requires explicit confirmation, not assumption, and should be noted in the route inventory.

Limen makes that confirmation explicit in config: the route opts the method in with `comparison.shadow_methods: ["POST"]` (see the [config reference](reference/config-reference.md)). The request body is buffered within `max_body_bytes` and replayed byte-identically to both upstreams; a larger body streams to the primary and is not shadowed (`comparison_skipped{reason="request_too_large"}`). Nothing changes for routes that don't opt in.

---

## 7. Data Consistency Validation

Under the assumed **shared-datastore** pattern, consistency validation is mostly about **response and behavioral parity over the same data** — which the contract and scenarios already deliver. This section makes the guarantees and the residual edges explicit.

### 7.1 What "consistent" means here

Because both implementations read and write the same store:
- **For reads:** identical stored data → responses must match (after normalization). A passing `compare_live` is a direct consistency check.
- **For writes:** a write through the new implementation must leave the store in the same state legacy would have. Validated via read-back (Section 6.2b).

### 7.2 Read-after-write across implementations

The shared store makes cross-implementation read-after-write meaningful and safe to test:
- Write via new (Pharos `new_only_assert` or a `percentage_split` request in Limen), then read via the **trusted** legacy-validated read path, and confirm the result matches legacy's representation. This catches new-write logic that stores subtly different data (wrong defaulting, dropped fields, format drift) even when the *write response* looked fine.

### 7.3 Drift checks during rollout

While a write route is split between implementations, periodically run the read-back consistency scenarios (Section 6.2b) against resources created by **each** implementation. Both populations should read back identically. Divergence is a signal to halt rollout (Section 8 gates).

### 7.4 The separate-store exception (caution)

If legacy and new do **not** share a store, none of the above holds cheaply: writes must be reconciled, read-after-write spans two stores, and drift is continuous. This is dual-write territory, explicitly outside Pharos/Limen. If you find yourself here, stop and design a dedicated data-migration/reconciliation approach; do not rely on this runbook's consistency guarantees.

---

## 8. Stages 6–7 — Shadow, then Roll Out (with Budgets)

This is where validation moves from deterministic testing to live traffic, and where the **latency + error-rate budget** becomes the go/no-go gate.

### 8.1 The budget (define before shadowing)

For each route, define a **budget** the route must satisfy to advance. Treat these as conventions the engineer maintains and checks against Limen's exposed metrics (a forward note: this budget could become a per-route field in Limen config later; today it is a documented gate, not a tool feature).

Default budget (tune per route/service):

| Dimension | Default gate to advance |
|---|---|
| **Response parity (shadow)** | Mismatch rate **< 0.1%** of compared requests over the observation window, with **zero** unexplained mismatches in high-risk fields. Every recurring mismatch is either fixed or explicitly accepted as `intentional-change`. |
| **New-service error rate** | New 5xx rate **≤ legacy 5xx rate** for the same route over the window (new must not be worse). |
| **New-service latency** | New **p95 ≤ legacy p95** (or within a **documented exception**, e.g. "+15% p95 allowed because Z") for the route over the window. p99 not pathologically worse. |
| **Proxy overhead** | Limen's own added latency within its SLO (streaming p50 < 1 ms, p99 < 5 ms; buffer-for-compare p50 < 3 ms) — i.e. confirm the *proxy* is healthy, separately from the service. |

A **migration exception** is a deliberate, documented allowance for a route to regress within a bound (e.g. a route that does more work in the new implementation by design). Record it next to the route; it changes the gate for that route only.

### 8.2 Stage 6 — Shadow

**Action:** wire the route into Limen as `shadow_legacy_primary` with the validated contract referenced, comparison enabled, and a sensible `sample_rate` for the route's volume.

```yaml
routes:
  - id: "get-user"
    match: { methods: ["GET"], path_prefix: "/users/" }
    legacy_upstream: "https://legacy.internal"
    new_upstream: "https://new.internal"
    mode: "shadow_legacy_primary"
    contract: "./contracts/user-service.contract.yaml#get-user"
    comparison: { enabled: true, sample_rate: 0.1, max_body_bytes: 262144 }
```

**Watch (on Limen's metrics/logs):**
- `comparison_match` vs. `comparison_mismatch` → the parity rate.
- Sampled mismatch diffs → *what* differs; feed recurring ones back as new Pharos scenarios (closing the loop) or accept as intentional.
- New-upstream latency and error metrics vs. legacy → the latency/error budget.
- Shadow-skip reasons → confirm you're actually comparing enough traffic.

**Gate to proceed:** the budget (8.1) is green over a meaningful observation window, and every recurring mismatch is resolved or explained. Clients have been served **only** legacy this entire time — shadow mode carries no user-facing risk.

### 8.3 Stage 7 — Percentage rollout

**Action:** move the route to `percentage_split` and raise the rollout flag in steps, **pausing at each step** to recheck the budget against *real* traffic now hitting the new service:

```
0%  →  1%  →  5%  →  25%  →  50%  →  100%
```

```yaml
routes:
  - id: "get-user"
    mode: "percentage_split"
    rollout:
      percentage_flag: "migration.get-user.rollout_percentage"
      default_percentage: 0
      assignment_key: { header: "x-tenant-id", fallback: "request_random" }
    # optionally keep shadowing the non-primary side to continue comparing
```

Raise the flag at runtime (no redeploy) via the flag provider. After each increase:
- Recheck the budget (8.1) on the now-live new traffic.
- For write routes, run the read-back drift check (7.3).
- Only advance when green; otherwise **hold or roll back** (8.4).

### 8.4 Rollback / abort criteria (named, automatic where possible)

Roll back **immediately** if any of the following, during shadow or rollout:

- New-service **error rate** exceeds the budget → Limen's **circuit breaker** opens automatically and returns traffic to legacy; investigate before re-enabling.
- New-service **latency** breaches the budget (and no exception covers it) → **lower the rollout flag** to the last-green percentage.
- **Parity** regresses (mismatch rate climbs, or a high-risk-field mismatch appears) → lower the flag; reproduce as a Pharos scenario; fix; re-refine.
- **Data drift** appears in read-back checks → halt rollout at the current percentage; do not advance until resolved.

Rollback mechanisms, in order of automaticity:
1. **Circuit breaker** (automatic, per Limen config) — covers error/timeout spikes with no human action.
2. **Lower the rollout flag** (manual, instant, no redeploy) — the primary deliberate rollback lever.
3. **Set the route to `legacy_only`** (config change) — full stop for that route.

Because traffic shifting is flag-driven, rollback is **fast and reversible** — a core safety property of the approach.

---

## 9. Cross-Framework Difference Catalog

A generic lookup table mapping the **common ways two HTTP/JSON implementations differ for incidental reasons** to the contract rule that handles each. Use it in Stage 2 (drafting) and Stage 5 (refining). It is framework-agnostic by design — these patterns recur across most stack changes.

| Difference pattern | Why it happens | Contract handling | Caution |
|---|---|---|---|
| **JSON object key order** | Different serializers emit keys in different orders | Handled automatically by `json_semantic` / canonical key sort — **no rule needed** | — |
| **Timestamp precision** (millis vs. seconds) | Framework default time formatting differs | `normalize_timestamps` at the coarser precision | Don't over-coarsen and hide a real time bug |
| **Timestamp zone format** (`Z` vs. `+00:00`) | Library formatting choice | `normalize_timestamps` (normalize representation) | — |
| **Generated IDs / UUIDs** | New resource IDs differ per request | `ignore_paths` on the ID **when the test doesn't pin it**; otherwise extract and reuse | Ignoring an ID you should be asserting hides identity bugs |
| **Request/trace/correlation IDs** | Per-request, non-deterministic | `ignore_paths` (or redact if sensitive) | — |
| **Array ordering** (incidental) | Different query/iteration order over the same data | `sort_arrays` by a stable key | Only if order is genuinely non-contractual |
| **Array as set** | Order truly meaningless (e.g. permissions) | `unordered_arrays` | Confirm order isn't part of the contract |
| **Enum casing/spelling** (`ACTIVE` vs `enabled`) | Re-implementation chose different tokens | `enum_aliases` | Document the mapping; ensure it's intended, not a typo |
| **Null vs. absent field** | Frameworks differ on emitting `null` vs. omitting | Decide the contract: assert the intended one; normalize the other only if the difference is agreed-incidental | This is often a **real** API contract question — don't paper over it reflexively |
| **Number formatting** (`1.0` vs `1`, trailing zeros) | Serializer numeric handling | Normalize only if semantically equal and agreed incidental | Precision changes can be real |
| **Error envelope shape** (`{error:{code,message}}` vs `{message}`) | Frameworks have different default error formats | Assert the **intended** envelope explicitly (`subset`/`explicit_expectations`); if the new shape is intended, tag `intentional-change` | Error contracts matter to clients — be deliberate |
| **Default/missing pagination fields** | Framework defaults differ | Assert intended pagination contract; normalize only incidental metadata | — |
| **Header casing / volatile headers** (`Date`, `X-Request-Id`) | Per-response, non-contractual | Compare headers only when listed; ignore volatile ones | — |
| **Whitespace / content-type charset** (`application/json` vs `; charset=utf-8`) | Framework defaults | Normalize/ignore if incidental | — |

**Rule of thumb for the catalog:** the left column is *incidental representation*; the right column makes it disappear **narrowly**. Anything that might be a real API-contract decision (null-vs-absent, error envelopes, number precision) is flagged "caution" because the correct move is often to **assert the intended behavior**, not to normalize the difference away.

---

## 10. Monitoring & Validating the Proxy Itself

Distinct from validating the service: you must also confirm **Limen** is behaving correctly, so that a "green" reading reflects reality.

**Confirm the proxy is healthy (separately from the service):**
- **Proxy overhead within SLO.** Limen's added latency on the streaming path (p50 < 1 ms, p99 < 5 ms) and buffer-for-compare path (p50 < 3 ms) — from Limen's own latency metrics, isolating proxy time from upstream time. If proxy overhead is out of band, a latency reading attributed to the new service may actually be the proxy.
- **Shadow truly off the client path.** Client-visible latency must be unchanged whether shadowing is on or off (Limen guarantees this architecturally; verify it in staging by toggling shadow and watching client latency).
- **Comparison coverage.** Watch `comparison_skipped` reasons — if most traffic is skipped (`response_too_large`, `not_sampled`, `concurrency_limit`), your parity rate is based on too little data; adjust `sample_rate`/`max_body_bytes` or accept the sampling and widen the window.
- **Flag health.** Provider health and staleness metrics green — a stale flag provider means rollout decisions may be running on the fail-safe (legacy), which is safe but means your "rollout" isn't actually happening.
- **Circuit-breaker state.** Know whether a breaker is open; an open breaker silently routing to legacy can masquerade as "new service has no traffic / no errors."

**Two-part latency obligation, restated:**
1. **The proxy adds acceptable overhead** (proxy health) — Limen SLO.
2. **The new service meets or beats legacy latency per route** (or within a documented exception) — the budget in Section 8.1.

Both must be true to call a route green. Conflating them is a common mistake: a fast proxy in front of a slow new service is not a green route, and a slow proxy can make a fast new service look bad.

---

## 11. Stage 8 — Cut Over and Regression

**Goal:** complete the migration and retain ongoing protection.

**Actions:**
- At 100% and stable, choose the end state:
  - **`failover_to_legacy`** — new is primary, legacy remains a safety net. Safest if legacy is still running and cheap to keep.
  - **`new_only`** — once legacy is decommissioned. No fallback; only after sustained confidence.
- **Keep the Pharos scenarios** as a **regression suite**. They run in CI for the new service indefinitely, in local/staging/CI, catching future regressions. The validated contract remains the comparison reference.
- Feed any **post-cutover** issues back as new scenarios.

**Outputs:** a migrated route at 100%, a documented end-state mode, and a living regression suite.

---

## 12. Quick Reference — Per-Route Checklist

```
[ ] DISCOVER: route classified (read/write, idempotent?, risk, side-effects), samples captured
[ ] DRAFT: contract rules derived narrowly from observable signals; check-contract passes
[ ] GENERATE: success + error + edge scenarios; correctly tagged; pharos validate passes
[ ] REVIEW (human gate): contract justified & narrow; scenarios = intended behavior; signed off
[ ] REFINE: suite green; failures diagnosed correctly; no unjustified ignore rules; contract validated
[ ] BUDGET: latency + error-rate + parity budget defined (with any documented exceptions)
[ ] SHADOW (reads): Limen shadow_legacy_primary; parity & budgets green over the window
[ ] WRITES: validated via new_only_assert (+ read-back); no shadow; no auto-failover on POST
[ ] ROLL OUT: 0→1→5→25→50→100, rechecking budget at each step; drift checks for writes
[ ] PROXY HEALTH: overhead within SLO; shadow off client path; coverage adequate; flags/breaker healthy
[ ] ROLLBACK READY: breaker configured; flag lever understood; legacy_only escape known
[ ] CUT OVER: end-state mode chosen (failover_to_legacy or new_only)
[ ] REGRESSION: scenarios retained in CI; contract retained as reference
```

---

## 13. Notes for Skill Authoring

This runbook is structured so a skill can be derived from it. The skill's job, given a service's code + OpenAPI + samples, is to **produce the artifacts** the runbook's early stages call for and to guide the later stages. Maintain these mappings when authoring the skill:

- **Discover → a route-inventory generator.** Input: OpenAPI + router code. Output: the classified inventory table (Section 1).
- **Draft → a contract generator.** Input: inventory + samples + serialization code. Output: a `*.contract.yaml` using the catalog (Section 9) to choose rules. Bias: narrowest rules; everything else is a real assertion.
- **Generate → a scenario generator.** Input: inventory + contract + OpenAPI responses. Output: the standard scenario family per route (Section 3), small and focused, success + error + edge.
- **Review → a checklist surfacer.** The skill **presents** the Stage 4 checklist for a human; it does not self-approve.
- **Refine → a failure classifier.** Given a Pharos diff, classify it (real gap / legitimate-dynamic / over-broad-rule / intended-change) and recommend the corresponding action (Section 5), defaulting to "fix the new service," never to "widen the ignore path."
- **Shadow/Roll out → a gate evaluator.** Given Limen metrics, evaluate the budget (Section 8.1) and recommend advance / hold / roll back per Section 8.4.

The invariant the skill must preserve end to end: **the tools generate and recommend; a human validates intent and correctness.** Every normalization rule and every trusted scenario passes through human judgment before it gates anything or reaches production traffic.