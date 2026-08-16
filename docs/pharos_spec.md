# Pharos — Black-Box Functional Test Suite for Service Migration

**A standalone TypeScript/Vitest functional test harness that validates a new service implementation against a legacy one, deterministically and from the outside, before and during migration — and refines the behavioral contract that drives production rollout.**

---

## 0. About This Document

This is the implementation specification for **Pharos**, intended to be handed to a coding agent (Claude Code, Codex, etc.) and to human engineers. It defines goals, scope, architecture, the scenario spec format, the shared behavioral contract, the phased build plan, acceptance criteria, and test plan in enough detail to implement the project from a clean checkout with high test coverage.

Pharos is one of two complementary projects:

- **Pharos** (this project, TypeScript/Vitest): a black-box functional test suite. Validates a new service against a legacy service deterministically in local development, CI, and staging — before and alongside rollout. It is where the behavioral contract is **refined**.
- **Limen** (separate project, Rust): the runtime migration proxy. Handles live traffic routing, read-path shadowing, response comparison, gradual rollout, and safe fallback. It is where the refined contract is **consumed** against production traffic.

The two share a **behavioral contract** (Section 5) but are independently deployable and have no build-time dependency on each other. See Section 13 for the relationship.

> **Naming note:** *Pharos* was the great lighthouse of Alexandria — the fixed reference that let ships verify their position and cross safely. Pharos the test suite is the fixed, deterministic reference against which a new service's behavior is checked before it carries real traffic.

> **Vocabulary note:** Pharos, Limen, and the shared contract use **one** normalization vocabulary and **one** JSONPath subset (Section 8.4). Field names are snake_case (`ignore_paths`, `sort_arrays`, `normalize_timestamps`, `enum_aliases`). This is a deliberate reconciliation so a single contract file is portable, unchanged, between the test suite and the proxy. Earlier draft dialects (camelCase `ignorePaths`, etc.) are superseded by this spec.

---

## 1. Goals and Non-Goals

### 1.1 Primary goals

1. **Validate black-box behavioral compatibility.** Treat both systems as black-box HTTP APIs. Issue requests to legacy and/or new and compare outputs. The core question: *for the same input and equivalent environment, does the new service behave like the legacy service?*
2. **Make behavior explicit through reviewable scenario specs.** Capture service behavior in readable YAML scenarios that humans can review in pull requests and LLMs can generate.
3. **Support local-first fast iteration.** A developer runs a focused subset locally and sees meaningful failures — request details, response summaries, JSON-aware diffs.
4. **Support CI/CD quality gates.** The same scenarios run in CI with deterministic, machine-readable output that can fail builds and produce artifacts.
5. **Support recording and replay.** Capture known-good legacy interactions, parameterize them, and replay them against the new service when legacy is unavailable or deterministic replay is desired.
6. **Refine the shared behavioral contract.** Take an AI-drafted contract, validate its normalization rules against real behavior, catch over-normalization and missed real differences, and tighten it — producing the contract that Limen consumes in production.
7. **Enable AI-assisted migration.** Be designed so coding agents can generate scenarios from docs/OpenAPI/traffic, implement new-service behavior against failing scenarios, and keep specs human-reviewable.

### 1.2 Non-goals (MVP)

- Unit testing the internals of either service, or replacing service-specific unit/integration tests.
- Production traffic routing, shadowing, or rollout — that is Limen's job.
- Long-term analytics dashboards or historical trend storage.
- Stateful load testing or large-scale performance testing.
- Protocols beyond HTTP/JSON (no gRPC, GraphQL, WebSockets in MVP). Plain-text response bodies are supported as a fallback.
- Automatically proving business correctness without reviewed scenarios, or generating perfect tests without human validation.
- Dual-writing or reconciling production data.

### 1.3 Assumed migration pattern

Pharos is designed for the same migration shape as Limen: **legacy and new share the same backing datastore**, and the migration is a **re-implementation of request-handling logic** (e.g. a framework or language change), not a data migration. This makes correctness a question of **behavioral parity over shared data**, which the contract and scenarios express directly:

- For **reads**, a passing `compare_live` over identical stored data is itself a consistency check.
- For **writes**, correctness is validated by performing the write through the new service and **reading the result back** through an already-validated read path, comparing it to what legacy produces (a multi-step scenario; see the migration runbook). Writes are not executed against both implementations, since a shared store would double the side effect.

Migrations that do **not** share a datastore (separate stores requiring synchronization) move into dual-write/reconciliation territory, which is out of scope; Pharos's consistency guarantees are not designed for that case.

---

## 2. Personas and Core Use Cases

### 2.1 Personas

- **Developer implementing a new service:** runs scenarios locally, inspects failures, fixes the new implementation, reruns focused scenarios.
- **Service owner:** reviews scenario coverage and decides whether the new service is ready for Limen shadowing or rollout.
- **Platform engineer:** maintains the harness, CI integration, shared utilities, and reporting conventions.
- **AI coding agent:** reads this spec and implements the harness; later, generates scenarios and uses failures to fix new-service behavior, and drafts the behavioral contract Pharos then refines.
- **Reviewer:** reviews scenario specs in PRs, checking they represent real intended behavior rather than codifying accidental legacy bugs.

### 2.2 Core use cases

1. **Local comparison during development.** `bun run ftest -- run --scenario users.get-user-success` calls both services, compares, prints readable pass/fail with diffs.
2. **CI compatibility gate.** A PR modifying the new service triggers the suite against deployed/containerized endpoints; required-scenario failures fail the build.
3. **Legacy recording workflow.** An engineer captures a real legacy interaction and converts it to a scenario; dynamic values (IDs, timestamps, tokens, request IDs, env URLs) become variables or normalization rules.
4. **Replay against new service.** Recorded legacy interactions are replayed against the new service and compared to the recorded response (or a fresh legacy response, depending on mode).
5. **Spec-first implementation with LLMs.** An LLM drafts scenarios from docs/behavior; humans review; an agent implements the new service until scenarios pass.
6. **Migration-readiness assessment.** A team evaluates whether a set of endpoints is ready for Limen shadow mode or rollout, using coverage and pass rates by tag.
7. **Post-rollout regression suite.** After a route moves behind Limen, Pharos remains useful for deterministic regression testing locally, in staging, and in CI.

---

## 3. Architecture

### 3.1 Technology choice

| Concern | Choice | Why |
|---|---|---|
| Language | **TypeScript** | Strong typing for maintainable specs and harness internals; excellent JSON/YAML ecosystem; friendly to AI agents; good HTTP and diff libraries; works well in CI. |
| Test runner | **Vitest** | Fast startup/execution; modern TS support; Jest-like API; good watch mode; suitable for focused local runs and CI. |
| TS execution | `tsx` | Run TypeScript CLI scripts without a separate build step. |
| Schema validation | `zod` | Strict scenario/contract validation with good error messages. |
| YAML | `yaml` | Parse scenario and contract files. |
| HTTP | native `fetch` / `undici` | Black-box requests to upstreams. |
| JSON diff | `jest-diff`, `jsondiffpatch`, or a custom utility | Readable structural diffs. |
| CLI | `commander` or `cac` | Subcommand parsing. |
| Scenario discovery | `fast-glob` | Find scenario files. |
| Structured logging | `pino` (optional) | If structured logs are needed beyond console output. |
| JUnit reporting | `junit-report-builder` or equivalent | CI-native report. |

The implementing agent may finalize dependencies, but the above is the assumed, recommended set.

### 3.2 Two layers: scenarios (executable) and contract (behavioral truth)

Pharos has two distinct artifacts, mirroring how Limen separates operational config from the shared contract:

1. **Scenario specs** (Section 4) — the **executable** layer. Each scenario describes a request (or multi-step flow), setup/cleanup, variables, the execution mode, and which contract rules to apply. Scenarios are how Pharos *runs* tests.
2. **The behavioral contract** (Section 5) — the **shared comparison truth**. The same framework-agnostic artifact Limen consumes, containing the normalization vocabulary (ignore/redact paths, array sorting, timestamp/enum normalization) per service and route.

Scenarios **reference** contract rules the same way Limen routes do (`device-service.contract.yaml#get-device`); both tools therefore read the **identical file**. Inline comparison rules remain supported for quickstart/fallback (Section 4.7), using the same vocabulary.

### 3.3 Execution pipeline

For each scenario:

1. Initialize scenario context.
2. Run scenario-level **setup** hooks.
3. For each step:
   1. Resolve variables in the request.
   2. Run step-level **before** hooks (if any).
   3. Execute the **legacy** request if the mode requires it.
   4. Execute the **new** request if the mode requires it.
   5. Store variables **extracted** from responses.
   6. **Normalize** responses (using merged contract rules).
   7. **Compare** per the scenario's strategy.
   8. Write **artifacts** on failure (or when configured).
   9. Run step-level **after** hooks (if any).
4. Run scenario-level **cleanup** hooks — **always**, even if a step failed.
5. Emit the scenario result.

Default behavior is **stop-on-first-failure within a scenario**; cleanup still runs.

### 3.4 Vitest integration

Two patterns are acceptable; the MVP uses the first and may add the second later:

- **Pattern A (MVP):** a custom CLI executes scenarios and produces custom reports; **Vitest tests the harness itself** (schema, comparison, normalization, hooks, recording, CLI). Simpler for custom reporting and the primary path for this spec.
- **Pattern B (later):** a Vitest file dynamically discovers scenarios and generates `describe`/`it` blocks, integrating with Vitest reporters. Add only if it creates value.

### 3.5 Module structure

