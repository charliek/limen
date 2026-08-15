# Putting a production service behind Limen

There is a difference between the sequence a tool *supports* and the sequence
someone has actually run end to end. This page is the second one: the stages two
real services have been through in order, what each stage costs, what each one
proves, and — at the end — an explicit ledger of the parts of Limen's design
that no service has exercised yet.

It is the narrative depth behind the `migrate-a-service` skill. That skill is
the funnel: six stages, the flags, the exit tables, the safety invariants, in
the order you run them. This page is why the funnel is shaped that way, and what
happened when it met a service nobody had prepared for it. Read the runbook
([migration runbook](../runbook.md)) for the full operational procedure across
both tools; read this for the adoption decision — *is this path real yet, and
where does it stop being real*.

## The tested path

Six stages, each producing an artifact the next one consumes. The ordering is
not stylistic: every stage exists to make the following stage's decision
cheaper or safer, and skipping one does not remove its cost, it relocates the
cost to an incident.

| Stage | Produces | Needs a `new` upstream? |
|---|---|---|
| Observe | a traffic profile | no |
| Draft | a suggested config | no |
| Classify | a dispositioned route inventory | no |
| Same-upstream rehearsal | a working shadow pipeline | no |
| Verdict | a typed exit code | yes, to mean anything |
| Report | one self-contained status page | — |

The first four run against a service whose replacement does not exist yet. That
is the property that makes adoption cheap: a team can put Limen in front of
production, learn which of its routes are dangerous, and rehearse the entire
comparison mechanism *before* committing a line of the rewrite.

### 1. Observe — passive, and safe to leave on in production

Put Limen in front of the legacy service with your route inventory as its route
table. `mode: legacy_only` is enough; no route needs a `new_upstream`. Then add
the `observe:` block — presence is the whole switch — and drive real traffic
through it.

```yaml
metrics: { listen_addr: "127.0.0.1:9090" }   # or an internal-only interface
observe: {}                                   # sample_rate defaults to 1.0
```

Two constraints are not negotiable. **Bind the control plane to loopback
first**: the profile discloses route ids and the distinct query-parameter
*names* traffic carried, which is more than an arbitrary caller on the network
should read for free. And **observe unsampled**: the classifier's danger rules
are existential, so a sampled profile is refused classification outright rather
than classified with lower confidence. Coverage of the route table, not request
volume, is what this stage is buying.

Nothing here touches the client path beyond one small process-wide lock on the
response path, and no request is ever duplicated — the whole stage is a record
of traffic Limen was already relaying.

→ [observe mode](observe-mode.md)

### 2. Draft — turn the profile into a config

```bash
limen suggest-routes -c limen.config.yaml --new-upstream https://new.internal \
  > draft.limen.config.yaml
limen validate-config -c draft.limen.config.yaml
```

The config passed here must be the *profiled proxy's* config: it supplies the
route table and the `observe.sample_rate` cross-check that proves the profile
and the config describe the same proxy.

The one thing worth internalizing about this stage is that **exit `20` still
writes a draft**. The document goes to stdout either way, so the existence of a
file proves nothing — `20` means the draft rests on refusals to classify (no
observations, every route below the read floor, or a sampled profile) and is
unadoptable rather than absent. Automation branches on the exit code, never on
whether output appeared.

