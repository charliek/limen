# Prove your lens bites

A migration campaign's headline number is almost always "zero mismatches."
That number is worthless on its own. It is satisfied just as well by "compared
everything, found nothing" as by "compared **nothing**" — and, less obviously,
by "compared plenty, but the recording pipeline silently dropped every
record before it reached the report." This page is about the discipline that
turns a clean [`limen verdict`](../reference/cli.md#verdict) into actual
evidence instead of an absence of evidence.

## The problem: a clean verdict can mean nothing happened

A zero-mismatch report is only evidence to the extent that:

1. **Something was compared.** A route with `comparison.enabled: false`, a
   `sample_rate` of zero, or traffic that never reached it contributes nothing
   — and reports the same "no mismatches" as a route compared thousands of
   times.
2. **The recording pipeline demonstrably works.** Comparisons happen
   in-process and increment counters synchronously, but the durable trail —
   the diff sink — is a separate async pipeline (a bounded channel, a
   dedicated writer thread, a file). If that pipeline is silently broken, the
   counters can be non-zero while the sink that should back them up is empty,
   and a report reading the sink alone would show a clean run that never
   happened.

Neither condition follows from "exit 0." `limen verdict` exists to check both,
explicitly, every time, rather than relying on an operator's memory of "yes, I
confirmed comparisons were running" from three campaigns ago.

## Three disciplines

### 1. Floors: prove something was compared

Every route's `comparison.min_comparisons` (default `1`) is a floor: the
minimum number of comparisons `limen verdict` requires that route to have
recorded before its floors check can pass. A route that never cleared its
floor makes the whole verdict exit `20`, regardless of how clean everything
else looks — a route that never compared cannot contribute a mismatch, so a
clean total proves nothing about it.

A config in which **no** enabled route carries a non-zero floor fails the
floors check outright. This is the vacuous-pass refusal: a verdict over a
config that compares nothing proves nothing, and a wrapper that let that
through would be reporting "clean" about a no-op.

Two things worth internalizing about floors:

- **Floors count comparisons, not traffic coverage.** `min_comparisons: 20`
  is satisfied by 20 comparisons out of 20 eligible requests just as much as
  by 20 comparisons out of 20,000 — `comparison.sample_rate` decides how much
  of eligible traffic gets buffered and compared in the first place. A floor
  is a **did this route get exercised at all** check, not a coverage
  percentage. If you need coverage assurance, that is a `sample_rate` and
  traffic-volume decision made before the campaign runs, not something the
  floor itself expresses.
- **`min_comparisons: 0` is a visible opt-out, not a silent gap.** Some
  routes genuinely cannot be exercised in a given campaign topology (a
  read-only test harness never driving a write-gated route, for instance).
  Setting the floor to `0` for that route is the honest way to say so — it
  shows up in the config, in `limen print-routes`, and in the verdict's floor
  list as an explicit, reviewable exemption, not as a route that quietly
  never came up.

### 2. The sink canary: prove the recording pipeline is live

Floors prove traffic reached the comparison engine. They say nothing about
whether the *sink* — the durable, replayable trail everything downstream
(triage, `limen report`, this campaign's own verdict) depends on — is
actually working. A campaign with real mismatches would eventually notice a
broken sink when the report came up suspiciously empty; a campaign with
**zero** real mismatches never would, because an empty sink and a correctly
empty sink render identically.

The debug sink canary (`debug.sink_canary: true`, `POST /debug/canary`,
driven by `limen verdict --canary`) closes exactly that gap. It injects one
deliberately mismatching synthetic response pair through the **real**
`compare` → observer `Fanout` → sink writer-thread → flush pipeline, under
the reserved route id `__limen_canary__`, and then verdict's canary check
confirms the record actually landed (sink count equals the engine's
`__limen_canary__` mismatch counter, and both are at least one).

Be precise about what this proves and what it does not:

| The canary proves | The canary does **not** prove |
|---|---|
| `compare::compare` still flags a divergent pair as a mismatch | Any real route's shadow request was actually dispatched |
| The observer fan-out reaches every registered observer (metrics + sink) | Any real route's comparison rules (JSON normalization, header lists, `Set-Cookie`/`Location` handling) are correctly configured |
| The sink writer thread is alive and its channel is connected | The data-plane shadow leg — upstream selection, timeouts, body capture — is working |
| A record offered to the writer reaches the daily file and is flush-visible | Anything about traffic volume or coverage |

The canary is a **standing check on the tooling**, deliberately independent
of the config under test — it does not go through any route's comparison
rules, so it stays meaningful even when every real route floors at zero. The
data-plane shadow leg and the comparison rules themselves are exactly what
**real traffic plus floors** cover — the two disciplines are complementary,
not redundant. Run with `--canary` in every campaign wrapper (see below); a
canary check that only runs "when someone remembers" defeats the point of it
being a standing check.

### 3. Periodic falsification: prove the comparison itself is live

Floors and the canary both assume the comparison rules in the config are
correctly wired to actually compare what you think they compare. Neither
checks that assumption directly — a rule that silently compares nothing (a
misconfigured `json.ignore_paths`, an accidentally-disabled `compare_body`)
would still let traffic flow, still increment `limen_comparisons_total`, and
still clear every floor, while never actually catching a real divergence.

The only way to check that is to introduce a divergence you know about and
confirm the verdict notices. This is falsification: temporarily mutate the
config or the sink in a way that has one specific, predicted effect on the
verdict's exit code, run it, confirm the prediction, then revert. The
mutations are throwaway — never committed — and exist purely to exercise the
"would this have caught something" question the zero-mismatch report cannot
answer on its own.

A representative falsification pass, generalized from a real two-backend
migration campaign:

| Mutation | Predicted verdict | Why |
|---|---|---|
| Loosen a compared route's comparison rule (e.g. drop a normalization or an ignored query param) so a known difference stops being masked | exit `10` (mismatches found) | Proves the rule was actually doing the masking, not just present in the config |
| Set a tiny `max_body_bytes` on a compared route so comparisons stop happening entirely | exit `20` (floors unmet) | Proves the floor genuinely depends on comparisons occurring, not on the route merely being configured |
| Append a corrupt/truncated line to a sink JSONL file before running `verdict` | exit `30` (sink integrity) | Proves `malformed_lines` really gates the verdict rather than being reported and ignored |

Each mutation targets exactly one check and predicts exactly one code. Revert
every mutation immediately after confirming the flip (`limen validate-config`
and `limen check-contract` clean afterward is the "you put it back correctly"
check), and re-run once more to confirm the verdict returns to its prior
state.

Falsification is not a per-campaign-run tax — running it every time would be
expensive and would not actually buy more confidence than the last pass did.
Run it **when the contract vocabulary changes materially**: a new comparison
rule kind, a change to how normalization or redaction works, a new mismatch
`kind` — anything that could plausibly change what "the comparison rule
caught it" means. A campaign whose contracts haven't changed since the last
falsification pass is still covered by it.

### 4. Skips: prove nothing sampled slipped through uncompared

The first three disciplines all assume that once a request is selected for
comparison, `limen verdict` can tell whether it actually got compared. That
assumption used to be optimistic. A response over `comparison.max_body_bytes`,
a shadow the global `server.shadow_concurrency_limit` had no room for, an
event-stream response, a primary that answered too slowly to finish
buffering, a shadow that never answered at all — every one of these is
sampled work that never reached the comparison engine, and `limen verdict`
printed all of them under "inspected, not gating." A route could clear its
floor on the comparisons it did make while the campaign quietly lost an
arbitrary share of the ones it planned, and the exit code never moved. A
floor met alongside hundreds of shadow timeouts on the new upstream is not
evidence either — it's a coincidence wearing evidence's clothes.

The rule this closes, stated once so it generalizes past this specific list:

> **A skip must either count against the gate or appear in the verdict —
> never neither.**

For a floored route, "count against the gate" is now literal. `floor_met` still
answers the old, narrower question — did the comparison count reach the
floor. `met` is the one the exit code reads, and it additionally requires that
**no sampled work on the route went uncompared**, for any reason. The check's
detail names a route **starved** when it never reached its floor and
**undermined** when it reached the floor but lost sampled work along the way.
They are different failures with different fixes, which is why the verdict
tells you which one you have rather than collapsing both into "floors unmet."

Every reason a route can be undermined has exactly one remedy, kept here in
the same words `limen verdict` prints (`remedy_for` in `src/verdict.rs` is
the source both the tool and this table are read from, so they cannot drift
apart):

| Reason | Family | Remedy |
|---|---|---|
| `response_too_large` | `limen_comparison_skipped_total` | Raise `comparison.max_body_bytes` on this route (default `262144`); the largest bodies are the ones that went uncompared. |
| `request_too_large` | `limen_shadow_skipped_total` | Only on routes with `shadow_methods`: the opted-in write's request body exceeded `comparison.max_body_bytes`; raise it or drop the opt-in. |
| `concurrency_limit` | `limen_shadow_skipped_total` | `server.shadow_concurrency_limit` is **global**: a slow-on-`new` route can hold a slot until its shadow completes or `shadow_ms` expires, so the route that consumed the slots may not be the route the skip is charged against — raise the cap or lower drive concurrency. |
| `response_buffer_timeout` | `limen_comparison_skipped_total` | The primary answered too slowly to buffer within `timeouts.primary_ms`; raise the budget or lower the load. |
| `event_stream` | `limen_comparison_skipped_total` | **This route can never meet a floor while it has one.** Every sampled request skips on content type before a byte is buffered, so the floor is unsatisfiable by construction — unfloor it (`min_comparisons: 0`) and relay it (`legacy_only`), rather than burning a campaign discovering this the hard way. |
| `timeout` / `error` | `limen_shadow_failed_total` | The new upstream failed to answer N shadow(s) — a finding about `new`, not tooling noise; read its logs before re-running. |

One rule sits underneath every remedy in that table and is easy to get wrong
under pressure: **the counters behind this check are cumulative for the life
of the process — they never reset, and `limen verdict` reads their absolute
value from a single scrape.** A single skip recorded an hour into a campaign
fails every verdict you take for the rest of that process's life, however
clean the traffic has been since. The fix is therefore always three steps,
not two: change the knob that caused the skip, **restart limen**, and
re-drive the floors from zero. "Change the knob and re-run `limen verdict`"
against the same running process stays exit `20` forever — the skip already
happened, and the counter that recorded it is still there to fail the next
scrape, and the one after that.

## Why exit codes are typed, not prose

Before `limen verdict` existed, campaign wrappers parsed `limen report`'s
human-readable output and their own ad hoc metrics scrapes to decide pass or
fail — brittle, and impossible to review at a glance across runs. Typed exit
codes (`0/10/20/30/40/50`, documented in the [CLI reference](../reference/cli.md#verdict))
turn "did this campaign prove anything" into a value a shell `case` statement
can branch on directly, and — because the codes are distinct per failure
class — into something a wrapper can *react* to differently: a `20` (floors
unmet) might mean "extend the test corpus" for a starved route or "raise
`max_body_bytes` and re-drive" for an undermined one (§4, above), while a `30`
(sink integrity) means "stop and investigate the sink, don't trust any of
these numbers."
Collapsing every failure into a generic non-zero exit would throw that
distinction away at exactly the boundary where a human has to decide what to
do next.

## The fresh-sink-per-start assumption

`limen verdict`'s sink-integrity check reconciles the sink's per-route counts
against the engine's live `limen_comparisons_total{route,result="mismatch"}`
counters. That reconciliation assumes **the sink directory was reset when the
proxy under test started** — the same assumption both of limen's own
campaign consumers already make. If the sink instead accumulates records
across multiple proxy starts (or multiple campaigns), the counts diverge:
the engine's counters reset with the process, the sink's file contents do
not, and verdict correctly reports that as a sink-integrity failure. This is
fail-closed and intentional, not a bug to route around — a campaign wrapper
that wants a clean verdict must reset (or point `--dir` at a fresh) sink
directory before starting the proxy under test, every time.

## A worked wrapper

The pattern every campaign wrapper should follow: reset the sink, start the
proxy with `debug.sink_canary: true`, drive traffic, then run `verdict`
**before** rendering any human-readable detail view (so the detail view
reads the already-drained sink and the two can never disagree):

```bash
#!/usr/bin/env bash
set -euo pipefail

rm -rf ./campaign-diffs
mkdir -p ./campaign-diffs

# Config carries `diff_sink: { dir: ./campaign-diffs }` and
# `debug: { sink_canary: true }`.
limen run -c campaign.config.yaml &
limen_pid=$!
trap 'kill "$limen_pid" 2>/dev/null || true' EXIT

# ... wait for readiness, drive the campaign's traffic ...

limen verdict -c campaign.config.yaml --canary --format json \
  | tee ./campaign-diffs/verdict.json
verdict_exit=${PIPESTATUS[0]}

# The detail view, read only after the verdict above has already drained
# the pipeline — its __limen_canary__ row is expected.
limen report --dir ./campaign-diffs --format human

exit "$verdict_exit"
```

`jq` against `verdict.json` gets you the per-check detail
(`.checks.floors.detail`, `.floors[]`, `.sink_mismatches_by_route`) for
whatever summary format your CI wants; the exit code alone is enough to gate
on.
