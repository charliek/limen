# Classifying routes

Before a route is shadowed, split, or ever pointed at a `new` upstream, someone
has to decide what "equal" means for it — and, for a growing number of routes,
whether it may be compared at all. That decision is route classification, and
it is the single most expensive intellectual step in a migration campaign. Get
it wrong in the unsafe direction and Limen faithfully shadows a request that
was never safe to repeat.

This page generalizes that method from a real two-backend migration campaign
into something any service can follow: a class taxonomy, the questions that
assign a route to a class, a catalog of the shapes that go wrong, and an honest
account of what traffic observation can and cannot establish on its own. [Limen
observe mode](observe-mode.md) automates the traffic side of this — gathering
evidence and demoting routes that show a danger signal — but the classification
itself is a human judgment informed by the service's source. No tool replaces
that judgment; observe mode exists to make it cheaper to reach.

## Why classification is the expensive step

Everything else about a migration is comparatively mechanical: config for two
upstreams, a comparison rule, a rollout percentage. What is *not* mechanical is
knowing, route by route, whether comparing (or shadowing) it is safe — and that
question cannot be answered by staring at a URL. `GET /orders/42` and
`GET /orders/42/mark-read` look identical to a router: same method, same
general shape, same plausible 200 JSON response. Only one of them is safe to
send twice.

Classification is expensive because it requires reading the service's source,
or driving its traffic and reasoning carefully about what came back — and doing
that for every route, not just the obviously risky ones. Skipping it does not
remove the cost; it moves the cost downstream, to an incident where a shadowed
request silently doubled a side effect. The rest of this page is the method
that keeps that cost visible and bounded instead of hidden and unbounded.

## The class taxonomy

Six classes cover the traffic shapes a migration campaign runs into. They are
ordered from safest to most dangerous to compare, and each is service-agnostic
— the campaign this page draws from used a specific route table, but nothing
below depends on it.

### A — deterministic reads

**What it is.** A read whose response is a pure function of the resource it
names and the backing state both upstreams share. No per-request randomness,
no server-side side effect, nothing minted.

**How to recognize it.** A `GET`/`HEAD` against a resource identified entirely
by the path (or a deterministic query), where the same request repeated
against the same backing state returns the same body. Health checks,
discovery documents, and read endpoints over shared, already-migrated state
are the common cases.

**What to do with it.** Shadow and compare fully — status, headers, and body.
This is the class the rest of the taxonomy exists to carve away from; a
mismatch here is unambiguous backend divergence, which is exactly the evidence
a migration campaign is for.

### B — flow-creating reads

**What it is.** A read whose *request* mints something server-side — a flow
record, a challenge, a session-bound identifier — at a shared identity or
authorization system, even though the read itself doesn't consume anything.
The endpoint is idempotent in the sense that repeating it doesn't break
anything, but its response is not deterministic: each call creates a fresh
per-request artifact.

