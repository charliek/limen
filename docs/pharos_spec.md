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

1. **Local comparison during development.** `npm run ftest -- run --scenario users.get-user-success` calls both services, compares, prints readable pass/fail with diffs.
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
    cli/
      run.ts                       # run scenarios (filters, modes)
      validate.ts                  # validate scenarios + contracts
      record.ts                    # record legacy interactions (explicit opt-in)
      check-contract.ts            # validate a contract + JSONPath compliance
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
      variables.ts                 # template substitution + extraction
      hooks.ts                     # hook registry loading + invocation
      fixtures.ts                  # recording read/write helpers
    comparison/
      compare.ts                   # strategy dispatch
      normalize.ts                 # normalization transforms
      jsonpath.ts                  # the supported JSONPath subset
      json-diff.ts                 # readable structural diff
      matchers.ts                  # explicit-expectation matchers
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
    auth.ts
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

### 4.6 Steps, requests, and extraction

Each step has an `id`, optional `name`, a `request`, optional `extract`, and a `compare` block.

```yaml
request:
  method: POST                      # GET | POST | PUT | PATCH | DELETE
  path: /users
  query:
    includeDetails: true
  headers:
    authorization: Bearer {{ env.AUTH_TOKEN }}
    content-type: application/json
  body:
    email: test-{{ random.uuid }}@example.com
    displayName: Test User
  timeoutMs: 5000                   # optional per-request timeout override
```

Extraction stores response values for later steps:

```yaml
extract:
  userId:
    from: legacy.body               # legacy.body | new.body | response.body | legacy.headers | new.headers
    path: $.id
  etag:
    from: legacy.headers
    path: etag
```

For single-target modes (`new_only_assert`, `replay_against_recording`), `from: response.body` may be used.

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
    body:
      json_paths:
        $.error.code: USER_NOT_FOUND
        $.error.message: User not found
```

**`custom`** — service-specific comparison via a named comparator from the hook registry.

```yaml
compare:
  strategy: custom
  comparator: compareDeviceList
  args:
    ignore_offline_timestamp: true
```

**Inline-rules fallback.** A scenario without a `contract` reference may declare normalization inline under `compare.body` / `compare.headers`, using the **same vocabulary as the contract** (Section 8). If a scenario specifies **both** a `contract` reference **and** an inline behavioral block, that is a **validation error** — one source of behavioral truth per scenario.

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

**Recording mode:**

```yaml
version: 1
id: users.record-existing-user
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
id: users.replay-existing-user
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
        404. Intentional change — tag the scenario intentional-change.
    tags: [read, migration-ready]
```

### 5.3 What lives in the contract vs. elsewhere

The contract owns **behavioral** comparison truth. Operational concerns live in each tool's own config (Pharos config / Limen route config). The key namespaces are distinct, so a merge is a **union, never a reconciliation**:

| Concern | Lives in | Used by |
|---|---|---|
| **What** to compare and **how** (`ignore_paths`, `redact_paths`, `sort_arrays`, `unordered_arrays`, `normalize_timestamps`, `enum_aliases`, `compare_status`, `compare_body`, `compare_headers`) | **Contract** | Both Pharos and Limen |
| Scenario structure, steps, hooks, modes, recording fixtures | **Pharos scenarios** | Pharos only |
| Rollout, upstreams, shadow sampling, circuit breaker, flags | **Limen route config** | Limen only |

### 5.4 How Pharos consumes the contract

- A scenario **references** a contract route via `contract: "path#routeId"`.
- At load time, Pharos resolves the reference and **merges** the contract's behavioral rules (service `defaults` + per-route `comparison`, per-route merging onto defaults) into the scenario's comparison configuration.
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
- Output mode (`local` | `ci`).
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
ALLOW_DESTRUCTIVE_TESTS=false
ALLOW_RECORDING_UPDATES=false
AUTH_TOKEN=...
```

### 6.3 Validation

The harness fails with an actionable error when required config for the selected mode is missing (e.g. `compare_live` requires both base URLs; `replay_against_recording` requires `fixture_dir`).

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
- **Normalize timestamps** to a configured precision (`normalize_timestamps`).
- **Map enum aliases** (`enum_aliases`).
- Apply **custom normalizers** by name (from the hook registry).

### 8.3 Comparison result model