```text
pharos/
  package.json
  tsconfig.json
  vitest.config.ts
  README.md
  LICENSE
  pharos.config.ts                 # or .json — config file (Section 6)
  src/
    index.ts                       # public barrel: hook/config/scenario/contract types (Section 19.1)
    scaffold.ts                    # `init` template generation, in-process (Section 19.2)
    cli/
      run.ts                       # run scenarios (filters, modes)
      validate.ts                  # validate scenarios + contracts
      record.ts                    # record legacy interactions (explicit opt-in)
      check-contract.ts            # validate a contract + JSONPath compliance
      init.ts                      # scaffold a conformance directory (Section 19.2)
    config/
      config.ts                    # load + layer config
      env.ts                       # env var parsing
    contract/
      model.ts                     # zod schema + types for the shared contract
      load.ts                      # load YAML/JSON, resolve `path#routeId`
      merge.ts                     # merge defaults + per-route + inline overrides
    scenarios/
      discover.ts                  # fast-glob scenario discovery + filtering
      load.ts                      # parse + validate scenarios
      schema.ts                    # zod schema for scenarios
      types.ts                     # Scenario, Step, Compare, etc.
    execution/
      runner.ts                    # scenario orchestration
      step-runner.ts               # per-step execution
      http-client.ts               # fetch/undici client, timeouts, default headers
      cookies.ts                   # per-target cookie jar (Section 9.5)
      variables.ts                 # template substitution + extraction
      hooks.ts                     # hook registry loading + invocation
      fixtures.ts                  # recording read/write helpers
    comparison/
      compare.ts                   # strategy dispatch
      normalize.ts                 # normalization transforms
      jsonpath.ts                  # the supported JSONPath subset
      json-diff.ts                 # readable structural diff
      matchers.ts                  # explicit-expectation matchers
      headers.ts                   # Set-Cookie/Location parsing + comparison (Section 8.6)
      expectations.ts              # one-sided `expect` vocabulary (Section 4.7)
      redaction.ts                 # header/path/query redaction
      result.ts                    # ComparisonResult, Mismatch types
    reporting/
      console-reporter.ts          # local developer output
      json-reporter.ts             # machine-readable JSON report
      junit-reporter.ts            # JUnit XML report
      artifacts.ts                 # redacted failure artifacts
  scenarios/
    examples/                      # the required example scenarios (Section 15)
  contracts/
    example-service.contract.yaml  # shared contract example (portable to Limen)
  hooks/
    index.ts                       # named hook registry
    auth.ts                        # example: session-establishment hook pattern
    data-factory.ts
    cleanup.ts
  fixtures/
    recordings/                    # recorded legacy responses (JSON)
  reports/                         # JSON / JUnit output + artifacts
  tests/                           # Vitest tests for the harness itself
```

---

## 4. Scenario Spec Format

### 4.1 Design principles

A good scenario is: stable across environments; explicit about what behavior matters; clear about which dynamic fields are ignored/normalized; focused on one behavior or journey; safe to run repeatedly; easy to debug on failure; small enough for code review.

YAML is the authoring format; JSON is acceptable for generated specs.

### 4.2 Top-level structure

```yaml
version: 1
id: users.get-user-success
name: Get an existing user
description: >
  Verifies the new service returns the same user representation as legacy for
  an existing user.
service: user-service
tags: [read, smoke, migration-ready]
mode: compare_live
safety:
  destructive: false
contract: "../contracts/user-service.contract.yaml#get-user"   # shared rules
variables:
  userId: user-123
setup:
  hooks: []
steps:
  - id: get-user
    name: Fetch user by ID
    request:
      method: GET
      path: /users/{{ variables.userId }}
      headers:
        authorization: Bearer {{ env.AUTH_TOKEN }}
    compare:
      strategy: json_semantic
      status: same
      # behavioral normalization comes from the referenced contract;
      # see Section 4.7 for the inline-rules fallback.
cleanup:
  hooks: []
```

### 4.3 Top-level fields

- `version` — required integer; `1` for MVP.
- `id` — required stable unique ID; lowercase dot- or slash-separated (`users.get-user-success`, `devices.list-devices-empty`).
- `name` — required human-readable name.
- `description` — recommended; explain the behavior and why it matters.
- `service` — required service identifier (should match a `service` in the contract when referenced).
- `tags` — required list (Section 4.8).
- `mode` — required; one of the execution modes (Section 4.4).
- `safety` — optional safety metadata (Section 4.5).
- `contract` — optional reference to shared comparison rules (`path#routeId`). Mutually exclusive with inline behavioral rules per step (Section 4.7).
- `variables` — optional map of scenario variables.
- `cookies` — optional boolean, default `false`; enables the per-target cookie jar for this scenario run (Section 4.6, Section 9.5).
- `setup` / `cleanup` — optional hook blocks.
- `steps` — required; one or more steps.

### 4.4 Execution modes

- **`compare_live`** — call both legacy and new; compare live responses. For active compatibility validation when both are available.
- **`legacy_record`** — call legacy only; write a recording fixture **only when recording is explicitly enabled**. For capturing known legacy behavior.
- **`replay_against_recording`** — load a recorded legacy response; call the new service; compare new to the recording. For when legacy is unavailable in CI or deterministic replay is desired.
- **`new_only_assert`** — call the new service only; assert explicit expectations from the scenario. For intentionally changed, new, or legacy-absent behavior.

### 4.5 Safety metadata

```yaml
safety:
  destructive: false
  requiresProductionGuardOverride: false
  allowedEnvironments: [local, ci, staging]
```

`allowedEnvironments` is compared against the config's `environment` field (Section 6.2), **not** `output_mode` — a scenario tagged `[local, ci, staging]` does not run when `environment: production` (Section 12), regardless of the configured output mode. Tag a scenario with the full environment list (`[local, ci, staging, production]`) if it is genuinely safe everywhere; tagging it `[production]` alone would make it skip everywhere else — never failing the run, but also not coverage: it counts only under `skipped` (Section 11.5), never `passed` — a tagging bug, not a feature.

### 4.6 Steps, requests, and extraction

Each step has an `id`, optional `name`, a `request`, optional `extract`, and a `compare` block.

```yaml
request:
  method: POST                      # GET | POST | PUT | PATCH | DELETE | OPTIONS | HEAD
  path: /users                      # absolute-URL paths allowed iff same-origin (Section 9.4)
  query:
    includeDetails: true
  headers:
    authorization: Bearer {{ env.AUTH_TOKEN }}
    content-type: application/json
  body:
    email: test-{{ random.uuid }}@example.com
    displayName: Test User
  # form: { grant_type: authorization_code }  # urlencoded; mutually exclusive with body (Section 9.6)
  follow_redirects: true            # optional, default true — see the pitfall in Section 9.3
                                    #   (on-disk name; `followRedirects` in memory, Section 9.3)
  timeoutMs: 5000                   # optional per-request timeout override
```

OPTIONS and HEAD requests must not set `body` or `form` — a validation error (Section 9.1). GET requests must not set `form` either — a GET form has no meaning — also a validation error (Section 9.6); `body` on GET is unaffected and remains silently ignored by the client.

### Cookie jar (`cookies: true`)

Setting the scenario-level `cookies: true` (Section 4.3) enables a **per-target** cookie jar for the run — independent jars for `legacy` and `new`, so a `compare_live` scenario never shares cookies across targets. Once enabled:

- Every step response's `Set-Cookie` headers (the `setCookie` capture, Section 9.2) are applied to the jar for that response's target, **including** the 30x response of a `follow_redirects: false` step — a manual redirect chain still accumulates cookies at each hop.
- The jar keys entries by **(name, path)** per RFC 6265 — domain is constant per target, so it is not part of the key. A `Set-Cookie` for a (name, path) pair already in the jar is **last-write-wins**: it replaces that entry's value and attributes. Two cookies with the same name but different `Path` attributes coexist as separate entries.
- On send, every subsequent request to that target gets a `Cookie` header built from the jar entries whose `Path` matches the request path, ordered **most-specific-path-first** (standard cookie path-matching), unless the step declares its own explicit `Cookie` header (Section 9.5) — in which case the jar is not consulted for that request's outgoing header, though it still ingests that response's `Set-Cookie`.
- A `Cookie` entry in the config's `default_headers` (Section 6.2) is **overridden by the jar**: a jar-built `Cookie` header replaces it for that request, since a run-wide default cookie must not silently outrank the session the scenario just established. Only a **step's** explicit `Cookie` header replaces the jar's (below and Section 9.5).
- The jar is scoped to **one scenario run** — it never persists or leaks across scenarios.
- Cookie **values** are still redacted (Section 8.5) in every rendered output; the jar's in-memory state is never itself an output.

Without `cookies: true` (the default), no jar exists and the scenario is fully responsible for propagating any `Cookie` header itself via `headers` and `extract`.

Extraction stores response values for later steps:

```yaml
extract:
  userId:
    from: legacy.body               # legacy.body | new.body | response.body | legacy.headers | new.headers
    path: $.id
  etag:
    from: legacy.headers
    path: etag
  refreshToken:
    from: response.set_cookie       # legacy.set_cookie | new.set_cookie | response.set_cookie
    path: refresh_token             # the cookie NAME, not a JSONPath
```

For single-target modes (`new_only_assert`, `replay_against_recording`), `from: response.body` may be used.

**`*.set_cookie` sources.** Header-based extraction (`*.headers`) reads the lossy single-value `headers` map, so a source backed by the lossless `setCookie` capture (Section 9.2) is needed to extract a cookie's value — e.g. feeding a refresh cookie into a JSON body for a dual-source refresh scenario. For `legacy.set_cookie` / `new.set_cookie` / `response.set_cookie`, `path` is the cookie **name**, not a JSONPath, and the extracted value is that cookie's **value** parsed from the response's `setCookie` array. If the name appears more than once, the **last** occurrence wins (RFC 6265 semantics, matching the cookie jar's same-name resolution in Section 4.6). A name not present behaves like a missing header extraction: undefined, surfacing as the usual missing-variable failure downstream with a clear message. Attributes are never extracted this way — assert them via `expect.set_cookie` (Section 4.7). The existing `*.headers` sources keep their single-value semantics untouched, and extracted cookie values remain subject to the existing redaction surfaces (Section 8.5) wherever they appear in output.

### 4.7 Comparison strategies

The scenario chooses a **strategy**; the **behavioral normalization rules** come from the referenced contract (or inline as a fallback). Strategies:

**`exact`** — status, selected headers, and normalized body must match exactly.

**`json_semantic`** — JSON compared semantically, ignoring object key order; normalization applied first.

```yaml
compare:
  strategy: json_semantic
  status: same
```

**`subset`** — only specified paths must match.

```yaml
compare:
  strategy: subset
  body:
    require_matching_paths:
      - $.id
      - $.name
      - $.status
```

**`explicit_expectations`** — response compared to explicit values in the scenario (the primary strategy for `new_only_assert`).

```yaml
compare:
  strategy: explicit_expectations
  expect:
    status: 404
    headers:                        # exact match on named single-value headers
      x-frame-options: DENY
    header_absent: [x-forwarded-host]
    body:
      json_paths:
        $.error.code: USER_NOT_FOUND
        $.error.message: User not found
    set_cookie:                     # order-insensitive list of expected cookies
      - name: session
        value_present: true         # value asserted non-empty, not equal to a literal
        attributes:                 # exact match on listed attributes; unlisted = don't-care
          Path: /
          HttpOnly: true
        exact_attributes: false     # true: the cookie's full attribute set must equal this map
    location:
      path: /login                  # asserted parts only; omitted parts = don't-care
      query:
        error: access_denied
      query_present: [return_to]    # param must exist, any value
      query_absent: [client_secret]
```

`expect` fields are all optional and independently assertable:

