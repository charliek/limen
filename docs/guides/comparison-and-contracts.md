# Comparison & contracts

When Limen shadows or splits traffic, it compares the new service's response
against legacy's. This page explains *how* that comparison works. The rules that
drive it live in the [behavioral contract](../reference/contract-reference.md);
here we cover the engine that applies them.

## Hybrid comparison

Comparison is a two-step, fail-fast process (spec §7.1):

1. **Normalize** both responses with the merged contract rules.
2. **Hash** the normalized canonical form of each (`blake3`).
3. If the hashes **match**, record a match — no diff is generated (the common,
   cheap case).
4. If they **differ** and both bodies are JSON, generate a JSON-aware structural
   **diff**.
5. If a body isn't JSON, record a body mismatch by byte comparison — never the
   bytes themselves.

Comparison dimensions default to **HTTP status** and **normalized body**.
Headers are compared only when a contract lists them in `compare_headers`.
`Set-Cookie` and `Location` are two further, optional dimensions — see below.

## `set_cookie` and `location`

A contract's `set_cookie` and `location` blocks (`defaults` or per-route
`comparison`) turn on comparison of every `Set-Cookie` response header and of
the `Location` header, respectively — read separately from the single-value
header map `compare_headers` uses, which is why listing `set-cookie` there is
always a load-time validation error (that map holds one value, so the rest of a
multi-cookie response would be dropped). Listing `location` there is a
load-time validation error only while the `location` block is present.

- `set_cookie` pairs cookies by name and compares their attributes (and, by
  default, their values — `compare_values: presence` relaxes that to "a value
  exists on both sides", useful for logout's shared `session=; Max-Age=0`
  shape). A cookie value is never rendered in a mismatch, only its name and
  attributes.
- `location` resolves a relative `Location` against the request URL before
  comparing part-wise, so a relative redirect on one side and an absolute one
  on the other can still match. `origin: ignore` drops scheme/host/port from
  the comparison, for routes that intentionally redirect to different hosts.

Full field reference: [contract reference → `set_cookie` and
`location`](../reference/contract-reference.md#set_cookie-and-location).

## Normalization

Normalization makes incidental differences disappear so only meaningful ones
remain. The transforms (spec §7.2), all driven by the contract:

| Rule | Effect |
|---|---|
| `ignore_paths` | Remove the matched fields entirely before comparing. |
| `enum_aliases` | Map equivalent enum spellings to one canonical value. |
| `normalize_timestamps` | Parse the timestamp, **convert to UTC**, and truncate to the configured precision. |
| `sort_arrays` | Order an array by a stable element key (tie-broken on the full element, so duplicate keys stay deterministic). |
| `unordered_arrays` | Order an array as a set. |

Object key order is canonicalized at hash time, so **key order never causes a
false mismatch**. Normalization is deterministic and order-independent — the
same logical response always produces the same canonical form.

!!! note "Timestamps are converted, not relabeled"
    `normalize_timestamps` parses the value as RFC 3339 and converts it to UTC
    before truncating. `2024-01-01T12:30:45+05:30` and `...T07:00:45Z` normalize
    equal (same instant); `...T12:30:45+05:30` and `...T12:30:45Z` do **not**
    (different instants) — so the normalization can never mask a real time
    difference.

## Redaction — no secret in any diff or log

Redaction is a load-bearing guarantee (spec §7.5): **no secret value appears in
any diff or log**. It is applied at *render* time, not during hashing — so the
hash and diff run over real values and a difference in a sensitive field is
still *detected*, while its value is *masked* in the output.

- `redact_paths` in the contract mask JSON values. A difference at, under, **or
  at an ancestor of** a redacted path is masked — so a removed subtree (or a
  whole-document type change) that contains a secret never leaks it. The
  rendered path is also truncated to the contract-defined prefix, so a value
  that happens to be an *object key* can't leak through the path either.
- A built-in set of sensitive **header** names (`authorization`, `cookie`,
  `set-cookie`, …) and **query** parameters (`access_token`, `token`, …) are
  always masked in output.
- If a redact path can't be resolved, the engine **fails closed** — it reports
  the mismatch but emits no diff values rather than risk a leak.

## Bounded output

Diffs are bounded (spec §7.3): a maximum number of differences and a maximum
rendered value length. Over-limit strings are truncated at a UTF-8 boundary and
large composite values are elided to a byte count, so a diff can never produce
unbounded output.

## The supported JSONPath subset

Every path in a contract must be within the
[documented subset](../reference/contract-reference.md#supported-jsonpath-subset):
`$.field`, `$.nested.field`, and a single `$.items[*].field` wildcard. This is
identical to Pharos, so a contract is portable between the two. Anything outside
the subset is rejected at load time by `validate-config` and `check-contract`.
