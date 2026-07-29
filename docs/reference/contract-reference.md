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

Every route's `match` block — `methods` and `path_template` — is **required**,
not just informational: it keeps a contract self-describing and is checked
byte-for-byte against Pharos's identical schema (lockstep). `check-contract`
rejects a route missing either field, or with an empty `path_template`.

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
| `normalize_timestamps` | `[{path, precision}]` | Truncate the timestamp to `seconds`/`milliseconds`/`minutes`/`hours`/`days`. `millis` is also accepted (Limen's historical spelling of `milliseconds`); the two resolve identically. |
| `enum_aliases` | `[{path, aliases}]` | Map equivalent enum spellings to a canonical value. |
| `set_cookie` | `{compare, ignore_cookies, ignore_attributes, compare_values}` | Optional dimension comparing every `Set-Cookie` header, by cookie name. Omitted = not compared. |
| `location` | `{compare, ignore_query_params, origin}` | Optional dimension comparing the `Location` header, part-wise. Omitted = not compared. |

### `set_cookie` and `location`

These are the two comparison dimensions beyond status/body/headers, legal at
both `defaults` and per-route `comparison`:

```yaml
set_cookie:
  compare: true
  ignore_cookies: []          # cookie names excluded entirely
  ignore_attributes: []       # e.g. Expires — clock-dependent
  compare_values: exact       # exact | presence
location:
  compare: true
  ignore_query_params: []     # e.g. state, nonce — per-request values
  origin: exact                # exact | ignore
```

- **`set_cookie`** parses each side's `Set-Cookie` values into `(name, value,
  attributes)` and pairs them by name (duplicate names pair positionally).
  `compare_values: exact` also compares the value; `presence` only checks that
  a value exists on both sides (so a shared `session=; Max-Age=0` deletion
  shape still matches). A cookie value is never rendered in a mismatch — only
  its name and attributes.
- **`location`** parses the `Location` header as a URL on both sides,
  resolving a relative value against the originating request URL first, so a
  legacy `/next?x=1` and a new `https://new.example/next?x=1` compare equal
  when both resolve to the same target. `origin: exact` compares
  scheme+host+effective-port along with path and query; `origin: ignore`
  compares only path and query, for routes where legacy and new intentionally
  redirect to different hosts.
- Both fall back to exact string comparison when a value can't be parsed.
  Full semantics — case sensitivity, malformed-value handling, rendering — are
  normative and pinned in lockstep with Pharos; see
  [the spec](../limen_spec.md#42-contract-format) §4.2.
- Because these are separate dimensions rather than `compare_headers` entries,
  listing `set-cookie` or `location` (any case) in `compare_headers` while the
  corresponding block is present anywhere in a route's resolved rules is a
  **load-time validation error** — drop the `compare_headers` entry.

## Merge semantics

A route's effective rules come from merging service `defaults` with the
per-route `comparison` block (spec §4.2):

- **Scalars** (`compare_status`, `compare_body`, and — within `set_cookie` /
  `location` — `compare`, `compare_values`, `origin`): the per-route value wins
  if set, else the default, else the safe built-in (`true` / `exact` / `exact`).
- **Lists** (`compare_headers`, every `json` list, and `set_cookie`'s /
  `location`'s own lists — `ignore_cookies`, `ignore_attributes`,
  `ignore_query_params`): the default's entries followed by the route's,
  concatenated and de-duplicated. This is a **union, never a reconciliation** —
  the behavioral and operational namespaces are disjoint by design, so a merge
  cannot conflict.

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