- `status` — exact status code.
- `headers` — exact match on named single-value headers (case-insensitive names), read from the response's `headers` map. Naming `set-cookie` or `cookie` in `headers`, `header_absent`, or `header_present` is a **load-time validation error** — cookie assertions read the lossy single-value map otherwise, exactly the drift Section 9.2 exists to prevent; use `set_cookie` / `set_cookie_absent` instead. This is the same rule `compare_headers` enforces for `set-cookie` in Section 8.6.
- `header_absent` — header names that must **not** be present on the response. Same `set-cookie`/`cookie` restriction as `headers`, above.
- `header_present` — header names that must be present on the response with **any** non-empty value — the value itself is not asserted, only that it exists and is non-empty. For a header whose value is inherently dynamic (e.g. `Retry-After`, a countdown) this is the only assertion that makes sense. Same `set-cookie`/`cookie` restriction as `headers` and `header_absent`, above. A missing header, or one present with an empty value, is a mismatch of kind `header` — the same kind `headers` uses.
- `body.json_paths` — exact value at each JSONPath (Section 8.4 subset).
- `set_cookie` — expected cookies matched by `name` against the response's `setCookie` capture (Section 9.2). Matching is **one-sided and order-preserving**: each `set_cookie` entry consumes the **first not-yet-consumed** response cookie with that `name`, in response order — so two expected entries with the same name consume successive occurrences. Response cookies never consumed by an expectation are **not** an error (expectations assert, they don't exhaustively describe the response). Each entry asserts either an exact `value` or `value_present: true` (non-empty, value itself not checked) — declaring both is a validation error. `attributes` is compared case-insensitively on attribute names and exactly on attribute values, except that a **boolean** expected value asserts a flag attribute's presence (`HttpOnly: true`) or absence (`HttpOnly: false`) rather than its text; unlisted attributes are don't-care unless `exact_attributes: true`, in which case the cookie's full attribute set must equal the listed one. An expected cookie with no matching unconsumed response cookie is a mismatch. Neither the expected nor the actual cookie **value** is ever rendered into a mismatch, exactly as in Section 8.6.
- `set_cookie_absent` — cookie names that must have **no** `Set-Cookie` entry anywhere in the response's `setCookie` capture (e.g. proving a `401` does **not** also emit a delete-cookie for the session). Reads the same lossless capture as `set_cookie`, but asserts presence only — no value or attribute is checked, and there is nothing to consume-and-pair, so it is independent of any `set_cookie` block on the same step. A name present one or more times is a mismatch, reported as `set_cookie.presence` — the same kind `set_cookie` uses for a missing expected cookie, just in the opposite direction.
- `location` — parses the response's `location` header as a URL, resolving a relative `Location` against the request URL first (Section 8.6), and asserts the given parts; a part omitted from the block is don't-care. `path` is exact; `query` gives exact values for named params; `query_present` asserts named params exist (value free); `query_absent` asserts named params are absent. A response with no `Location` fails a `location` block as a presence mismatch, and one whose `Location` cannot be resolved fails as a raw mismatch without being rendered; query values under the secret-bearing parameter names (Section 8.5) are masked on **both** sides, expected included.

**Template substitution inside `expect`.** Every string value inside `headers`, `header_absent`, `header_present`, `body.json_paths`, `set_cookie` (`name`, `value`, and `attributes` values), `set_cookie_absent`, and `location` (`path`, `query` values, `query_present`, `query_absent`) is substituted with the same `{{ variables.x }}` / `{{ env.X }}` engine requests use (Section 7.1) — evaluated at **expectation time**, i.e. after every prior step's extractions and this step's own (extraction runs before comparison, Section 3.3). `expect.status` is the one exception: it is compared as an already-typed integer and is never templated. A whole-string template preserves the resolved value's type exactly as request substitution does (Section 7.1) — so a boolean `attributes` entry or a `body.json_paths` value can come from an extracted variable without being coerced to a string; an embedded template still stringifies as usual. This is what lets a scenario assert `$.identity_id` equals `{{ variables.identityId }}` extracted from an earlier step in the same flow.

`set_cookie` and `location` here reuse the **same** Set-Cookie/URL parsers as the two-sided `set_cookie`/`location` comparison blocks (Section 8.6) — one implementation, two consumers. Unlike Section 8.6, these are Pharos-only, one-sided assertions: Limen never asserts one-sided, so this vocabulary carries no lockstep obligation (Section 13); only the parsing semantics (including relative-Location resolution) are shared. The name-pairing rule differs deliberately from Section 8.6's two-sided *positional* pairing within a duplicate-name group: one-sided assertions consume in response order instead, since there is no second side to position against.

**`custom`** — service-specific comparison via a named comparator from the hook registry. The comparator always receives a **redacted view** of each response: configured `redact_paths`/`sensitiveHeaders` are masked as usual, and `set-cookie`/`cookie` are *unconditionally* masked in the header/`setCookie` view regardless of the scenario's `sensitiveHeaders` config — a custom comparator can never surface a raw cookie value into `diffText`, closing off the one path Section 8.5's "no secret value appears in any output" invariant can't enforce by construction alone.

```yaml
compare:
  strategy: custom
  comparator: compareDeviceList
  args:
    ignore_offline_timestamp: true
```

**Inline-rules fallback.** A scenario without a `contract` reference may declare normalization inline under `compare.body` / `compare.headers` — and the two comparison dimensions under `compare.set_cookie` / `compare.location` (Section 8.6) — using the **same vocabulary as the contract** (Section 8). If a scenario specifies **both** a `contract` reference **and** an inline behavioral block, that is a **validation error** — one source of behavioral truth per scenario.

```yaml
# inline fallback (no contract reference)
compare:
  strategy: json_semantic
  status: same
  headers:
    compare: [content-type]
    ignore: [date, x-request-id]
  body:
    ignore_paths:
      - $.metadata.requestId
      - $.metadata.generatedAt
    sort_arrays:
      - path: $.items
        key: id
```

### 4.8 Tags

Required list, used for filtering and reporting. Common tags:

```
read, write, smoke, regression, migration-ready, legacy-bug-compatible,
intentional-change, requires-auth, requires-fixtures, destructive
```

### 4.9 Hooks

Setup/cleanup at scenario level, before/after at step level; plus custom comparator and normalizer hooks. Loaded by name from the hook registry (Section 7).

```yaml
setup:
  hooks:
    - name: createTestUser
      args: { plan: standard }
      assign: { userId: id }        # merge hook output {id} into variables.userId

cleanup:
  hooks:
    - name: deleteTestUser
      args: { userId: '{{ variables.userId }}' }
```

### 4.10 Recording and replay specs

The two scenarios below share `id: users.replay-get-user-recording` and `stepId:
get-user` deliberately: the replay identity cross-check (Section 10.3) requires
a fixture's stamped `scenarioId`/`stepId` to match the scenario/step now
replaying it, so recording and replay are the same scenario with its `mode`
flipped after the fixture is captured, not two independently-named files.

**Recording mode:**

```yaml
version: 1
id: users.replay-get-user-recording
name: Record existing user behavior
service: user-service
tags: [read, recording]
mode: legacy_record
variables: { userId: user-123 }
steps:
  - id: get-user
    request:
      method: GET
      path: /users/{{ variables.userId }}
    recording:
      fixture: users/get-user-success/get-user.json
      safe_headers: [content-type]   # only these headers are recorded
```

**Replay mode:**

```yaml
version: 1
id: users.replay-get-user-recording
name: Replay existing user behavior against new service
service: user-service
tags: [read, regression]
mode: replay_against_recording
contract: "../contracts/user-service.contract.yaml#get-user"
steps:
  - id: get-user
    recording:
      fixture: users/get-user-success/get-user.json
    request:
      method: GET
      path: /users/user-123
    compare:
      strategy: json_semantic
      status: same
```

### 4.11 Multi-step example (destructive, guarded)

```yaml
version: 1
id: users.create-then-fetch
name: Create then fetch user
service: user-service
tags: [write, read, regression, destructive]
mode: compare_live
safety:
  destructive: true
  allowedEnvironments: [local, ci, staging]
contract: "../contracts/user-service.contract.yaml#create-user"
setup:
  hooks:
    - name: generateUserPayload
      assign: { userPayload: payload }
steps:
  - id: create-user
    request:
      method: POST
      path: /users
      headers: { authorization: Bearer {{ env.AUTH_TOKEN }} }
      body: '{{ variables.userPayload }}'
    extract:
      userId: { from: legacy.body, path: $.id }
    compare:
      strategy: json_semantic
      status: same
  - id: get-created-user
    request:
      method: GET
      path: /users/{{ variables.userId }}
      headers: { authorization: Bearer {{ env.AUTH_TOKEN }} }
    compare:
      strategy: subset
      body:
        require_matching_paths: [$.email, $.displayName, $.status]
cleanup:
  hooks:
    - name: deleteUser
      args: { userId: '{{ variables.userId }}' }
```

### 4.12 Agent-generated scenario guidelines

When an AI agent generates scenarios it should: prefer small focused scenarios; include a clear description; use tags consistently; mark destructive tests explicitly; avoid hardcoding secrets; add ignore rules only with a reason/comment; prefer semantic JSON comparison for JSON APIs; avoid over-normalizing meaningful differences; include both success and error cases; and include edge cases (empty lists, missing resources, invalid input, permission errors).

---

## 5. The Shared Behavioral Contract

> This section is intentionally consistent with the Limen specification. The contract format, the contract-vs-operational split, the JSONPath subset, and the normalization vocabulary are **identical** so a single contract file is portable between Pharos and Limen.

### 5.1 Purpose and lifecycle

The contract is the artifact that flows through the migration workflow and grows more trustworthy at each stage:

```
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

**Pharos is the refinement stage.** When a scenario fails because a rule over-normalized (hiding a real difference) or under-normalized (causing a false failure on a dynamic field), the engineer or agent adjusts the contract, reruns, and converges. The contract that emerges is what Limen consumes against production traffic.

### 5.2 Contract format

A contract file is YAML or JSON (Pharos and Limen both detect by extension):

```yaml
# contracts/device-service.contract.yaml
version: 1
service: device-service
description: >
  Behavioral contract for device-service migration. Drafted from OpenAPI +
  captured traffic, refined by Pharos runs, consumed by Limen for shadow comparison.

defaults:
  compare_status: true
  compare_body: true
  compare_headers: []
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
        precision: seconds
    enum_aliases:
      - path: "$.status"
        aliases: { ACTIVE: enabled, INACTIVE: disabled }
  set_cookie:                        # optional; omitted = not compared (Section 8.6)
    compare: true
    ignore_cookies: []
    ignore_attributes: []
    compare_values: exact            # exact | presence
  location:                          # optional; omitted = not compared (Section 8.6)
    compare: true
    ignore_query_params: []
    origin: exact                    # exact | ignore