```ts
export type ComparisonStrategy =
  | 'exact' | 'json_semantic' | 'subset' | 'explicit_expectations' | 'custom';

export interface Mismatch {
  path: string;
  kind: 'status' | 'header' | 'body' | 'missing' | 'extra' | 'type' | 'value' | 'custom';
  expected?: unknown;
  actual?: unknown;
  message: string;
}

export interface ComparisonResult {
  pass: boolean;
  summary: string;
  mismatches: Mismatch[];
  diffText?: string;
}
```

### 8.4 Supported JSONPath subset (hard MVP boundary)

Identical to Limen's, so contracts are portable:

- `$.field`
- `$.nested.field`
- `$.items[*].field` (wildcard over array elements)

Anything outside this subset is a **validation error** at scenario/contract load time. The subset may expand later, **in lockstep** with Limen.

### 8.5 Redaction scope

Applies to console logs, JSON reports, JUnit reports, failure artifacts, and recordings. Configurable targets: header names (`authorization`, `cookie`, `x-api-key`), JSON paths (`$.token`, `$.password`, `$.user.email`), query parameters (`access_token`). **No secret value appears in any output.** A test proves it (Section 16).

---

## 9. HTTP Client

The client must:

- Build absolute URLs from base URL + request spec.
- Support GET/POST/PUT/PATCH/DELETE, path templates, query params, headers, JSON bodies, and plain-text bodies as a fallback.
- Apply default headers from config and per-request timeout (with optional, default-off retries for transient failures).
- Capture status, headers, body text, parsed JSON (when possible), duration, and errors.
- Redact sensitive headers in logs.

Internal models:

```ts
export interface HttpRequestSpec {
  method: 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE';
  path: string;
  query?: Record<string, string | number | boolean | null>;
  headers?: Record<string, string>;
  body?: unknown;
  timeoutMs?: number;
}

export interface HttpResponseRecord {
  status: number;
  headers: Record<string, string>;
  bodyText: string;
  bodyJson?: unknown;
  durationMs: number;
  error?: { type: string; message: string };
}
```

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

### 10.2 Recording safety

- Writes require an explicit `record` command **or** `ALLOW_RECORDING_UPDATES=true`.
- **CI refuses recording updates by default.**
- Secrets are redacted **before** a recording is written.
- Scenarios declare which headers are safe to record (`safe_headers`); only those are persisted.

### 10.3 Replay behavior

Replay loads the recording, applies allowed variable substitutions to recorded request paths/bodies, executes the new request, normalizes both responses, and compares. A missing or invalid fixture fails clearly.

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

Exit `1` when any required scenario fails; exit `0` when all selected required scenarios pass. Skipped scenarios are reported separately and do not by themselves fail the run.

---

## 12. Safety Controls

- **Destructive scenarios require explicit opt-in** (`ALLOW_DESTRUCTIVE_TESTS=true` or a CLI flag).
- **Production-like environment destructive runs require an additional guard override** (`requiresProductionGuardOverride`).
- **Recording updates require explicit opt-in** and are refused in CI by default.
- **Secrets are redacted** in all logs, reports, artifacts, and recordings; no secret is written to a fixture or report.
- **Fail fast** when required base URLs (or other mode-required config) are missing.
- **Environment guardrails:** refuse destructive tests against disallowed environments unless explicitly allowed.

---

## 13. Relationship to Limen (Migration Proxy)

- **Separate repositories, no build-time dependency.** Either project builds and runs alone.
- **The shared behavioral contract (Section 5) is the integration point** — a documented schema and vocabulary, *not* shared code in the MVP.
- **One vocabulary, one JSONPath subset.** Field names, normalization semantics, and the supported JSONPath subset are identical, enforced by Section 8.4 and the shared contract format. `check-contract` in both tools must agree.
- **Workflow:** AI drafts a contract → **Pharos validates and refines** it deterministically (catching over- and under-normalization) → **Limen consumes** the refined contract unchanged for production shadow comparison and rollout.
- **Readiness signals (bidirectional):** passing Pharos scenarios for a route is a precondition for enabling Limen shadow mode; a clean Limen shadow mismatch rate is a precondition for raising rollout; **Limen-observed mismatches become new Pharos scenarios**, closing the loop.
- **Deferred:** a shared `normalization` package extracted from both could later replace the shared-contract-by-schema approach. Designed toward, not built now.

---

## 14. Phased Build Plan

Each phase ends with passing harness tests and a runnable CLI. Build incrementally and run tests after each major component.