**How to recognize it.** The response (or a redirect target's query string)
carries a freshly minted, single-use-looking identifier — a flow id, a
challenge value — that differs on every call even against unchanged backing
state. Two otherwise-identical requests never come back byte-identical.

**What to do with it.** Shadow and compare, but narrow what "equal" means: a
contract that ignores or normalizes the minted field, or compares its presence
rather than its value. Comparing a class-B route with no narrowing manufactures
mismatches that have nothing to do with backend divergence.

### C — one-time-token hops

**What it is.** A request that itself carries and *consumes* a single-use
credential minted by an earlier step — a verifier, a challenge, an
authorization code. The primary leg's use burns the token at the shared
backend; a shadow's replay of the same token is not a parallel observation, it
is a guaranteed failure, because the token is now gone.

**How to recognize it.** A query parameter or path segment names something the
service validates and invalidates in one motion — the two legs racing to
consume the same one-time value, where only the first can win. If the primary
already succeeded, the shadow's identical request cannot.

**What to do with it.** Never shadow. A guaranteed mismatch on every occurrence
teaches nothing about backend parity and drowns real signal in expected noise.
Where a proxy genuinely cannot scope the hop out of a wider route (a known,
temporary tool gap), comparing it *on purpose*, as a stopgap to capture that
the token really is single-use, is defensible — but the destination is always
relay-only once the route can be scoped.

### D — writes in GET clothing

**What it is.** A request shaped like a read — typically a `GET` — that has a
side effect: accepting a pending challenge, advancing a flow's state machine,
minting a cookie or session, consuming a one-time authorization. The mutation
rides the read verb because the surrounding protocol (an OAuth/OIDC provider
flow, an email-link confirmation, a bookmarkable "accept" URL) demands it.

**How to recognize it.** The strongest signal is behavioral, not lexical: a
3xx status, a `Location` header, or a `Set-Cookie` on what is nominally a read.
Do not rely on a query parameter's *name* to catch this class — see [the
worked example](#worked-example-shape-beats-name) below for why that is a
documented, not a hypothetical, failure.

**What to do with it.** Never shadow, unconditionally. The hazard is not the
shadow racing its own primary — Limen sequences the shadow after the primary
succeeds — but the shadow's own *re-acceptance* racing whatever the flow's next
hop does at the shared backend, or double-consuming state the primary already
consumed. Neither outcome is worth the evidence gained, and both are avoidable
by simply not sending the second request.

### E — reads racing writes

**What it is.** A read whose correct answer depends on a write from a nearby
step in the same scenario — the read is safe and deterministic once that write
has landed, but a shadow completing asynchronously can observe the backend
*before* that write, or interleaved with a later one, producing a mismatch
that reflects timing, not backend divergence.

**How to recognize it.** The route reads data that a step elsewhere in the same
driving flow writes, and the shadow's response window (bounded by its timeout)
is wide enough to straddle that write. This is a property of the *traffic*,
not the route in isolation — the same endpoint can be class A against one
corpus and class E against another.

**What to do with it.** Shadow and compare, but treat an isolated mismatch here
as a flake candidate first, a divergence second. Pacing the driving traffic so
each step's writes land before the next step's reads fire reduces this
close to zero; it does not make the race structurally impossible, so a
re-run policy is the honest backstop.

### F — writes

**What it is.** Any request with a side effect that isn't disguised as a read
— the plain mutations: creates, updates, deletes, revocations, token
issuance.

**How to recognize it.** A non-`GET`/`HEAD` method is the strong signal; Limen
itself never shadows a write by default, only `GET`/`HEAD` are eligible
without an explicit opt-in ([`comparison.shadow_methods`](../reference/config-reference.md#comparisonshadow_methods-shadowing-a-write) —
`POST`, `PUT`, or `PATCH`; `DELETE` is not eligible at all, opt-in or not).

**What to do with it.** Leave unshadowed. Shadowing a write doubles its side
effect against a system of record — a second charge, a second token, a second
revocation of state a later step still needs. The narrow exception is a write
that is provably idempotent, whose side effect is not response-visible to
anything the campaign observes, and whose author has made that argument
explicitly in the config, one route at a time. That is an opt-in, never a
default, and it belongs beside the route as a comment explaining why. Being
one of the methods `shadow_methods` accepts is necessary but not
sufficient — the allowlist only says a method is *mechanically* eligible; it
is this per-route argument that actually justifies opting in.

## The decision questions

Assigning a route to a class is a short, ordered set of questions — ask them in
this order, because an earlier "yes" pre-empts a later class the same way the
[classifier's rule table](observe-mode.md#reading-a-suggestion) always resolves
the safer disposition first:

1. **Does the route mutate at all?** Any write, however small → class F. Stop;
   this route is never shadowed by default.
2. **Does the request consume something that can only be used once?** A
   verifier, an authorization code, a challenge the backend invalidates on use
   → class C. Stop; never shadow.
3. **Does the request have a side effect expressed through a read-shaped call?**
   Check the response shape (3xx, `Location`, `Set-Cookie`) *and* the service's
   source — the shape is necessary evidence but the source is the only place
   that proves intent → class D. Stop; never shadow.
4. **Does the response contain something freshly minted on every call**, even
   though nothing is consumed? → class B. Compare with narrowed equality.
5. **Does the route's correctness depend on timing relative to a nearby write
   in the same flow?** → class E. Compare, but treat mismatches as flakes
   first.
6. **None of the above?** → class A. Compare fully.

Every "stop" above is deliberate: classes C, D, and F are never comparison
candidates by policy, regardless of what a traffic sample happens to show
about them on a given day. That is the same posture [observe
mode](observe-mode.md) encodes mechanically — the rules for those shapes land
on relay-only and stay there.

## The sharp-edge catalog

Six shapes account for most of the classification mistakes worth naming
explicitly, because each one has bitten a real campaign in a way that a naive
first pass at the taxonomy did not anticipate.

| Sharp edge | What it looks like | Why it is dangerous | Class |
|---|---|---|---|
| One-time-token hops | A query parameter or path segment names a verifier, challenge, or code the backend invalidates on first use. | The shadow's replay is a *guaranteed* failure, not a parallel observation — it proves nothing and drowns real signal. | C |
| Writes in GET clothing | A bare redirect (3xx, possibly with no cookie at all) or an otherwise read-shaped call that accepts a pending flow step. | Comparing it plainly (no `Location`/`Set-Cookie` narrowing) can shadow *and compare clean*, hiding the mutation entirely — and re-sending it can break the flow at the shared backend. | D |
| XFF-keyed behavior | A route whose behavior — a rate limit bucket, a geo decision — is keyed off a forwarded-client-address header the *test harness* sets synthetically to isolate scenarios. | Any correct reverse proxy collapses synthetic `X-Forwarded-For` bucketing the same way a real one would from behind a shared edge, so the route's behavior through the lens legitimately diverges from behind it — a false mismatch that traffic shape alone cannot distinguish from a real one. | — (harness-dependent; exclude from comparison rather than classify) |
| Wildcard-proxy granularity | One route config (a path prefix, a catch-all) actually serves many distinct underlying paths — some safe to compare, some not — folded into a single classification decision. | Classification is inherently per *route*; safety is inherently per *path*. A route this coarse can carry a minority of mutating traffic that never moves the aggregate signal enough to demote the whole route. See [sub-path aliasing](#what-observation-can-and-cannot-tell-you) below — this is the one sharp edge with a config-side fix, `match.path_template`. | any |
| Wildcard-shaped templates | A `path_template` most of whose segments are parameters (`/api/{a}/{b}/{c}`) rather than literals. | The template still absorbs whatever cardinality R7/R8 exist to catch — quiet path-count rules mean the shape was named once, not that the operation is narrow. See [the wildcard-template residual](#what-observation-can-and-cannot-tell-you) below. | any |
| Error-only corpora | Every read the route ever answered came back 4xx/5xx — the route has never once shown what it returns when it works. | A fixed-length error page can still satisfy the raw repeat-evidence signal, so absent a rule watching status class, a route with zero successful reads could still reach `compare_candidate` on evidence that only shows how it fails. See [R8a](#what-observation-can-and-cannot-tell-you) below. | — |

## What observation can and cannot tell you

This is the honest core of this page, and it holds regardless of how much
traffic a campaign drives or how careful the harness is.

**Response metadata can prove a route UNSAFE to compare. It can never prove one
SAFE.** `GET /orders/42` and `GET /orders/42/mark-read` are indistinguishable
from the outside when both return a stable 200 JSON body — nothing observable
about the response tells you the second one moved a database row. Every rule
in [observe mode's classifier](observe-mode.md) is built around this asymmetry:
it demotes on danger signals and never asserts safety, because the traffic
cannot support that assertion. A route reaching `compare_candidate` is a
hypothesis carrying evidence, not a verdict — a human still has to read the
source before enabling comparison.

**Sub-path aliasing is the sharpest residual, and it is not closable from
traffic.** Classification happens per *route*; mutation happens per *path*. A
route matching a prefix like `/orders/` folds `GET /orders/42` and
`GET /orders/42/mark-read` into one profile — one aggregate of methods,
status codes, and stability signals across every path underneath it. Worse,
the aggregate has no way to see the fold happening: the recorder deliberately
stores path *hashes*, never paths, so it can count how many distinct paths a
route served without ever writing a user-identifying string to the control
plane. No classification rule can see which sub-paths a route's reads actually
hit, because that information was never recorded in the first place — refusing
to record it is what makes the profile safe to expose, and it is exactly what
makes this residual unfixable *from traffic*. This is the single
strongest argument for keeping route granularity a human decision, and for
never letting a tool draft a comparison-enabled route on the strength of
traffic shape alone.

**The lever against it lives in config, not in observation.**
[`match.path_template`](../reference/config-reference.md#matchpath_template-route-by-shape-not-just-by-subtree)
turns the fold itself into a decision the operator makes explicitly: splitting
`/orders/` into `/orders/{id}` and `/orders/{id}/mark-read` makes the two
operations two routes with two profiles, each classified on its own evidence.
No traffic shape can make this split for you — the recorder still cannot see
which path a read hit, so a tool can never *infer* the boundary — but once the
config draws it, R7 and R8 read `distinct_read_paths` per *operation* rather
than per prefix, and a route that used to hide a minority of mutating traffic
inside a majority of safe reads becomes two routes the classifier can actually
tell apart.

**A template does not, on its own, guarantee narrow granularity.** Absorbing
path cardinality is what a template is *for* — `/conversations/{id}` reports
one distinct path however many ids it served — but a template whose segments
are almost entirely parameters (`/api/{a}/{b}/{c}`) absorbs just as much
cardinality while naming almost nothing. R7 and R8 go quiet on a shape like
that by the same mechanism that makes them go quiet on a well-scoped template,
so their silence is not evidence the operation is narrow, only that it was
named once instead of counted per id. R10 (`no-repeat-evidence`) is the net
that remains: candidacy still requires a *concrete* request to repeat with a
stable length, so a wildcard-shaped template with no actual repeat traffic
still lands short of `compare_candidate` even though the path-count rules
never fired.

**The `Content-Length` stability signal fails in both directions, and only one
of them is safe.** Absent a body byte to inspect — reading one would delay
every client's first byte, which observe mode refuses to do — the only free
stability signal is whether a repeated request's response length stays
constant. That signal is honest about its own limits:

- Two different bodies of identical length (padded JSON, a fixed-size error
  page) look stable when they are not — a **false non-demotion**. This is why
  stability is treated as *necessary* evidence for candidacy, never
  *sufficient* on its own.
- Two requests carrying different credentials share the same fingerprint (the
  fingerprint never includes header or cookie values), so a genuinely safe,
  per-user response can look "varied" and get demoted — a **false demotion**.
  This is the safe direction: it costs evidence, not safety.

**A response with no `Content-Length` at all is the third case, and it is
counted rather than guessed at.** An SSE stream (`text/event-stream`), a chunked
response, and any other reply that simply never declares a length all collapse
into one bucket — `length_missing` in the profile — because the signal has
nothing to compare against a previous sighting. Observe mode narrows a route on
**any** such read, not only when every read lacked a length: one length-less
read means that route's stability evidence is incomplete, and incomplete
evidence about body trustworthiness is a reason to compare status instead of
body, never a reason to assume the body is fine. That is the same safe direction
as the false demotion above — it costs evidence, not safety.

Do not over-read it as a runtime prediction, though. Of these shapes only a
`text/event-stream` response is declined outright at comparison time, because it
can never complete. A chunked response, or one that simply omits the header, is
buffered and compared exactly like any other as long as it finishes inside the
route's size and time bounds. A missing `Content-Length` is a hole in the
*profile's* evidence, not an exclusion from comparison (see [observe
mode](observe-mode.md#length-less-responses-collapse-to-one-bucket)).

**A corpus of nothing but failures cannot vouch for a route, and the
classifier now says so explicitly.** Status class used to participate in no
danger rule, so a route whose reads answered nothing but 4xx/5xx could still
repeat at a fixed length and reach `compare_candidate` on evidence that never
once showed what the operation returns when it works — a stable 404 page is
still a stable length. Tapper's phase-04 field test recorded two real
instances of exactly this: a legitimately-all-404 route (an images endpoint
whose test corpus never produced a cache hit) and a corpus poisoned by an
unrelated driver bug that made every read fail for reasons that had nothing to
do with the route's own safety. Both reached candidacy under the earlier rule
table. `no-success-evidence` (R8a) now demotes any route whose reads were
observed and never once answered `2xx`, provided at least one of them actually
reached an upstream — a route where every read was withheld by a transport
failure is [R3/R10's](observe-mode.md#reading-a-suggestion) to demote, not
R8a's, so a flapping upstream cannot manufacture the same verdict a genuinely
broken route earns; withheld evidence only withholds. Stability evidence is
success-qualified for the same reason: only `2xx` reads enter the fingerprint
map at all, because a fixed-length error page repeating says nothing about the
body an operation returns when it succeeds. That is the asymmetry the whole
classifier rests on — **an error can condemn a route, it can never vouch for
one.**

**Classification requires a full, unsampled traffic set — sampling and
classification are mutually exclusive.** The dangerous rules (a redirecting
read, a cookie-minting read, a one-time-token query name) are *existential*:
one occurrence is enough to condemn a route, no matter how many thousand
unremarkable requests surround it. Sampling drops requests wholesale, and the
rare mutating request is exactly the kind of observation sampling is most
likely to drop while an unlucky route still clears every other floor. A
profile recorded below full rate is therefore not a smaller version of the
truth — it is a version with the decisive observation possibly missing, which
is why a sampled profile is refused classification outright rather than
classified with lower confidence.

## Worked example: shape beats name

A real two-backend migration campaign's canonical writes-in-GET-clothing were
three routes that each returned a bare `303` redirect with **no `Set-Cookie`
at all**. An early version of the redirect-detection rule required *both* a
3xx-or-`Location` response *and* a `Set-Cookie` — reasoning, reasonably enough,
that a flow-accepting hop usually also mints a session artifact. Against this
campaign's real routes, that rule fired on none of the three: no cookie meant
no match, full stop.

They survived being shadowed by accident, not by design. Each route's query
parameter happened to be named with a `_challenge` suffix, which a *separate*,
name-based rule caught — the one-time-token vocabulary. Had that parameter been
named anything else (`ref`, a generic `id`, nothing distinctive at all), the
mutating traffic would have sailed straight through the conjunctive redirect
rule and become a comparison candidate: shadowed, and — because plain
comparison does not check `Location` or `Set-Cookie` by default — compared
*clean*, with no error, no mismatch, no signal that anything was wrong.

The fix was to stop requiring the conjunction. A redirecting read is the
universal shape of an interstitial flow hop, independent of whether it happens
to mint a cookie: the rule now fires on *any* 3xx status or `Location` header,
full stop. This is the page's best argument for preferring shape-based rules
over name-based ones wherever the two disagree: a name is something an
application author can rename without thinking about safety; a redirect
response's HTTP shape is what a client actually sees and cannot be refactored
away by accident. Name-based signals (the one-time-token vocabulary of
[observe mode](observe-mode.md#reading-a-suggestion)) remain useful as a
*supplement* — catching hops that carry a token without a redirect at all —
but nothing safety-critical should stand or fall on a parameter's spelling
alone.

This is also why [Limen observe mode](observe-mode.md)'s classifier treats
every mutation-suspect signal as terminal at relay-only rather than as
something a later, more specific rule could override: the lesson of this
example is not "add a rule for this one shape," it is "prefer the rule that
cannot be defeated by an unrelated naming choice." [`limen
suggest-routes`](../reference/cli.md#suggest-routes) applies exactly that rule
table to a service's real traffic; [prove your lens bites](prove-your-lens-bites.md)
covers the complementary discipline of proving the comparison pipeline built
on top of a classification is actually running once routes are wired up.