→ [CLI → `suggest-routes`](../reference/cli.md#suggest-routes)

### 3. Classify — the expensive stage, and the one no tool finishes

The draft's three dispositions are evidence, not answers. Mapping each route
onto the class taxonomy — and reading the handler source for every route the
tool proposed as a comparison candidate — is the single most expensive
intellectual step in the whole campaign, and it is the step that cannot be
delegated.

The reason is an asymmetry the classifier is built around: **response metadata
can prove a route unsafe to compare, and can never prove one safe.** A route
whose traffic is flawless from the outside — reads only, `2xx` only, a stable
`Content-Length` — may still bill an external API, advance a flow's state
machine, or move a database row. Nothing observable says so. That is why the
default draft emits `comparison: { enabled: false }` for *every* route, with the
suggestion riding as a comment, and why `--adopt-suggestions` is a deliberate
promotion act whose precondition is that reading.

Worked reference 2 below is that claim being demonstrated rather than asserted,
on a route nobody had flagged in advance.

→ [classifying routes](classifying-routes.md) ·
[the class taxonomy](classifying-routes.md#the-class-taxonomy) ·
[what observation can and cannot tell you](classifying-routes.md#what-observation-can-and-cannot-tell-you)

### 4. Rehearse against the same upstream

Point `new_upstream` at the **legacy** host, on the routes that survived
classification, and run a full shadow campaign against a `new` that does not
exist yet. This is the stage most teams do not think to run, and it is the one
that de-risks everything after it.

What it proves, cheaply and before any rewrite exists:

- **Every comparison should match.** A mismatch in this configuration is not
  backend divergence — there is only one backend — it is a Limen config error,
  a contract rule that normalizes something it should not, or a genuinely
  non-deterministic route you misclassified. Each of those is far easier to
  diagnose now than mixed in with real divergence later.
- **The recording pipeline is real.** The sink canary rides `compare` → observer
  fan-out → sink writer thread → flush → report exactly as it will in the real
  campaign, so `verdict --canary` and `report --format html` are exercised for
  real, not rehearsed in theory.
- **The traffic corpus is adequate.** Floors that go unmet here go unmet later
  too. Finding that out now costs a re-run of the driving traffic; finding it
  out during the parity campaign costs a re-run of the campaign.

!!! warning "A rehearsed write is a doubled write"
    Shadowing is off by default for every method but `GET`/`HEAD`, and that
    default is what keeps this stage safe. If a route opts a write in with
    `comparison.shadow_methods: ["POST"]` while `new_upstream` points at the
    legacy host, the second leg lands on the **same system of record** as the
    first — the side effect is duplicated against production, not compared
    against a replica. Rehearse writes only where the side effect is provably
    idempotent, or do not rehearse them at all. Neither the same-upstream
    configuration nor Limen itself can detect the difference.

→ [comparison & contracts](comparison-and-contracts.md) ·
[`comparison.shadow_methods`](../reference/config-reference.md#comparisonshadow_methods-shadowing-a-write)

### 5. Verdict — a typed exit code, canary-backed

"Zero mismatches" is not a gate. It is satisfied equally by *compared
everything, found nothing*, by *compared nothing*, and by *compared plenty while
the sink silently dropped every record*. Stop traffic, then take the gate:

```bash
limen verdict -c campaign.config.yaml --canary --format json > verdict.json
```

Two disciplines are what make a clean exit mean anything. **Floors prove
something was compared** — they count comparisons, not coverage, so
`min_comparisons: 20` is met by 20 of 20 exactly as by 20 of 20,000. **The
canary proves the recording pipeline is live**, because an empty sink and a
correctly empty sink render identically. Run `--canary` in every campaign; a
standing check that only runs when someone remembers is not a standing check.

→ [prove your lens bites](prove-your-lens-bites.md) ·
[CLI → `verdict`](../reference/cli.md#verdict)

### 6. Report — the page is downstream of the gate

Take the verdict **first**, then render the page from the artifacts it left, so
the page can never disagree with the gate it renders. `report --format html`
produces one self-contained page — no JavaScript, no external references — that
cross-checks its inputs rather than trusting them: sink counts against the
verdict's per-route map, verdict floors against the config's effective floors,
every route id against the config's route table. A missing input is INCOMPLETE;
an unreadable one, or a cross-check drift, is FAILURE.

The page exits `0` whenever a page was produced, including a page of nothing but
failures. **It is not the gate** — a CI artifact that vanishes on a bad run is
one nobody looks at.

→ [CLI → the HTML status page](../reference/cli.md#the-html-status-page)

### The contract leg

Stages 1–6 are the runtime half. The behavioral contract they consume —
`ignore_paths`, `normalize_timestamps`, `set_cookie`, the rest of the comparison
vocabulary — is authored and refined against the *pre-production* half of the
approach, where a deterministic test suite drives the new service against legacy
before any live traffic sees it. That work, and the scenario authoring around
it, belongs to Pharos: <https://charliek.github.io/pharos/>. The two tools share
the contract file unchanged and have no build-time dependency on each other.

## Worked reference 1 — the slauth dual-lens campaign

The first real service behind Limen was slauth, a centralized authentication
service being ported from Python (FastAPI) to Rust (axum). Both implementations
were complete and running side by side over the same database — the "dual lens"
— and the campaign's job was to prove the port was behaviorally identical before
the cutover.

The shape of it: a **20-route table** covering the full API surface (health and
discovery documents, JWKS, session and config reads, the OAuth provider's login
and consent hops, the reverse-proxied Kratos and Hydra paths), of which **14
routes carried comparison enabled** and cleared their floors. The remaining six
were relay-only by classification, not by omission — the one-time-token hops and
the flow-accepting redirects that the class taxonomy exists to carve away. The
final verdict was **exit `0`**: drained, floors met, sink integrity clean, the
canary landed, **zero non-canary mismatches**. That verdict is the parity
evidence the cutover decision rested on.

It is also the campaign that produced most of the doctrine on
[classifying routes](classifying-routes.md) — including the
[worked example](classifying-routes.md#worked-example-shape-beats-name) in which
three flow-accepting redirects survived being shadowed by accident rather than
by design, which is why the redirect rule no longer requires a `Set-Cookie` to
fire.

**What it did not prove.** The traffic was driven by a functional harness
against a single local tenant: one identity, one browser session, no concurrent
users, no production load mix, no long tail of real-world request shapes. Every
route's evidence is therefore evidence about *that* corpus. A route can be class
A against a single-tenant corpus and class E against a concurrent one — the
taxonomy says so explicitly — and slauth's campaign never exercised the
difference. Nor did it exercise volume: 75 comparisons across 14 routes is
enough to clear floors and catch a systematic divergence, and is not a
statement about behavior under load.

## Worked reference 2 — the tapper cold-start field test

The slauth campaign was run by the people who built the tooling, on a service
they knew. The obvious question was what the same path costs someone who has
neither. So it was measured: a fresh agent session, briefed with **nothing but
the embedded `migrate-a-service` skill**, was pointed at *tapper* — a 19-route
FastAPI service including a real SSE streaming endpoint — and told to take it
from nothing to a rendered status page.

It did, in **8 minutes 28 seconds** of wall clock, warm build cache, with **zero
interventions**.

| Stage | Wall clock | Result |
|---|---|---|
| Observe | 3m35s | 19 routes profiled |
| Draft | 20s | 7 `compare_candidate` · 1 `compare_narrowed` · 11 `relay_only` |
| Classify | 1m35s | 3 demotions against the handler source |
| Same-upstream rehearsal | 20s | 128 comparisons, 0 skips |
| Verdict | 1s | exit `0`, canary landed |
| Report | 22s | one self-contained HTML status page |

Four things that run taught, none of which were engineered to happen.

**The doctrine's central claim, demonstrated.** One route's observational record
was flawless: 6 reads, `2xx` only, a stable `Content-Length`, no cookie, no
redirect, one path. The classifier suggested `compare_candidate`, correctly — on
that evidence there was nothing to demote it for. Reading the handler source, as
the skill mandates before promoting any candidate, showed that the route called
a **paid text-to-speech API on every cache miss**. Shadowing it would have
double-billed an external vendor on live traffic, and no amount of additional
observation would ever have said so. It was dispositioned class D — a write in
GET clothing — and left `relay_only`. This is
[what observation can and cannot tell you](classifying-routes.md#what-observation-can-and-cannot-tell-you)
happening to someone, once, in the wild.

**The SSE endpoint landed relay-only, and the guard was proven to fire early.**
tapper's chat endpoint streams `text/event-stream`, and the streaming doctrine
is unambiguous: never enable comparison on a streaming route. It was
dispositioned relay-only. A separate diagnostic then confirmed *where* the
mechanical backstop sits: the `event_stream` guard fires on the primary's
**response headers**, before a byte is buffered and before the shadow leg is
dispatched. The shadow plan is dropped, so no duplicate request ever leaves the
proxy, and the stream relays to the client intact. The guard is a backstop, not
a plan — but it is a backstop that engages before the expensive, duplicating
part of the pipeline, not after.

**The cardinality and token rules earned their place.** R6 (a one-time-token
query name), R7 (reads spread over more distinct paths than the ceiling) and R8
(nearly every read hitting a distinct path) each fired on real traffic shapes
this service produced, not on synthetic fixtures. When `--adopt-suggestions` was
finally run, it promoted exactly one route — the stable control route — which is
the correct outcome for a service whose remaining candidates had not yet had
their sources read.

**Counters are not the gate, and a mid-flight scrape shows why.** A `/metrics`
scrape taken while traffic was still settling read **8 shadows dispatched
against 7 comparisons recorded** — a discrepancy that looks alarming and means
nothing, because the eighth comparison was in flight. `limen verdict`'s drain
step waits for the pipeline to quiesce before scoring anything, and resolved it
to **8/8**. Any campaign wrapper that gated on a raw counter scrape instead of
the verdict would have flagged a phantom failure here.

## What remains — the honest ledger

Everything above is tested. The following is not, and this page would be worth
less if it did not say so.

**Rollout is not part of the tested path.** Percentage split, runtime flag
flips, and breaker fallback to legacy are implemented and unit-tested, but no
service has been rolled out through them. Rollout *simulation* — driving a
campaign through the 0 → 1 → 5 → 25 → 50 → 100 ladder and proving the
budget-recheck loop under real conditions — is a later roadmap phase. Read
[flags & rollout](flags-and-rollout.md) and
[resilience & failover](resilience.md) as design documentation, not as field
reports, and do not promise the ladder to a team on the strength of this page.

**No production load mix has fronted Limen.** Both campaigns drove functional
traffic from a harness. Nothing has yet sat behind a real load balancer taking
real user traffic at real concurrency, which means the throughput cost of
observe mode's response-path lock, the shadow leg's concurrency bounds, and the
buffer-for-compare path's first-byte delay are all bounded *by design* and
unmeasured *in practice*. Validate throughput with observe mode on before
enabling it broadly.

**The JVM target fleet is untested.** Both tools are HTTP-agnostic by
construction — Limen sees status codes, headers, and bodies, and has no
knowledge of what produced them — so nothing in the design distinguishes a Java
or Kotlin upstream from a Python one. That is a strong argument, and it is still
an argument rather than an observation. The two services proven so far were
Python, Rust, and Python again.

**Route-template matching is a real granularity limit.** A route matches on
`path_prefix`, which means `/x/{id}` and `/x/{id}/sub` fold into one route
whenever the prefix covers both. Classification suffers (one profile, one
disposition, aggregated across paths that may not share a safety class) and so
does routing granularity (you cannot roll out the two independently). The
recorder deliberately stores path *hashes* rather than paths, so no rule can
even see the fold happening —
[sub-path aliasing](classifying-routes.md#what-observation-can-and-cannot-tell-you)
is the residual with no traffic-side fix. Keep route granularity a human
decision, and never widen a `path_prefix` to make more traffic land in one
class.

**An all-non-2xx corpus is not flagged.** The classifier records status classes
as evidence but no rule demotes on them, so a route whose entire observation
window is `404`s can reach `compare_candidate` on the strength of stable,
repeated, danger-signal-free reads — of an error page. The suggestion is not
wrong about what it saw; it is a hypothesis about a route nobody successfully
called. Read the status mix in the profile before adopting any candidate, the
same way you read the handler source.