### Phase 0 — Scaffold
- TypeScript + Vitest project; module skeleton (Section 3.5); npm scripts; CI (typecheck + test + lint); README stub; license.
- CLI shell with `run`, `validate`, `record`, `check-contract`.
- **Done when:** `npm run test` (empty) passes; typecheck and lint pass in CI.

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

---

## 15. Example Service and Scenarios

### 15.1 Example endpoints (implement or mock)

- `GET /users/:id`
- `GET /users`
- `POST /users`
- `GET /users/:id` not-found case
- `POST /users` validation-error case

### 15.2 Required example scenarios

1. `users.get-user-success` — successful GET comparison.
2. `users.get-user-not-found` — 404/not-found comparison.
3. `users.create-user-validation-error` — validation-error comparison.
4. `users.list-users-sort-array` — list endpoint with array sorting.
5. `users.replay-get-user-recording` — replay against a recording.
6. `users.new-only-healthcheck` — new-only explicit assertion.
7. `users.create-then-fetch-destructive` — multi-step create/read/delete flow guarded as destructive.

Plus an example scenario demonstrating **ignored dynamic response fields** (may be folded into #1 via the contract).

---

## 16. Acceptance Criteria and Test Plan

### 16.1 MVP acceptance criteria

**Project scaffold:** TypeScript + Vitest; clear npm scripts for validation, execution, and harness tests; example scenarios; README.

**Scenario loading & validation:** loads YAML (and JSON) from `scenario_dir`; validates required fields; rejects unknown/invalid enum values; reports errors with file path **and** field path; selects by ID and tag.

**Contract:** loads YAML/JSON; reference resolves; behavioral rules merge (defaults + per-route, per-route overriding); contract-vs-inline conflict rejected; `check-contract` validates schema and JSONPath subset and agrees with Limen.

**Configuration:** legacy/new base URLs from env or file; scenario/contract/fixture/report dirs configurable; actionable error if required config missing for the selected mode; local and CI output modes.

**HTTP execution:** GET/POST/PUT/PATCH/DELETE; path templates, query, headers, JSON bodies; default headers; per-request timeout; captures status, headers, body text, parsed JSON, duration, errors.

**`compare_live`:** calls both services; compares per strategy; per-step pass/fail; failure artifacts written.

**`legacy_record`:** calls legacy only; writes fixture only when recording enabled; redacts secrets before writing; refuses recording updates in CI by default.

**`replay_against_recording`:** loads recorded legacy response; calls new; normalizes both; compares; fails clearly on missing/invalid fixture.

**`new_only_assert`:** calls new only; supports explicit expectations for status and JSON paths.

**Normalization & diffing:** ignore paths; ignore headers; semantic JSON (key-order-independent); sort arrays by key; readable JSON diffs; no secrets in diffs.

**Hooks & variables:** scenario variables; env substitution; built-ins (UUID, timestamp); setup/cleanup from the registry; extraction into later steps; cleanup runs after a failed step.

**Reporting:** useful console output; JSON report written; exit `1` on required-scenario failure, `0` on pass; skipped reported separately.

**Safety:** destructive scenarios require opt-in; production-like destructive runs require an additional guard override; auth headers and configured secret fields redacted; fixtures cannot be written accidentally.

### 16.2 Harness test plan (Vitest)

These validate the framework itself.

**Scenario schema:** valid minimal scenario passes; missing ID fails; invalid mode fails; missing request method fails; unknown comparison strategy fails; destructive scenario without safety block fails/warns per policy; YAML parse error gives a useful file path.

**Variable substitution:** `{{ variables.userId }}` resolves; `{{ env.AUTH_TOKEN }}` resolves; missing variable fails with a clear message; random UUID is valid; substitution works in path, query, headers, body.

**HTTP client (mock server):** GET sends path + query; POST sends JSON body; headers sent; JSON parsed; non-JSON preserves body text; timeout captured as error; duration recorded.

**Comparison:** exact match passes; status mismatch fails; selected header mismatch fails; key-order difference passes semantically; ignored path not compared; array sort by ID works; unordered arrays compare as sets; missing field reported with path; extra field reported when not ignored; redacted fields absent from diff output.

**Contract:** valid contract loads; reference resolves; merge correct; contract+inline conflict rejected; `check-contract` flags out-of-subset paths.

**Hooks:** setup assigns variables; cleanup runs after success; cleanup runs after failure; hook failure marks scenario failed with a useful message; unknown hook fails clearly.

**Recording:** legacy response recorded; fixture schema correct; recording redacts secrets; update refused when not enabled; replay loads fixture and compares; replay with missing fixture fails clearly.

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