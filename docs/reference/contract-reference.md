# Contract reference

The behavioral contract is the single source of truth for **comparison
semantics** — *what* to compare and *how* to normalize it. It is portable,
byte-for-byte, between Limen and the [Pharos](../pharos_spec.md) test suite:
Pharos refines it against real responses, and Limen consumes the refined
contract unchanged for production shadow comparison (spec §4).

A contract is YAML or JSON (detected by extension). Validate one with:

```bash
limen check-contract ./contracts/device-service.contract.yaml
```

## Format

```yaml
version: 1
service: device-service
description: >
  Drafted from OpenAPI + traffic, refined by Pharos, consumed by Limen.

defaults:                       # service-wide; per-route blocks merge on top
  compare_status: true
  compare_body: true
  compare_headers: []           # headers compared only if listed
  json:
    ignore_paths:    [ "$.metadata.requestId" ]
    redact_paths:    [ "$.user.email", "$.token" ]
    sort_arrays:     [ { path: "$.devices", key: "id" } ]
    unordered_arrays: [ { path: "$.permissions" } ]
    normalize_timestamps: [ { path: "$.createdAt", precision: seconds } ]
    enum_aliases:    [ { path: "$.status", aliases: { ACTIVE: enabled } } ]

routes:
  - id: "get-device"
    match: { methods: ["GET"], path_template: "/devices/{id}" }
    comparison:
      json:
        ignore_paths: [ "$.device.lastSeenAt" ]   # merged with defaults
    expectations:
      typical_status: 200
      notes: "Legacy returns 200 on soft-delete; new returns 404 — intentional."
    tags: [read, migration-ready]
```

A Limen route references one entry by file + fragment:

```yaml
contract: "./contracts/device-service.contract.yaml#get-device"
```

## The behavioral vocabulary

Every rule is a deliberate exception to the default posture of *compare
everything*. Keep them narrow.

| Rule | Shape | Effect |
|---|---|---|
| `compare_status` | bool | Compare the HTTP status code (default `true`). |
| `compare_body` | bool | Compare the normalized body (default `true`). |
| `compare_headers` | `[name]` | Compare only the listed headers (default none). |
| `ignore_paths` | `[path]` | Remove these paths before hashing/diffing. |
| `redact_paths` | `[path]` | Mask these paths in all output (logs, diffs). |
| `sort_arrays` | `[{path, key}]` | Sort the array by a stable element key. |
| `unordered_arrays` | `[{path}]` | Compare the array as an unordered set. |
| `normalize_timestamps` | `[{path, precision}]` | Truncate the timestamp to `seconds`/`millis`/`minutes`/`hours`/`days`. |
| `enum_aliases` | `[{path, aliases}]` | Map equivalent enum spellings to a canonical value. |

## Merge semantics

A route's effective rules come from merging service `defaults` with the
per-route `comparison` block (spec §4.2):

- **Scalars** (`compare_status`, `compare_body`): the per-route value wins if
  set, else the default, else the safe built-in (`true`).
- **Lists** (`compare_headers` and every `json` list): the default's entries
  followed by the route's, de-duplicated. This is a **union, never a
  reconciliation** — the behavioral and operational namespaces are disjoint by
  design, so a merge cannot conflict.

## Contract vs. inline rules

A route gets its behavioral rules from **exactly one** source:

- A `contract` reference (the normal path, shared with Pharos), **or**
- Inline rules under the route's `comparison` block, using the **same
  vocabulary** (a single-file quickstart fallback).

Declaring both is a validation error — there must be one source of behavioral
truth per route.

## Supported JSONPath subset

To keep normalization fast, predictable, and identical to Pharos, only a
documented subset is accepted (spec §7.4):

| Form | Example |
|---|---|
| Field | `$.field` |
| Nested field | `$.metadata.requestId` |
| Wildcard over array elements | `$.items[*].id` |

Anything else — array indices (`$.a[0]`), recursive descent (`$..x`), bracket
notation (`$['x']`), filters — is rejected at load time. `check-contract`
reports every offending path so you can fix a contract before wiring it in. The
subset may expand later, in lockstep across Limen and Pharos.

## What does *not* live in the contract

Operational concerns — `enabled`, `sample_rate`, `max_body_bytes`, routing,
rollout, timeouts, the circuit breaker, and flags — live in the
[Limen route config](config-reference.md), never in the contract. That
separation is what keeps a single contract file portable between the proxy and
the test suite.