routes:
  - id: "get-device"
    match:
      methods: ["GET"]     # GET | POST | PUT | PATCH | DELETE | OPTIONS | HEAD (Section 9.1)
      path_template: "/devices/{id}"
    comparison:
      json:
        ignore_paths: ["$.device.lastSeenAt"]   # merged with defaults
    expectations:
      typical_status: 200
      notes: >
        Legacy returns 200 with empty body on soft-deleted devices; new returns
        404. Intentional change — tag the scenario intentional-change.
    tags: [read, migration-ready]
```

`set_cookie` and `location` are legal at both `defaults` and per-route `comparison` levels, exactly like `json` — the route example above omits them purely for brevity. Their comparison semantics are defined in Section 8.6.

### 5.3 What lives in the contract vs. elsewhere

The contract owns **behavioral** comparison truth. Operational concerns live in each tool's own config (Pharos config / Limen route config). The key namespaces are distinct, so a merge is a **union, never a reconciliation**:

| Concern | Lives in | Used by |
|---|---|---|
| **What** to compare and **how** (`ignore_paths`, `redact_paths`, `sort_arrays`, `unordered_arrays`, `normalize_timestamps`, `enum_aliases`, `compare_status`, `compare_body`, `compare_headers`, `set_cookie`, `location`) | **Contract** | Both Pharos and Limen |
| Scenario structure, steps, hooks, modes, recording fixtures | **Pharos scenarios** | Pharos only |
| Rollout, upstreams, shadow sampling, circuit breaker, flags | **Limen route config** | Limen only |

### 5.4 How Pharos consumes the contract

- A scenario **references** a contract route via `contract: "path#routeId"`.
- At load time, Pharos resolves the reference and **merges** the contract's behavioral rules (service `defaults` + per-route `comparison`, per-route merging onto defaults) into the scenario's comparison configuration.
- **Merge semantics:** scalar-valued rules (e.g. `compare_status`, `set_cookie.compare_values`, `location.origin`) are a simple override — per-route wins over defaults. List-valued rules (`ignore_paths`, `redact_paths`, `set_cookie.ignore_cookies`, `location.ignore_query_params`, etc.) **concatenate defaults then per-route, then de-duplicate preserving first occurrence** — an entry present in both `defaults` and a route's `comparison` appears once in the resolved rule. This matches Limen's merge behavior (Section 13).
- The scenario's `strategy` (Section 4.7) decides *how* to compare; the contract decides *what to normalize/ignore/redact* before comparing.
- A scenario with **both** a contract reference and an inline behavioral block is a **validation error**.
- Contracts are loaded at run start; Pharos does not hot-reload them mid-run (consistency with Limen; a run uses fixed comparison semantics).

### 5.5 `check-contract` command

Pharos ships a `check-contract` command that validates a contract file against the schema and verifies every JSONPath expression is within the supported subset (Section 8.4). This lets the AI→Pharos→Limen loop confirm a freshly drafted contract is consumable by **both** tools before wiring it into scenarios or proxy routes. The command must produce the **same** verdict Limen's `check-contract` would, since both implement the identical subset.

---

## 6. Configuration

### 6.1 Sources and precedence

Layered, later overriding earlier: defaults < config file (`pharos.config.ts`/`.json`) < environment variables < CLI arguments.

### 6.2 Config contents

- `legacy_base_url`, `new_base_url`.
- `scenario_dir`, `contract_dir`, `fixture_dir`, `report_dir`.
- `default_timeout_ms`.
- `default_headers`.
- Auth token environment variable names.
- `environment` — `local | ci | staging | production`, default `local`. The safety-relevant environment this run targets; `safety.allowedEnvironments` (Section 4.5) is compared against this field. See Section 12 for the `production` fail-closed profile.
- `production_url_patterns` — optional list of host globs (e.g. `*.example.com`). Globs match **only** the **hostname** of each configured base URL (`legacy_base_url`, `new_base_url`) — lowercased, with no scheme, no port, and no path. If any configured base URL's hostname matches a pattern while `environment != production`, the run aborts with a config error before any request is issued (Section 12).
- Output mode (`local` | `ci`) — governs **reporting and recording** conventions only (Section 11, Section 10.2). Independent of `environment`: a production smoke run driven from CI legitimately wants CI-style reporting *and* the production safety profile at once.
- `allow_destructive_tests` (default false).
- `allow_recording_updates` (default false).
- Redaction targets (headers, JSON paths, query params).

Example environment variables:

```bash
LEGACY_BASE_URL=http://localhost:3001
NEW_BASE_URL=http://localhost:3002
SCENARIO_DIR=./scenarios
CONTRACT_DIR=./contracts
FIXTURE_DIR=./fixtures/recordings
REPORT_DIR=./reports
PHAROS_MODE=local
PHAROS_ENVIRONMENT=local
ALLOW_DESTRUCTIVE_TESTS=false
ALLOW_RECORDING_UPDATES=false
AUTH_TOKEN=...
```

### 6.3 Validation

The harness fails with an actionable error when required config for the selected mode is missing (e.g. `compare_live` requires both base URLs; `replay_against_recording` requires `fixture_dir`). `environment` and `production_url_patterns` are validated at config-load time, before any scenario runs (Section 12).

---

## 7. Variables and Hooks

### 7.1 Variable substitution

Template syntax with these namespaces:

```
{{ variables.userId }}   {{ env.AUTH_TOKEN }}
{{ random.uuid }}        {{ random.int }}
{{ now.iso }}            {{ now.epochMs }}
```

Substitution works in `path`, `query`, `headers`, and `body`. A missing variable fails with a clear message naming the variable and step. Sources: scenario `variables`, environment, built-ins, hook outputs, and values extracted from prior responses.

Its scope also reaches the `explicit_expectations` `expect` block (Section 4.7): every string leaf there except `expect.status` is substituted too, evaluated at expectation time — after this step's own extraction, so an assertion can reference a value the same step just captured, not only ones from earlier steps.

### 7.2 Hook registry

A `hooks/index.ts` exports named hooks; scenarios reference them by name; the harness imports the registry from a configured path.

```ts
export interface HookContext extends ScenarioContext {
  stepId?: string;
}
export type HookFn = (
  ctx: HookContext,
  args?: unknown
) => Promise<Record<string, unknown> | void>;

export const hooks = {
  createTestUser,
  deleteTestUser,
  authHeaders,
  normalizeDeviceCapabilities, // custom normalizer
  compareDeviceList,           // custom comparator
};
```

Hook outputs merge into scenario variables when an `assign` mapping is given. **Cleanup hooks run even when a step fails.** An unknown hook name fails clearly (at validation or execution).

---

## 8. Comparison and Normalization Engine

### 8.1 Inputs

Comparison receives: the legacy response, new response, or recorded response (per mode); the scenario comparison config (strategy); the merged contract normalization rules; and the redaction config.

### 8.2 Normalization (runs before comparison and before any diff)

All transforms are driven by the merged contract (or inline) rules and must be deterministic:

- Parse JSON; **canonical key order**.
- **Remove ignored JSON paths** (`ignore_paths`).
- **Redact configured JSON paths** for output (`redact_paths`).
- **Sort arrays** by key (`sort_arrays`).
- **Treat configured arrays as unordered sets** (`unordered_arrays`).
- **Normalize timestamps** to a configured precision (`normalize_timestamps`). The precision field accepts both `milliseconds` and `millis` (Limen's historical spelling) — a deliberate lockstep accommodation (Section 13); `milliseconds` is canonical and what tooling documents/emits.
- **Map enum aliases** (`enum_aliases`).
- Apply **custom normalizers** by name (from the hook registry).

### 8.3 Comparison result model

```ts
export type ComparisonStrategy =
  | 'exact' | 'json_semantic' | 'subset' | 'explicit_expectations' | 'custom';

export interface Mismatch {
  path: string;
  kind:
    | 'status' | 'header' | 'body' | 'missing' | 'extra' | 'type' | 'value' | 'custom'
    // the two opt-in dimensions of Section 8.6
    | 'set_cookie.presence' | 'set_cookie.value' | 'set_cookie.attribute' | 'set_cookie.malformed'
    | 'location.presence' | 'location.origin' | 'location.path' | 'location.query' | 'location.raw';
  expected?: unknown;
  actual?: unknown;
  message: string;
}

export interface ComparisonResult {
  pass: boolean;
  summary: string;
  mismatches: Mismatch[];
  diffText?: string;
  diffTruncated?: boolean;    // a bounded mismatch list was clipped (Section 8.6)
}
```

**Engine-neutral mismatch kinds.** The cross-engine decision table (Section 13) records a result as a **sorted, de-duplicated set** of kind strings: `status`, `body` (which the body-level kinds `missing`/`extra`/`type`/`value` collapse into), `header`, `set_cookie.<kind>`, and `location.<kind>`. It is a set, deliberately order-independent: the two engines must agree on *which* mismatches exist, not on the order in which they find them.

### 8.4 Supported JSONPath subset (hard MVP boundary)

Identical to Limen's, so contracts are portable:

- `$.field`
- `$.nested.field`
- `$.items[*].field` (wildcard over array elements)

Anything outside this subset is a **validation error** at scenario/contract load time. The subset may expand later, **in lockstep** with Limen.

### 8.5 Redaction scope

Applies to console logs, JSON reports, JUnit reports, failure artifacts, and recordings. Configurable targets: header names (`authorization`, `cookie`, `x-api-key`), JSON paths (`$.token`, `$.password`, `$.user.email`), query parameters (`access_token`). **No secret value appears in any output.** A test proves it (Section 16).

**Sensitivity propagation.** Name- and path-based targets mask a secret where it is *declared*; they cannot help once a value has been **extracted into a variable** and substituted somewhere else — a session cookie captured by `extract` (Section 4.6) and later sent as a JSON body field, a custom header, or a form value is, to every static list, an ordinary value. So the run also tracks the *values themselves*: every value extracted from a `*.set_cookie` or `*.headers` source registers as sensitive automatically (no opt-out — `sensitive: false` beside those sources is a load-time error), and a body extraction registers when the rule declares `sensitive: true`. The registry is scenario-scoped, created and discarded with the variable store it sits beside. Masking is by value at the boundaries where data leaves execution — mismatches (before the diff text is rendered, so a bounded preview cannot leak a truncated prefix), step and lifecycle error strings, failure artifacts, recordings, and the view a custom comparator is handed — and is applied structurally, before serialization, so an encoder cannot escape a value out of reach. Substitution itself is untouched: the wire still carries the real credential. A whole value is masked at any length; a value embedded in a larger string is replaced only from eight characters up (the over-masking guard), and the replacement names the variable (`[REDACTED:<name>]`), the first-registered one when several share a value. An extracted object or array registers every scalar leaf, so a credential bundle cannot hide its tokens behind a container. Percent- and form-encoded forms of a registered value are masked too, so a secret that reached a URL query or a urlencoded body cannot survive its encoding. This is deliberately **not** taint tracking through substitution.

Two bounds are stated rather than solved. **A value shorter than eight characters is masked only where it stands alone** — an occurrence inside a larger string (`Bearer abc123`) survives, because replacing so short a string everywhere would corrupt unrelated output; registration emits a warning naming the variable (never the value) so the residual is visible rather than assumed away. And **hook code is a trust boundary**: hooks receive the raw variable store by design, so this invariant covers Pharos's own output surfaces — what a hook itself prints or ships is the hook author's responsibility.

### 8.6 Set-Cookie and Location comparison

Two additional, **optional** comparison dimensions, read from `HttpResponseRecord.setCookie` (Section 9.2) and the `location` response header — not from the single-value `headers` map that `compare_headers` uses. These are new dimensions layered onto the engine, not an extension of `compare_headers`, and the two are **asymmetric** about it:

- **Listing `set-cookie` in `compare_headers` is always a load-time validation error**, block or no block. The generic header path compares one value per name, so a multi-cookie response silently loses all but the last — comparing cookies that way is never right, and the `set_cookie` block is the only correct tool.
- **Listing `location` in `compare_headers` is a load-time validation error only while a `location` block is present** (the block wins conceptually; the error keeps intent unambiguous). `Location` is genuinely single-valued, so the generic path compares it faithfully and listing it on its own stays legal.

```yaml
# both blocks optional; omitted = today's behavior (not compared)
set_cookie:
  compare: true                # master switch
  ignore_cookies: []           # cookie names excluded entirely
  ignore_attributes: []        # e.g. [expires] — clock-dependent attributes
  compare_values: exact        # exact | presence  (presence: name + attributes only)
