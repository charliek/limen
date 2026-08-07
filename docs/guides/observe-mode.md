# Observe mode

Observe mode is Limen's answer to "what would this route's traffic tell a
classifier, without shadowing anything?" It turns on a passive, bounded
profile of the traffic Limen already relays, and `limen suggest-routes` turns
that profile into a draft configuration — evidence and a disposition per
route, never a decision made on your behalf. This guide runs the loop end to
end: turn the block on, drive traffic, read the profile, generate a draft, and
treat that draft the way [classifying routes](classifying-routes.md) says a
human should treat any traffic-derived hypothesis — as a starting point to
confirm, not a verdict to trust.

## Prerequisite: bind the control plane to loopback

**Do this before turning observe mode on anywhere but a laptop.**
[`metrics.listen_addr`](../reference/config-reference.md#metrics) defaults to
`0.0.0.0:9090` — every interface. That default is a deployment choice you make
consciously for the ordinary control plane (health checks, `/metrics`), and
observe mode raises the stakes on it: the profile it serves at
`GET /observe/profile` discloses route topology (every configured route id)
and the distinct query-parameter *names* traffic has carried on each one. None
of that is a secret *value* — see [what the profile does and does not
contain](#what-the-profile-does-and-does-not-contain) — but names and shapes
are still more than an arbitrary caller on the network should be able to read
for free.

```yaml
metrics:
  listen_addr: "127.0.0.1:9090" # or an internal-only interface
```

`limen run` logs a loud startup warning whenever the `observe:` block is
present, precisely so an accidental production enablement is noisy rather than
silent — the same posture as [`debug.sink_canary`](../reference/config-reference.md#debug).

## 1. Turn the block on

Presence of `observe:` is the whole switch — there is no separate `enabled`
field, like [`diff_sink`](../reference/config-reference.md#diff_sink). This
differs from [`debug`](../reference/config-reference.md#debug): an empty
`debug: {}` does *not* enable `sink_canary`, which is a bool defaulting to
`false` inside that block, whereas an empty `observe: {}` does enable
observation:

```yaml
observe:
  sample_rate: 1.0      # 0-1; see "sampling vs. classification" below
  max_query_names: 32   # per-route cap on distinct query-parameter names
  max_path_shapes: 32   # per-route cap on distinct observed paths
  max_fingerprints: 32  # per-route cap on stability fingerprints
```

All four fields default to the values shown, so `observe: {}` is a complete,
valid block. Observation is orthogonal to routing: it is legal under every
route [`mode`](../reference/config-reference.md#routes), and a route with no
`new` upstream at all can still be observed — a `RouteProfile` combines two
sources, neither of which requires a response to exist. Bounded **request**
metadata (`methods`, `query_names`, `distinct_read_paths`) is recorded for
every request regardless of what answers it. Everything else — status class,
content type, `Set-Cookie`/redirect/`Location` presence, the
`Content-Length`-based stability signal — comes from the final **response**
Limen sends the client, whether that response came from `legacy`, `new`,
both, or neither; when no upstream answers at all, none of that response
metadata is recorded and `transport_errors` counts the silent upstream
instead.

## 2. Drive representative traffic

Point real (or realistically shaped) traffic at the proxy. What "representative"
means here matters more than volume: the classifier's dangerous rules are
*existential* — one redirecting or cookie-minting read is enough to demote a
route — so a corpus that never exercises a route's flow-hop paths will tell you
nothing about them, no matter how many times it hits the route's happy path.
A route with fewer than `--min-samples` reads (`GET`/`HEAD`; 5 by default) is
not classified at all — writes and transport errors do not count toward the
floor. A functional test suite driven once through each endpoint will trip
that floor on nearly every route, which is the correct, unhelpful answer for
that corpus.

### Sampling and classification are mutually exclusive

`observe.sample_rate` below `1.0` makes the profile cheaper to record but
**unusable for classification**, and `limen suggest-routes` refuses to
classify a sampled profile rather than classify it with lower confidence. The
reason is the same existential-rule fact from the previous paragraph: sampling
drops requests wholesale, and the rare mutating request is exactly the
observation sampling is most likely to drop while the route still clears every
other floor. A sampled profile is not a smaller version of the truth — it is a
version with the decisive observation possibly missing. So: observe cheaply
with `sample_rate < 1.0` to watch shape and volume over time, or observe at
`sample_rate: 1.0` to eventually classify. Not both from the same window.

**The implemented behavior, precisely** (this is the single contract both
this guide and [CLI → `suggest-routes`](../reference/cli.md#suggest-routes)
state): every route in a sampled profile is suggested `relay_only` with
reason `partial-sample`, and the run **exits `20`** — the same "nothing was
profiled" code a config with zero observations gets, because a draft resting
entirely on a refusal to classify rests on no evidence either way.
Automation driving `suggest-routes` must treat exit `20` as "no usable draft
was produced," not as a successful classification that merely says
relay-only everywhere.

## 3. Read the profile

```bash
curl -s http://127.0.0.1:9090/observe/profile | jq .
```

404s unless the `observe:` block is present. With it present, every
*configured* route appears from the first request onward, zero-filled until
observed — traffic never adds a route to the document, it only fills one in:

```json
{
  "sample_rate": 1.0,
  "routes": {
    "get-device": {
      "observations": 148,
      "reads": 148,
      "writes": 0,
      "transport_errors": 0,
      "methods": { "GET": 148 },
      "query_names": [],
      "query_names_overflow": false,
      "distinct_read_paths": 1,
      "distinct_read_paths_overflow": false,
      "status_classes": { "2xx": 148 },
      "content_types": ["application/json"],
      "content_types_overflow": false,
      "set_cookie_reads": 0,
      "redirect_reads": 0,
      "location_reads": 0,
      "length_repeats": 96,
      "length_varied": 0,
      "length_missing": 0,
      "fingerprint_overflow": false
    },
    "oauth-login-hop": {
      "observations": 0,
      "reads": 0,
      "writes": 0,
      "transport_errors": 0,
      "methods": {},
      "query_names": [],
      "query_names_overflow": false,
      "distinct_read_paths": 0,
      "distinct_read_paths_overflow": false,
      "status_classes": {},
      "content_types": [],
      "content_types_overflow": false,
      "set_cookie_reads": 0,
      "redirect_reads": 0,
      "location_reads": 0,
      "length_repeats": 0,
      "length_varied": 0,
      "length_missing": 0,
      "fingerprint_overflow": false
    }
  }
}
```

`get-device` has been driven; `oauth-login-hop` has not — and the document
says so explicitly rather than by omission, the same absence-vs-zero
discipline the [observability guide](observability.md#metrics) already
applies to Limen's zero-registered metric series. `sample_rate` travels with the document because
the proxy that recorded it is the only authority on whether the profile is
complete; a config file handed to `suggest-routes` later cannot be trusted to
restate it accurately, so the two are cross-checked and a mismatch is a typed
failure rather than a silent average.

## 4. Run `limen suggest-routes`

```bash
limen suggest-routes -c limen.config.yaml --new-upstream https://new.internal \
  > draft.limen.config.yaml
```

This polls the control plane until the profile stops changing **and**
`limen_in_flight_requests` reads zero — never a blind sleep — then classifies
every configured route and writes a draft. Full option and exit-code reference:
[CLI → `suggest-routes`](../reference/cli.md#suggest-routes).

### Reading a suggestion

Each route in the draft carries a `SUGGESTED:` comment naming a disposition, a
machine-readable reason, and the evidence behind it:

```yaml
routes:
  # SUGGESTED: compare_candidate (stable-repeated-reads) — stable across 12
  #   repeated request(s)
  #   evidence: 34 reads / 0 writes · 1 path · 2xx only · application/json
  #             · no Set-Cookie · no redirect
  #             · Content-Length stable over 12 repeats
  #   Observation cannot prove this route does not mutate. Confirm against the
  #   service's source before enabling comparison.
  #   note: to adopt this, re-run with --adopt-suggestions — once you have
  #         confirmed against the service's source that this route does not
  #         mutate.
  - id: get-device
    ...
    comparison: { enabled: false }
```

Three dispositions, in the order the classifier always resolves them (the
safer one wins a tie):

| Disposition | Meaning |
|---|---|
| `relay_only` | Either a danger signal fired, or too little was observed to say anything. Never compare. |
| `compare_narrowed` | Nothing dangerous fired, but the response body cannot be trusted for equality (varying length, more than one content type, or incomplete stability evidence). Compare status, not body. |
| `compare_candidate` | A request fingerprint repeated with a stable length and no danger signal fired. **Not a safety claim** — see [classifying routes](classifying-routes.md#what-observation-can-and-cannot-tell-you) for exactly what this can and cannot establish. |

Every rule's machine-readable `reason` (`redirecting-read`, `mints-state`,
`one-time-token-query`, and eleven others) is a stable, documented vocabulary
— the same page linked above catalogs what each one means and why it exists.

## 5. Why the default draft shadows nothing

Run `suggest-routes` with no extra flags and every route in the draft is
emitted `comparison: { enabled: false }` — including routes suggested
`compare_candidate`. This is not a hedge; it is the mechanical expression of
[the classifier's epistemic limit](classifying-routes.md#what-observation-can-and-cannot-tell-you):
response metadata can prove a route unsafe to compare, never safe, so no
traffic shape may cause this tool to *emit* a config that shadows a mutating
read. The suggestion rides as a comment specifically so it is not lost — a
draft that shadowed nothing and said nothing would be useless rather than
safe.

`--adopt-suggestions` is what emits the shadowing form (`comparison.enabled:
true`, and — for `compare_narrowed` routes — `compare_status: true` /
`compare_body: false`) for routes that reached `compare_candidate` or
`compare_narrowed`. It is a precondition, not a report: pass it only once you
have read each candidate route's suggestion against the service's *source*,
not just its traffic, and confirmed the route does not mutate. The flag's help
text and every candidate's comment repeat this; treat all three as the same
warning, not three separate ones.

```bash
limen suggest-routes -c limen.config.yaml --new-upstream https://new.internal \
  --adopt-suggestions > draft.limen.config.yaml

limen validate-config -c draft.limen.config.yaml
```

The emitted document is always a complete, loadable config — validating it is
cheap insurance, not a formality, since the tool proves this property in its
own test suite by running every draft it emits through the real loader and
validator.

## What the profile does and does not contain

The profile is a new output surface, so it is held to the same standard as
every other one (safety invariant 5: never log a secret value). It contains:

- Counts: observations, reads, writes, transport errors, per-method counts,
  per-status-class counts, a distinct-path *count*.
- Query-parameter **names** seen on reads — never a value. A bare token with
  no `=` (how a bearer credential often shows up in a URL) and an
  over-long name both collapse to a fixed `<oversized>` sentinel rather than
  being recorded or truncated, so a credential cannot ride in disguised as a
  "name".
- A response content-type *essence*, parameters stripped.
- Whether reads set a cookie, redirected, or carried `Location` — counts,
  never the header's value.
- A `Content-Length`-based stability count — never a body byte. Buffering a
  body to fingerprint it would delay every client's first byte, which safety
  invariant 2 forbids.

It never contains: a request or response body, a header value, a cookie
value, or a path. Paths are counted through a bounded set of *hashes*, which
makes emitting one structurally impossible rather than merely a promise the
code keeps today — see [sub-path
aliasing](classifying-routes.md#what-observation-can-and-cannot-tell-you) for
why that refusal is also the deepest residual gap in what the classifier can
see.

Full field-by-field reference: [config reference →
`observe`](../reference/config-reference.md#observe); the counter and
endpoint's place among Limen's other control-plane surfaces:
[observability → observe mode](observability.md#observe-mode).

## Cost: one process-wide lock on the response path

Recording an observation takes one lock shared by every route's aggregate,
on the response path, for every request a profiled proxy serves. The
critical section is deliberately small — a handful of bounded map updates,
no I/O and no `await` — so it can neither block on anything nor be held
across a yield point; it is not zero cost, but it is not proportional to the
body and not a round trip. For most deployments this is not something you
need to think about. A deployment running at very high request concurrency
should validate throughput with observe mode on before enabling it broadly,
rather than assume the lock is free at any scale. If contention ever shows
up in practice, the recorder is the isolated place to shard — sharding the
map by route or by a hash of the route id is the escape hatch, not a metric
added ahead of need.

## The draft is a starting point, not a verdict

Every draft's header says this, and it is worth restating outside the
generated comment: `limen suggest-routes` gathers evidence and demotes
everything showing a danger signal, but it never asserts a route is safe to
compare, because nothing about the traffic could establish that. Read
[classifying routes](classifying-routes.md) for the taxonomy behind the
classifier's rules and the sharp edges it exists to catch; once a route is
actually shadowing traffic, [prove your lens bites](prove-your-lens-bites.md)
covers the complementary discipline of proving the comparison pipeline built
on top of it is really running.