location:
  compare: true
  ignore_query_params: []      # e.g. [state, nonce, code]
  origin: exact                # exact | ignore  (ignore: compares path + remaining query only)
```

**Nested defaults (normative, lockstep):** every field inside `set_cookie`/`location` is optional. A block that is **present but empty** (`set_cookie: {}`) is valid and resolves to `{compare: true, ignore_cookies: [], ignore_attributes: [], compare_values: exact}`; likewise `location: {}` resolves to `{compare: true, ignore_query_params: [], origin: exact}`. A block that is **absent** means the dimension is not compared at all — there is no implicit default block. Both engines must resolve empty blocks to exactly these values.

**`set_cookie` semantics:** each side's `setCookie` array is parsed into `(name, value, attribute map)` tuples. Cookies are paired across sides **by name**; duplicate names on one side pair **positionally** within the name group. Attribute names are compared case-insensitively; attribute values are compared exactly, except for attributes listed in `ignore_attributes`. A cookie present on one side only is a mismatch. `compare_values: presence` compares only that a value exists on both sides (plus the attribute map) without comparing the value itself; `exact` also compares the value.

**`location` semantics:** the `location` header is parsed as a URL on both sides. A **relative** `Location` value is first resolved against the URL of the request that produced the response (RFC 9110 §10.2.2) — the same resolution a browser would perform — before any parts are compared; only if that resolution itself fails does the exact-string fallback below apply. Query params named in `ignore_query_params` are removed from both sides before comparing. `origin: exact` compares scheme+host+port as well as path and remaining query; `origin: ignore` compares only path and remaining query — for cases where legacy and new intentionally redirect to different hosts for the same logical destination. The `expect.location` assertion (Section 4.7) resolves relative Locations the same way.

**Both:** a value that fails to parse (a malformed Set-Cookie, or a Location that cannot be resolved even relative to the request URL) falls back to **exact string comparison** and counts as a mismatch if the sides differ. A `Location` that resolves successfully is always compared part-wise, never as a raw string. Redaction (Section 8.5) still applies to rendered values — a dedicated test proves a `set_cookie` mismatch never renders a raw cookie value (name and attribute diff only, per the no-secret-value invariant in Section 12).

**Comparison details (normative, lockstep).** Both engines resolved these while implementing the dimensions; they are as binding as the field names:

- **Case sensitivity.** Cookie names — and therefore `ignore_cookies` — are compared **case-sensitively** (RFC 6265). Cookie *attribute* names — and therefore `ignore_attributes` — are compared **ASCII-case-insensitively**; attribute *values* are compared exactly. Query parameter names — and therefore `ignore_query_params` — are compared case-sensitively.
- **Malformed Set-Cookie** means the name/value pair has no `=`, or the name is empty (the values RFC 6265 §5.2 discards). Unparseable entries are paired with each other **positionally**, never with parsed cookies, and take the exact-string fallback. A duplicated attribute inside one `Set-Cookie` keeps its **last** occurrence, as RFC 6265 §5.2 prescribes.
- **`compare_values: presence`** compares only whether the two sides *agree* that a value exists: an empty value counts as no value, so `sid=` against `sid=abc` is a value mismatch, while `sid=` on **both** sides matches — that is the cookie-deletion shape (`session=; Max-Age=0`) legacy and new both emit on logout, and it is agreement, not a failure.
- **Location query.** After `ignore_query_params` removal, the remaining query is compared as a `name -> values` map, so parameter **order never matters**; repeated names compare as an ordered list of values.
- **Location parts.** `origin: exact` compares the `(scheme, host, effective port)` triple and nothing more — *effective* port, so `https://a` and `https://a:443` are one origin. It is computed from those three parts explicitly rather than from a URL library's `origin` accessor: JavaScript's `URL.origin` reports the string `"null"` for non-special schemes, and Rust's `Url::origin` returns an opaque, never-equal origin for them — either would make two identical `mailto:` Locations mismatch, and they disagree with each other besides. Neither mode compares the URL **fragment** or **userinfo**, which are outside the enumerated parts.
- **Rendering.** A cookie value is never rendered — a value difference shows the redaction marker (an `<empty>` equivalent when the value is empty), a one-sided cookie shows a `<present>` equivalent, and an unparseable entry is redacted wholesale because it cannot be masked selectively. Attribute values, `Location` origins, and paths are rendered verbatim; `Location` query values are masked for the standard secret-bearing parameter names (Section 8.5) — which include the OAuth authorization `code`. A rendered `Location` is origin + path only, so a `user:password@` userinfo is never emitted, and an unresolvable `Location` is redacted for the same reason as an unparseable cookie. (The marker *text* is each tool's own — Pharos writes `***REDACTED***` — only the verdicts are lockstep.)
- **Bounds.** The cookie and `Location` mismatch lists are each capped at `MAX_DIFFERENCES` (100, Limen's `max_differences` default), and `ComparisonResult.diffTruncated` says a list was clipped — no single response can grow an unbounded log line.

**Lockstep:** this vocabulary — field names, parsing (including relative-Location resolution and empty-block defaults), merge (Section 5.4), and validation semantics — must remain **identical** between Pharos and Limen (Section 13), the same obligation as the JSONPath subset (Section 8.4). The shared fixture in `tests/fixtures/lockstep/` (a byte-identical twin of the Limen copy) plus its `decisions.json` table pin the resolution rules in both engines: `merge_cases` pins contract resolution and `verdict_cases` pins the comparison itself (a response pair and its rules resolve to one verdict plus a **set** of mismatch kinds, Section 8.3).

**Where the dimensions apply.** They are two-sided comparisons, so a declared block is compared under every two-sided strategy — `exact`, `json_semantic`, and `subset`. `explicit_expectations` has only one response and asserts through the one-sided `expect` vocabulary instead (Section 4.7); `custom` comparators own their verdict entirely. Both blocks are legal in the contract (`defaults` and per-route `comparison`) **and** in a scenario's inline `compare` block, in the same vocabulary — and, like every other behavioral rule, declaring one inline while the scenario also references a contract is a validation error (Section 5.4).

The one-sided `expect.set_cookie` / `expect.location` assertion vocabulary (Section 4.7) reuses these same parsers; that vocabulary is Pharos-only and carries no lockstep obligation of its own.

---

## 9. HTTP Client

The client must:

- Build absolute URLs from base URL + request spec.
- Support GET/POST/PUT/PATCH/DELETE/OPTIONS/HEAD, path templates, query params, headers, JSON bodies, form-encoded bodies, and plain-text bodies as a fallback.
- Apply default headers from config and per-request timeout (with optional, default-off retries for transient failures).
- Capture status, headers, multi-value `Set-Cookie`, body text, parsed JSON (when possible), duration, and errors.
- Redact sensitive headers in logs.

### 9.1 Methods

OPTIONS and HEAD are supported alongside GET/POST/PUT/PATCH/DELETE — required for CORS-preflight and HEAD scenarios. Both **forbid** a request `body` and `form`: this is a `fetch`/HTTP quirk (bodies on these methods are unreliable or meaningless across implementations), not a Pharos-specific restriction. A scenario specifying `body` or `form` on an OPTIONS or HEAD step is a **validation error** at load time.

### 9.2 Multi-value headers: `setCookie`

`Headers.getSetCookie()` captures every `Set-Cookie` header **losslessly**, independent of the single-value `headers` map — per the `Headers` API, plain iteration/`get` on `headers` exposes only the **last** `Set-Cookie` value, silently dropping the rest. `HttpResponseRecord` gains a `setCookie: string[]` field (empty array when the response sets no cookies); the existing `headers: Record<string, string>` is unchanged and must not be used for Set-Cookie inspection.

### 9.3 Redirects: `follow_redirects`

Requests gain a `follow_redirects: boolean` field, **default `true`** (today's behavior — `fetch`'s default `redirect: 'follow'`). When `false`, the client sets `redirect: 'manual'`; the 30x response itself (status plus `location` header) becomes the step's response — extractable and comparable like any other response.

**Spelling.** `follow_redirects` is the **on-disk** scenario field (snake_case, the portable vocabulary of Section 4.6); the **in-memory** `HttpRequestSpec` field is `followRedirects`, camelCase like every other in-memory field (`bodyText`, `durationMs`, `setCookie`). The step runner maps `follow_redirects` → `followRedirects` when it resolves a step's request, the same explicit mapping the recording writer/reader performs for `setCookie` ⇄ `set_cookie` (Section 9.2). `form`, `query`, `headers`, and `body` need no mapping — their names are identical on both sides.

**Pitfall:** with `follow_redirects: true`, intermediate 30x hops are **invisible** to both the cookie jar (Section 9.5) and comparison/extraction — `fetch` follows them internally and only the final response is observed. A flow that needs to inspect, extract from, or apply cookies set by an intermediate hop **must** set `follow_redirects: false` on that step and walk the chain manually, one step per hop, replaying the extracted `Location` as the next step's `path` (Section 9.4). This is the common shape of an OAuth-style authorize redirect chain, and it is the most likely authoring mistake — the scaffold README (Section 19.2) repeats this warning.

### 9.4 Absolute-URL paths and cross-origin replay

A step's `request.path` is normally relative to the target's configured base URL. It **may** instead be an absolute URL (e.g. `{{ variables.nextHop }}`, a `Location` extracted from a prior manual-redirect step) **iff**, after template substitution, its origin equals the step's target base URL origin. This is what lets a scenario replay an extracted `Location` as the next request in a redirect chain without hand-rebuilding it as a relative path. A cross-origin absolute path is a **runtime error** naming both the requested origin and the target's configured origin. Cross-origin Locations (e.g. the final client `redirect_uri`) are never fetched — they are asserted on via `expect.location` (Section 4.7) or the `location` comparison block (Section 8.6).

### 9.5 Cookie jar

See "Cookie jar (`cookies: true`)" in Section 4.6 for the scenario-level opt-in, the (name, path) keying, and most-specific-path-first send ordering. The client itself is stateless per request; the jar lives in the execution layer (`src/execution/cookies.ts`) and injects/reads `Cookie`/`Set-Cookie` around each call. An explicit `Cookie` header on a step's `request.headers` **replaces** the jar-built `Cookie` header for that request entirely — the jar is not consulted for sending on that request — but the jar still ingests `Set-Cookie` from that request's response as usual. A `Cookie` in the config's `default_headers` is **not** such an override: a jar-built header replaces it (Section 4.6); only a step's own header replaces the jar.

### 9.6 Form bodies

A step may set `form: Record<string, string | number | boolean>` instead of `body`. The client urlencodes it (`application/x-www-form-urlencoded`, unless a `content-type` header is already set) and sends it as the request body. `form` and `body` are **mutually exclusive** on a request — specifying both is a validation error. `form` is also forbidden on `GET` (a validation error, like OPTIONS/HEAD in Section 9.1) since a GET form has no meaning; `body` on `GET` is left as-is (silently ignored by the client).

Internal models:

```ts
export type HttpMethod =
  | 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE' | 'OPTIONS' | 'HEAD';

export interface HttpRequestSpec {
  method: HttpMethod;
  path: string;
  query?: Record<string, string | number | boolean | null>;
  headers?: Record<string, string>;
  body?: unknown;
  form?: Record<string, string | number | boolean>;   // mutually exclusive with body (Section 9.6)
  followRedirects?: boolean;       // default true; on-disk spelling is follow_redirects (Section 9.3)
  timeoutMs?: number;
}

export interface HttpResponseRecord {
  status: number;
  headers: Record<string, string>;
  setCookie: string[];             // every Set-Cookie header, losslessly (Section 9.2)
  bodyText: string;
  bodyJson?: unknown;
  durationMs: number;
  error?: { type: string; message: string };
}
```

The in-memory `HttpResponseRecord.setCookie: string[]` field is **required** (an empty array when the response sets no cookies — never absent). The **on-disk** recording field is the differently-shaped `recordingResponseSchema.set_cookie: string[]` (Section 10.1): snake_case, mirroring the recording format's on-disk convention, and **optional**. Absent on disk means "no cookie data was captured; cookie comparison is unavailable for this fixture" — true for every recording made before this change. The recording writer/reader map explicitly between the two shapes (`setCookie` ⇄ `set_cookie`) rather than sharing one schema; this is not an error case, just a documented limitation of pre-existing fixtures. Re-record to add cookie data to an old fixture.

---

## 10. Recording and Replay

### 10.1 Recording format

Recordings are JSON, even though scenarios are YAML:

```ts
export interface Recording {
  version: 1;
  scenarioId: string;
  stepId: string;
  recordedAt: string;
  environment?: string;
  request: HttpRequestSpec;
  response: HttpResponseRecord;
  metadata?: Record<string, unknown>;
}
```

Example path: `fixtures/recordings/users/get-user-success/get-user.json`.

`response` is validated on disk against `recordingResponseSchema`, not `HttpResponseRecord` directly: the on-disk `set_cookie` field is optional and snake_case, versus the in-memory `setCookie`, which is required. See Section 9's `HttpResponseRecord` model for the full required-vs-optional mapping and why pre-existing recordings have no cookie data.

The recorded **request** is informational — replay re-sends the scenario's freshly substituted request, never the recorded one (Section 10.3) — so `form` and `followRedirects` are **not** persisted into a recording: form values routinely carry credentials the redaction discipline (Section 10.2) has no rule for, and the redirect mode is a send-time knob with nothing to replay against.

### 10.2 Recording safety

- Writes require an explicit `record` command **or** `ALLOW_RECORDING_UPDATES=true`.
- **CI refuses recording updates by default.**
- Secrets are redacted **before** a recording is written.
- Scenarios declare which headers are safe to record (`safe_headers`); only those are persisted.

### 10.3 Replay behavior

Replay loads the recording for its **response** only — the legacy side of the comparison — and sends the step's own request, freshly variable-substituted so it carries current auth, to the new service; both responses are then normalized and compared. The recorded request is never replayed: it is redacted on disk (Section 10.2), and its path serves one purpose only, as the base a relative recorded `Location` resolves against (below). A missing or invalid fixture fails clearly. So does a recording whose `scenarioId`/`stepId` don't match the scenario/step now replaying it — a step execution failure naming the fixture path, the expected (running) ids, and the actual (recorded) ids. No escape hatch: re-record under the correct scenario/step.

**Relative `Location` in a recorded response** resolves against the **recorded** request's path (joined to `legacy_base_url`), never the live step's — the recorded response is the answer to the recorded request, and a parameterized replay may send a different path entirely. With no `legacy_base_url` configured (replay does not require one), or a recorded path that does not resolve against it, there is no base and the `location` comparison takes its exact-string fallback (Section 8.6).

---

## 11. Reporting

### 11.1 Console reporter (developer mode)

Prints: total scenarios run; pass/fail/skip counts; failed scenario IDs and names; per-step failure summary; status/header/body diffs; normalization rules applied; artifact paths; and suggested debugging pointers where possible.

### 11.2 JSON report (CI)

```ts
export interface TestRunReport {
  startedAt: string;
  finishedAt: string;
  durationMs: number;
  summary: { total: number; passed: number; failed: number; skipped: number };
  scenarios: ScenarioResult[];
}
```

Written to `report_dir`.

### 11.3 JUnit report

JUnit XML written for CI-native integration.

### 11.4 Failure artifacts

For each failed comparison, write **redacted** artifacts:

```text
reports/artifacts/<scenario-id>/<step-id>/legacy-response.json
reports/artifacts/<scenario-id>/<step-id>/new-response.json
reports/artifacts/<scenario-id>/<step-id>/diff.txt
reports/artifacts/<scenario-id>/<step-id>/request.json
```

### 11.5 Exit codes

Exit `1` when any required scenario fails; exit `0` when all selected required scenarios pass. A **skipped** scenario increments only the `skipped` counter — **never** `passed` — and never fails the run by itself; it is reported separately and is not counted as coverage. A **refusal** (Section 12) is reported as a failing scenario result, distinct from a skip, and behaves like any other failure for exit-code purposes.

---

## 12. Safety Controls

- **Destructive scenarios require explicit opt-in** (`ALLOW_DESTRUCTIVE_TESTS=true` or a CLI flag).
- **Production-like environment destructive runs require an additional guard override** (`requiresProductionGuardOverride`).
- **Recording updates require explicit opt-in** and are refused in CI by default.
- **Secrets are redacted** in all logs, reports, artifacts, and recordings; no secret is written to a fixture or report.
- **Fail fast** when required base URLs (or other mode-required config) are missing.
- **Environment guardrails:** refuse destructive tests against disallowed environments unless explicitly allowed.
- **`environment: production` is fail-closed.** A scenario runs only if `safety.allowedEnvironments` explicitly includes `production`; every other scenario in a production run is a **refusal**, not a skip — a distinct failing result (`pass: false`, with the rendered reason) that contributes to a non-zero exit code (Section 11.5). This is the counterpart to the permissive skip behavior in `local`/`ci`/`staging` (an environment-mismatched scenario there is a skip — counted only under `skipped`, never `passed`, and never failing the run by itself): production reverses the default, because a silently-skipped destructive scenario in production is exactly the failure mode this profile exists to prevent. Destructive-scenario opt-in and the production guard override (above) still apply on top of the refusal check — the gates compose.
- **`production_url_patterns`** (Section 6.2): globs match **only** the **hostname** of each configured base URL — lowercased, no scheme, no port, no path — so the representation being matched is unambiguous. If any configured base URL's hostname matches a pattern while `environment != production`, the run aborts with a config error **before any request is issued**. The config file's `environment` and `production_url_patterns` together are the declared trust boundary for "is this run allowed to touch production."

---

## 13. Relationship to Limen (Migration Proxy)

- **Separate repositories, no build-time dependency.** Either project builds and runs alone.
- **The shared behavioral contract (Section 5) is the integration point** — a documented schema and vocabulary, *not* shared code in the MVP.
- **One vocabulary, one JSONPath subset.** Field names, normalization semantics, and the supported JSONPath subset are identical, enforced by Section 8.4 and the shared contract format — including the `set_cookie`/`location` comparison blocks (Section 8.6) and the dual `milliseconds`/`millis` timestamp-precision spelling (Section 8.2). `check-contract` in both tools must agree.
- **CI-enforced, not just documented.** The `lockstep-twin` job (`.github/workflows/ci.yml`) fetches Limen main's copy of the shared fixture (`tests/fixtures/lockstep/{lockstep.contract.yaml,decisions.json}`, Section 8.6) and byte-compares it against Pharos's own — any drift fails the build. A missing copy on Limen main (not yet merged there) is tolerated as bootstrap, but only once the job has confirmed the token can actually see the Limen repo, so an access failure is never mistaken for bootstrap.
- **Workflow:** AI drafts a contract → **Pharos validates and refines** it deterministically (catching over- and under-normalization) → **Limen consumes** the refined contract unchanged for production shadow comparison and rollout.
- **Readiness signals (bidirectional):** passing Pharos scenarios for a route is a precondition for enabling Limen shadow mode; a clean Limen shadow mismatch rate is a precondition for raising rollout; **Limen-observed mismatches become new Pharos scenarios**, closing the loop.
- **Deferred:** a shared `normalization` package extracted from both could later replace the shared-contract-by-schema approach. Designed toward, not built now.

---

## 14. Phased Build Plan

Each phase ends with passing harness tests and a runnable CLI. Build incrementally and run tests after each major component.

### Phase 0 — Scaffold
- TypeScript + Vitest project; module skeleton (Section 3.5); package scripts; CI (typecheck + test + lint); README stub; license.
- CLI shell with `run`, `validate`, `record`, `check-contract`.
- **Done when:** `bun run test` (empty) passes; typecheck and lint pass in CI.

### Phase 1 — Scenario + contract loading and validation
- Zod schemas for scenarios and the contract; discovery via `fast-glob`; `validate` and `check-contract`; reference resolution and merge; JSONPath-subset enforcement; the contract-vs-inline conflict rule.
- **Done when:** valid scenarios/contracts load; invalid ones fail with file + field path; the validation matrix (Section 16.1) passes.

### Phase 2 — Config + HTTP client
- Layered config; mode-aware config validation; `fetch`/`undici` client with default headers, timeouts, JSON + text bodies, error/duration capture.
- **Done when:** HTTP client unit tests pass against a local mock server (path/query/headers/body, JSON parse, text fallback, timeout-as-error, duration).

### Phase 3 — Comparison + normalization
- All strategies (`exact`, `json_semantic`, `subset`, `explicit_expectations`, `custom`); all normalization transforms; readable JSON diff; redaction.
- **Done when:** comparison and normalization unit tests pass (Section 16.1), including the redaction-in-diff test.

### Phase 4 — Execution: compare_live + new_only_assert
- Scenario runner, step runner, scenario context, variable substitution + extraction, stop-on-first-failure, artifact writing.
- **Done when:** `compare_live` and `new_only_assert` run end-to-end against mock endpoints; multi-step variable extraction works; artifacts are written and redacted.

### Phase 5 — Hooks
- Hook registry loading; setup/cleanup; step before/after; custom comparator/normalizer wiring; cleanup-on-failure.
- **Done when:** hook tests pass — setup assigns variables; cleanup runs after success **and** after failure; unknown hook fails clearly.

### Phase 6 — Recording + replay
- Recording writer (opt-in, redacted, `safe_headers`); `legacy_record` and `replay_against_recording` modes; fixture load/validate.
- **Done when:** recording tests pass — record legacy; fixture schema correct; secrets redacted; update refused unless enabled; replay loads + compares; missing fixture fails clearly.

### Phase 7 — Reporting + CLI filtering + exit codes
- Console reporter; JSON report; JUnit report; failure artifacts; CLI filters (`--scenario`, `--include-tag`, `--exclude-tag`); exit codes.
- **Done when:** CLI tests pass — single-scenario run; tag include/exclude; `validate` catches invalid scenarios; non-zero exit on failure, zero on pass; reports written.

### Phase 8 — Example service, example scenarios, docs
- Mock endpoints (Section 15.1); the required example scenarios (Section 15.2) and example contract; README that runs from a fresh checkout.
- **Done when:** example scenarios validate and run against the mock endpoints; README instructions verified from a clean checkout.

### Post-Phase 8 increments

Work that landed after the MVP phases above. Each increment adds a dimension or a guard to the existing pipeline (Section 3.3) rather than reshaping it, and each is specified in place — the rows point at the section that owns the increment, they do not restate it.

| Increment | Specified in |
|---|---|
| Per-target cookie jar (`cookies: true`) | Section 9.5; scenario opt-in in Section 4.6 |
| `set_cookie` and `location` comparison dimensions | Section 8.6 |
| The one-sided `expect` vocabulary (`header_present`/`header_absent`, `set_cookie`/`set_cookie_absent`, `location`) | Section 4.7 |
| The `environment` model and the `production_url_patterns` guard | Section 12; fields in Section 6.2 |
| Packaging as a pinned git dependency and `pharos init` scaffolding | Section 19 |
| The `lockstep-twin` CI job byte-comparing the shared fixture against Limen main | Section 13 |

---

## 15. Example Service and Scenarios

### 15.1 Example endpoints (implement or mock)

- `GET /users/:id`
- `GET /users`
- `POST /users`
- `GET /users/:id` not-found case
- `POST /users` validation-error case
- `POST /login` — accepts any JSON credentials fixture; sets a `session`
  cookie (spec Sections 4.6, 8.6, and 9.5) whose value differs per instance
  and whose `SameSite` attribute differs cosmetically per instance.
- `GET /profile` — 200 with a small JSON body given a valid session cookie,
  else 401.
- `GET /users/find?name=` — a redirect endpoint: 303 with a **relative**
  `Location` header to the matching user, so the `location` contract
  dimension's `origin: ignore` (Section 8.6) is load-bearing across the two
  instances' different origins.

### 15.2 Required example scenarios

1. `users.get-user-success` — successful GET comparison.
2. `users.get-user-not-found` — 404/not-found comparison.
3. `users.create-user-validation-error` — validation-error comparison.
4. `users.list-users-sort-array` — list endpoint with array sorting.
5. `users.replay-get-user-recording` — replay against a recording.
6. `users.new-only-healthcheck` — new-only explicit assertion.
7. `users.create-then-fetch-destructive` — multi-step create/read/delete flow guarded as destructive.
8. `users.session-login-profile` — `cookies: true` jar flow: a login compared
   two-sided through the contract's `set_cookie` dimension (presence-level
   value, `SameSite` ignored), a second login asserted one-sided via
   `expect.set_cookie`, and a profile read asserted with an explicit
   `expect.status` (a two-sided "same" status would let a broken jar's
   identical 401s pass unnoticed).
9. `users.find-user-redirect` — a `follow_redirects: false` redirect compared
   two-sided through the contract's `location` dimension (`origin: ignore`)
   and, in a second step, asserted one-sided via `expect.location`.

Plus an example scenario demonstrating **ignored dynamic response fields** (may be folded into #1 via the contract).

---

## 16. Acceptance Criteria and Test Plan

### 16.1 MVP acceptance criteria

**Project scaffold:** TypeScript + Vitest; clear bun scripts for validation (`bun run validate`), execution, and harness tests; example scenarios; README.

**Scenario loading & validation:** loads YAML (and JSON) from `scenario_dir`; validates required fields; rejects unknown/invalid enum values; reports errors with file path **and** field path; selects by ID and tag.

**Contract:** loads YAML/JSON; reference resolves; behavioral rules merge (defaults + per-route, per-route overriding); contract-vs-inline conflict rejected; `check-contract` validates schema and JSONPath subset and agrees with Limen.

**Configuration:** legacy/new base URLs from env or file; scenario/contract/fixture/report dirs configurable; actionable error if required config missing for the selected mode; local and CI output modes; `environment` configurable independently of output mode.

**HTTP execution:** GET/POST/PUT/PATCH/DELETE/OPTIONS/HEAD (OPTIONS/HEAD refuse a `body`/`form`); path templates, query, headers, JSON/form/plain-text bodies; default headers; per-request timeout; `follow_redirects: false` exposes the 30x response and its `location` header instead of following it; absolute-URL paths allowed when same-origin as the target (cross-origin is a runtime error naming both origins); captures status, headers, losslessly-captured multi-value `setCookie`, body text, parsed JSON, duration, errors.

**Cookie jar:** scenario `cookies: true` opens an independent jar per target; `Set-Cookie` from every step response (including manual-redirect 30x responses) populates it, keyed by (name, path) with last-write-wins per key; subsequent requests to the same target send the matching entries most-specific-path-first unless the step sets an explicit `Cookie` header (which replaces the jar's header for sending only); jars never leak across scenarios; cookie values remain redacted in all output.

**`compare_live`:** calls both services; compares per strategy; per-step pass/fail; failure artifacts written.

**Set-Cookie/Location comparison:** `set_cookie`/`location` contract blocks (Section 8.6) parse, merge (list fields concatenate-then-dedup, Section 5.4), and compare per their documented semantics; listing `set-cookie` in `compare_headers` is a load-time validation error on its own, and listing `location` is one while a `location` block is present; a dedicated test proves no raw cookie value renders in a `set_cookie` mismatch; both `milliseconds` and `millis` timestamp-precision spellings are accepted.

**Expectation vocabulary:** `explicit_expectations`/`new_only_assert` `expect` supports `headers`, `header_absent`, `set_cookie` (name/value/value_present/attributes/exact_attributes), and `location` (path/query/query_present/query_absent), reusing the Section 8.6 parsers.

**`legacy_record`:** calls legacy only; writes fixture only when recording enabled; redacts secrets before writing; refuses recording updates in CI by default.

**`replay_against_recording`:** loads recorded legacy response; calls new; normalizes both; compares; fails clearly on missing/invalid fixture.

**`new_only_assert`:** calls new only; supports explicit expectations for status and JSON paths.

**Normalization & diffing:** ignore paths; ignore headers; semantic JSON (key-order-independent); sort arrays by key; readable JSON diffs; no secrets in diffs.

**Hooks & variables:** scenario variables; env substitution; built-ins (UUID, timestamp); setup/cleanup from the registry; extraction into later steps; cleanup runs after a failed step.

**Reporting:** useful console output; JSON report written; exit `1` on required-scenario failure, `0` on pass; skipped reported separately.

**Safety:** destructive scenarios require opt-in; production-like destructive runs require an additional guard override; auth headers and configured secret fields redacted; fixtures cannot be written accidentally; `environment: production` refuses (fails, does not skip) any scenario not tagged with `allowedEnvironments` including `production`, contributing to a non-zero exit; `production_url_patterns` aborts a run with a config error before any request when a configured base URL matches a production host pattern outside `environment: production`.

**Scaffolding:** `pharos init` produces a tree that `validate` passes on unmodified; rerunning without `--force` refuses to overwrite and names the conflicting files; generated hooks import from the `pharos` package name, not a relative path into Pharos's own source.

### 16.2 Harness test plan (Vitest)

These validate the framework itself.

**Scenario schema:** valid minimal scenario passes; missing ID fails; invalid mode fails; missing request method fails; unknown comparison strategy fails; destructive scenario without safety block fails/warns per policy; YAML parse error gives a useful file path.

**Variable substitution:** `{{ variables.userId }}` resolves; `{{ env.AUTH_TOKEN }}` resolves; missing variable fails with a clear message; random UUID is valid; substitution works in path, query, headers, body.

**HTTP client (mock server):** GET sends path + query; POST sends JSON body; headers sent; JSON parsed; non-JSON preserves body text; timeout captured as error; duration recorded; OPTIONS/HEAD requests succeed and reject a `body`/`form` at validation; multiple `Set-Cookie` headers are all captured in `setCookie`; `follow_redirects: false` returns the 30x response with its `location` header instead of following it; `form` sends a urlencoded body; a same-origin absolute-URL `path` succeeds and a cross-origin one fails with an error naming both origins.

**Cookie jar:** `cookies: true` applies a step response's Set-Cookie to later requests on the same target, keyed by (name, path) with last-write-wins per key; a manual-redirect (30x) step's Set-Cookie is still captured; send order is most-specific-path-first; an explicit step `Cookie` header overrides the jar for that request's send only (the jar still ingests the response); `legacy`/`new` jars never cross-contaminate; a scenario without `cookies: true` never populates a jar.

**Comparison:** exact match passes; status mismatch fails; selected header mismatch fails; key-order difference passes semantically; ignored path not compared; array sort by ID works; unordered arrays compare as sets; missing field reported with path; extra field reported when not ignored; redacted fields absent from diff output.

**Set-Cookie/Location comparison and expectations:** `set_cookie`/`location` contract blocks compare per Section 8.6 (name pairing, positional pairing within duplicate-name groups, attribute case-insensitivity, `ignore_cookies`/`ignore_attributes`/`ignore_query_params`, `compare_values`/`origin` variants); an unparseable value falls back to exact-string compare; `compare_headers` listing `set-cookie` is a load-time error with or without a block, and listing `location` is one only alongside a `location` block; a dedicated test proves a `set_cookie` mismatch never renders a raw cookie value; `expect.headers`/`header_absent`/`set_cookie`/`location` assert correctly against a single response; both `milliseconds` and `millis` are accepted for timestamp precision; a duplicate list entry across defaults and a route resolves to one entry after merge.

**Contract:** valid contract loads; reference resolves; merge correct; contract+inline conflict rejected; `check-contract` flags out-of-subset paths.

**Hooks:** setup assigns variables; cleanup runs after success; cleanup runs after failure; hook failure marks scenario failed with a useful message; unknown hook fails clearly.

**Recording:** legacy response recorded; fixture schema correct; recording redacts secrets; update refused when not enabled; replay loads fixture and compares; replay with missing fixture fails clearly.

**Environment and production safety:** `environment: production` with an untagged scenario produces a refusal result (`pass: false`, non-zero exit), not a skip; a scenario tagged with `allowedEnvironments` including `production` runs; `production_url_patterns` matching a configured base URL outside `environment: production` aborts before any request with a config error; non-production environments keep today's skip-and-pass-through behavior.

**Scaffolding (`pharos init`):** writes the documented file set into a tmpdir; the generated tree's `hooks/index.ts` imports from the `pharos` package name; `validate` passes on the generated tree unmodified; rerunning without `--force` refuses and names the conflicting files; `--force` overwrites.

**CLI:** `run --scenario <id>` runs one; `run --include-tag smoke` filters; `run --exclude-tag destructive` filters; `validate` catches invalid scenarios; failed scenario → non-zero exit; passing run → zero exit.

### 16.3 Quality gates

- TypeScript compiles; Vitest harness tests pass; example scenario validation passes; example scenarios run against mock endpoints; README instructions work from a fresh checkout.

### 16.4 Migration-readiness quality gate (for a service/endpoint)

A route is ready for **Limen shadow mode** when: relevant scenarios pass in CI; success, error, and edge cases are covered; known intentional differences are tagged `intentional-change`; ignore/normalization rules in the contract have been reviewed; destructive tests are isolated and guarded.

---

## 17. Risks and Mitigations

- **Tests codify legacy bugs unintentionally.** Require human review of scenarios; tag intentional bug-compatibility as `legacy-bug-compatible`; tag intended new behavior as `intentional-change`.
- **Over-normalization hides real bugs.** Require justification/comments for broad ignore rules; keep ignored paths narrow; review normalization (in the contract) in PRs. (This is exactly the failure mode Pharos exists to catch during refinement.)
- **Tests mutate shared data.** Guard destructive scenarios; prefer generated test data; require cleanup hooks; support environment restrictions.
- **Recordings contain secrets.** Redact headers and JSON paths before writing; default to recording only safe headers; test that redaction works.
- **CI flaky due to live legacy dependency.** Support replay mode; separate live-comparison tests from deterministic replay tests; use tags to control which modes run in CI.

---

## 18. Implementation Guidance

Favor simple, clear abstractions over framework magic. Prioritize, in order: **correctness and clear error messages**; readable code and simple abstractions; deterministic output; useful failure artifacts; safe handling of secrets and destructive operations.

**Keep extension interfaces small:** the hook registry, custom comparators/normalizers, the contract loader, and the HTTP client should be modular so future work (OpenAPI/HAR import, a shared normalization package, Pattern B Vitest generation, additional providers) lands without reshaping the core.

**Avoid:** a web UI; a database; distributed execution; production traffic routing; service-mesh features; overly complex plugin systems.

**Before coding:** read this spec end to end (and the Limen spec for contract alignment). Then implement phase by phase (Section 14), running tests after each major component.

---

## 19. Packaging and Scaffolding (`pharos init`)

### 19.1 Packaging: consuming Pharos as a git dependency

Target repos consume Pharos as a **bun git dependency** pinned to a commit (`"pharos": "github:charliek/pharos#<sha>"` in the target repo's `package.json`), not as a published npm package. This requires Pharos to expose a stable import surface:

- `src/index.ts` — a public barrel exporting hook types (`HookContext`, `HookFn`), config types (`PharosConfig`), and the scenario/contract zod schemas and inferred types — the minimal surface a target repo's `hooks/index.ts` and tooling need. Internal modules (`src/execution/*`, `src/comparison/*`, etc.) remain unexported implementation detail, free to change without a semver contract.
- `package.json` gains `exports` and `types` fields pointing at `src/index.ts`, and a `files` allowlist so a pinned git ref carries only what a consumer needs. No build step is introduced — bun runs TypeScript directly, so `exports` maps straight to source. (Because `init`'s templates are inline strings rather than a `templates/` directory, scaffolding needs no addition to this allowlist.)
- The consuming side of the pin is generated: `pharos init` writes the target repo's `package.json` with the dependency pre-shaped and an unmistakable placeholder SHA to replace (Section 19.2).
- Local co-dev override: a target repo may point its dependency at `"pharos": "file:../pharos"` while iterating locally against an unmerged Pharos change; this override is **never committed** — the committed reference is always the pinned commit SHA.

### 19.2 `pharos init [dir]`

Scaffolds a runnable conformance directory into a target repo (default `dir`: current directory). Writes:

- `package.json` — a minimal, private package making the scaffold runnable: a `conformance`/`validate`/`record` script trio invoking the `pharos` bin, and the git dependency of Section 19.1 written with a **placeholder** commit SHA the user must replace. The placeholder is deliberately not a floating branch ref: an unpinned dependency would let the harness change underneath a target repo's CI.
- `pharos.config.json` — pre-filled with `hooks_module`, the standard directory layout (Section 6.2), and sensible redaction defaults. The `allow_*` safety toggles are omitted rather than written as `false`: their default is already `false`, and pre-writing them makes flipping one a one-character edit nobody reviews. JSON carries no comments, so the guidance lives in the generated README.
- `scenarios/` — containing one minimal example scenario (a `new_only_assert` `GET /health` asserting status 200, tagged for every environment) that references the stub contract below. The scaffold must be *demonstrably* runnable, not merely well-formed, so the tree ships with something to run and to imitate; delete it once real scenarios exist.
- `contracts/<service>.contract.yaml` — a minimal stub contract (Section 5.2) with the one route the example scenario references; `<service>` is derived from the target directory name or an `--service` flag. `<service>` must be a lowercase slug — it becomes a filename, the contract/scenario `service` field, and the generated package name — so a value outside that shape is a config error.
- `hooks/index.ts` — a named hook registry stub (Section 7.2) importing its types from the `pharos` package name — not a relative path into Pharos's own source (Section 19.1) — so it works unmodified once the git dependency is installed.
- `.gitignore` — ignores `reports/` (generated output, Section 11), `fixtures/recordings/` (recordings are opt-in and reviewed before they are committed, Section 10.2), and `node_modules/`.
- `README.md` — points at this spec, documents that the runner must be invoked from the scaffold root (Section 19.3), summarizes the safety model (`environment` vs. `safety.allowedEnvironments` vs. `production_url_patterns`, Section 12) and the recognized environment variables, and repeats the `follow_redirects` pitfall (Section 9.3), since it is the most common cookie/redirect authoring mistake.

`pharos init` is **idempotent**: rerunning it against a directory with existing scaffold files **refuses to overwrite** them and exits non-zero, naming the conflicting files. The refusal is **all-or-nothing** — a single collision aborts before the first write, so a refused `init` never leaves a half-scaffold behind. Conflict detection is by path *type*, not mere presence: a directory the scaffold needs (`scenarios/`, `contracts/`) occupying disk as a **file**, or a generated file path occupying disk as a **directory**, is a conflict too, because the write would otherwise fail partway through. `--force` overrides a stale *file* and rewrites everything, but never a path held by the wrong kind of entry — resolving that needs a delete, and `init` does not delete what the user put there. Scaffolding into an existing, non-empty directory is otherwise fine: only a path the scaffold itself needs counts as a conflict.

### 19.3 Cwd-based config resolution

Pharos resolves its config file and the directories within it relative to the **current working directory** at invocation time (Section 6.1). `pharos init` therefore writes paths relative to the scaffold root, and the scaffold's README documents that the runner (the target repo's `conformance`-style script) must be invoked from that root — not from the target repo's own root if the scaffold lives in a subdirectory.

---

## Appendix A. Streaming and SSE Endpoints Are Out of Scope

Live streaming endpoints — server-sent events, chunked feeds, any response whose body is not intended to end — are **outside Pharos scenario scope entirely**. This is a scope boundary, not a gap awaiting a mode.

**Why the executor cannot assert one.** The HTTP client reads the full response body before it builds a response record: `sendRequest` awaits `fetch`, then awaits `response.text()`, and only then returns an `HttpResponseRecord` (Section 9). Nothing downstream — extraction, normalization, comparison, expectations — sees a response until that read completes. Against an unending body the read never completes, so the request's `AbortController` fires at the timeout and the step produces the timeout error record (status `0`, empty `bodyText`, `error.type: 'timeout'`) instead of an assertable response. That record is honest about what happened and useless as a behavioral assertion: it describes Pharos's own timeout, not the service's streaming behavior.

**No headers-only mode.** Returning a record after the response head, leaving the body undrained, would be a material scope increase — a second response shape that every strategy, the recording format (Section 10.1), and the reporters would have to understand, plus stream lifetime management the harness deliberately does not have. It is **deferred**, not planned.

**Where such routes belong.** They belong to Limen's relay and observe side (Section 13): unsampled traffic is proxied on the streaming path without buffering, observe mode profiles these routes under `length_missing` (a response with no `Content-Length` yields no stability evidence), and a request selected for comparison whose response is `text/event-stream` is comparison-skipped by content type before a byte is buffered, so the client keeps streaming. A sampled response that streams without declaring itself SSE is buffered only up to Limen's size and time bounds, then demoted back to streaming with the comparison skipped.

**What a scenario covers instead.** The service's non-streaming metadata endpoints — the handshake, subscription, or status routes around the stream — are ordinary request/response routes and are compared normally. No new vocabulary is introduced for any of this.
