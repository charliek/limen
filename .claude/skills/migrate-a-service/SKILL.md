---
name: migrate-a-service
description: Drive a legacy→new HTTP service migration through limen — observe, draft, classify, shadow-compare, verdict, report.
---

# Migrate a service with limen

limen is a reverse proxy that moves HTTP traffic from a `legacy` upstream to a `new` one. This is the
procedure for one service, in stage order; each stage links the page that owns the detail instead of
restating it. The whole procedure, including the Pharos contract/scenario work this skill does not cover,
is the [migration runbook](https://charliek.github.io/limen/runbook/).

**The invariant, end to end: the tools generate and recommend; a human validates intent and correctness.**
Never promote a suggestion, enable a comparison, or call a campaign green on tool output alone.
`-c/--config` also reads `LIMEN_CONFIG`, default `limen.config.yaml`; confirm any flag with `--help`.

**Getting the binary.** There is no package install — build from the limen repo. `mise exec -- cargo build
--release` from the repo root resolves the pinned 1.97.1 toolchain (`.mise.toml`, `rust-toolchain.toml`)
and writes `target/release/limen`; `mise exec -- make release` is the same thing. If `cargo` is missing
without mise, a rustup install puts it at `~/.cargo/bin/cargo` even when that directory is off `PATH`.

## 1. Observe — let real traffic name the risky routes

**Bind the control plane to loopback first** — `metrics.listen_addr` defaults to `0.0.0.0:9090`, and the
profile discloses route ids and the distinct query-parameter *names* traffic carried. Put limen in front
of legacy with your route inventory as its route table (`mode: legacy_only` is enough; a `new` upstream
need not exist yet), then add the block — `limen run` warns loudly whenever it is present:

```yaml
metrics: { listen_addr: "127.0.0.1:9090" }   # or an internal-only interface
observe: {}    # presence is the whole switch; no `enabled` field; sample_rate defaults to 1.0
```

**Where the route inventory comes from**, in descending fidelity: the service's own OpenAPI document or
route table (one `curl -s http://legacy/openapi.json | jq '.paths | keys'` for a FastAPI-style service),
else the framework's router registration read from source, else path sampling from access logs. **Prefer
`match.path_template` over `path_prefix` for any REST-shaped operation you can already name** —
`/conversations/{id}` instead of folding it under `path_prefix: "/conversations/"` — because the profile
recorder counts `distinct_read_paths` per the matched shape: a template gives you per-*operation* counters
from the first request, while a prefix folds every id into one aggregate you can only split later by
re-profiling. Where the inventory itself is uncertain, `path_prefix` is still the right coarse starting
point; expect stage 3 to refine it against the handler source, and treat a fold discovered there as a
reason to template and re-profile rather than to classify around it.

Drive representative traffic **unsampled**: the classifier's danger rules are existential, so sampling and
classification are mutually exclusive. A sampled profile is refused outright — every route lands
`relay_only` / `partial-sample` and `suggest-routes` exits `20`. Coverage, not volume, is the point.

```bash
curl -s http://127.0.0.1:9090/observe/profile | jq . > profile.json
```

Every *configured* route appears zero-filled from the first request; `"observations": 0` means your corpus
never touched that route — itself a finding.

→ [observe mode](https://charliek.github.io/limen/guides/observe-mode/) · [turn the block on](https://charliek.github.io/limen/guides/observe-mode/#1-turn-the-block-on) · [sampling vs. classification](https://charliek.github.io/limen/guides/observe-mode/#sampling-and-classification-are-mutually-exclusive) · [`observe` config](https://charliek.github.io/limen/reference/config-reference/#observe)

## 2. Draft — classify the profile into a config

```bash
# live proxy: polls until the profile quiesces AND limen_in_flight_requests == 0
limen suggest-routes -c limen.config.yaml --new-upstream https://new.internal > draft.limen.config.yaml
# or the saved document, contacting nothing (--profile conflicts with --control-url)
limen suggest-routes -c limen.config.yaml --profile ./profile.json --format json
limen validate-config -c draft.limen.config.yaml
```

The config must be the *profiled proxy's* config — it supplies the route table and the
`observe.sample_rate` cross-check. `--new-upstream` is the fallback for routes configuring none; without
it such a route drafts `mode: legacy_only`. `--min-samples` (5) and `--max-compare-paths` (8) tune the
read floor and the wildcard-granularity rule.

| Exit | Meaning |
|---|---|
| `0` | Draft emitted on real classifications. |
| `20` | Nothing usefully profiled: no observations, every route below the read floor, or a sampled profile. |
| `40` | The profile never quiesced within `--drain-deadline-ms`. |
| `50` | Required input is unavailable: control plane unreachable, running proxy has no `observe:` block, unreadable `--profile`, or a config that does not describe the profiled proxy — including a **stale profile**, where a route's recorded `match_basis` (`prefix:…`/`template:…`) disagrees with what the config now declares (re-profile after templating a route, never reclassify the old capture), and a **consistency refusal**, where a profile's own counters could not have come from the recorder (e.g. more read transport errors than reads) and are therefore corrupt or hand-edited. |

**Exit `20` still writes a draft** — the document goes to stdout either way, so the existence of a file
proves nothing. `20` means the draft rests on refusals to classify: unadoptable, not absent. **Branch on
the exit code, never on whether output appeared.**

→ [CLI → `suggest-routes`](https://charliek.github.io/limen/reference/cli/#suggest-routes)

## 3. Classify — disposition every route against the doctrine

The three dispositions are evidence, not answers. Map each onto the class taxonomy and record the class
in the inventory:

| Disposition | What it means |
|---|---|
| `relay_only` | A danger signal fired (redirecting read, cookie-minting read, one-time-token query name, catch-all/wildcard granularity, **or every observed read failed — R8a, `no-success-evidence`**) **or** too little was observed to say anything. Different states — do not record the second as the first. A route where reads only ever answered 4xx/5xx has demonstrated how it fails, not that it is safe; an error can condemn a route, it can never vouch for one. |
| `compare_narrowed` | Nothing dangerous fired, but the body cannot be trusted for equality (varying length, several content types, incomplete stability evidence). The narrowing becomes contract work. |
| `compare_candidate` | A fingerprint repeated with a stable length and no danger signal. A hypothesis carrying evidence, never a safety claim. |

Suggestions **fail toward relay-only** by construction: response metadata can prove a route unsafe to
compare and can never prove one safe, so the default draft emits `comparison: { enabled: false }` for
*every* route, suggestion riding as a comment. No traffic shape makes the tool emit a shadowing config.

**A mutating read with innocuous metadata is invisible to observation.** `GET /orders/42/mark-read`
returning a stable `200` looks exactly like `GET /orders/42` from outside and is suggested
`compare_candidate`. **Read each candidate route's handler source before promoting it** —
`--adopt-suggestions` is the deliberate promotion act and that reading is its precondition. Observation
is blind to sub-path aliasing too (a prefix route folds every path beneath it into one profile), so never
widen a `path_prefix` to make more traffic land in one class.

**No source to read** (closed binary, vendor service)? Then that precondition cannot be met:
leave the route at the draft's disposition or demote it further, and let a Pharos scenario
against a non-production environment establish what reading the handler would have — absence of
source is ambiguity, and ambiguity fails toward relay-only.

→ [classifying routes](https://charliek.github.io/limen/guides/classifying-routes/) · [the class taxonomy](https://charliek.github.io/limen/guides/classifying-routes/#the-class-taxonomy) · [what observation can and cannot tell you](https://charliek.github.io/limen/guides/classifying-routes/#what-observation-can-and-cannot-tell-you) · [why the default draft shadows nothing](https://charliek.github.io/limen/guides/observe-mode/#5-why-the-default-draft-shadows-nothing)

## 4. Shadow-compare — only on the routes that earned it

Legacy still serves every client; the shadow leg is fire-and-forget off the client path.
To rehearse the mechanics before a real `new` exists, point `new_upstream` at the *legacy*
host: every comparison should match, and the sink canary still proves the recording pipeline.
The `observe:` block can stay on during shadowing (it is passive either way); remove it when
you no longer want profiling.

```yaml
routes:
  - id: "get-user"
    match: { methods: ["GET"], path_prefix: "/users/" }
    legacy_upstream: "https://legacy.internal"
    new_upstream: "https://new.internal"
    mode: "shadow_legacy_primary"
    contract: "./contracts/user-service.contract.yaml#get-user"   # same file Pharos consumes
    comparison: { enabled: true, sample_rate: 0.1, max_body_bytes: 262144, min_comparisons: 20 }

diff_sink: { dir: "./campaign-diffs" }   # durable trail — reset it on every proxy start; limen only appends
debug:     { sink_canary: true }         # exposes POST /debug/canary
```

`min_comparisons` is an *exercise floor* (default `1`; `0` an explicit, visible exemption) read by
`verdict` and cross-checked by `report --format html` — not a tolerance. Coverage is `sample_rate` against eligible volume, arithmetic you do
yourself: an unsampled request appears in no skip metric. A *sampled* one that was not compared does —
`response_too_large`, `request_too_large`, `concurrency_limit`, `response_buffer_timeout`, `event_stream`,
or a failed shadow (`timeout`/`error`) — and every one of those now fails the floor of the route it
happened on, even once that route's raw comparison count clears `min_comparisons`. Validate the contract with `limen check-contract
./contracts/user-service.contract.yaml`. `comparison.shadow_methods: ["POST"]` (also `PUT`/`PATCH`; `DELETE`
is not eligible) opts a write into shadowing — body buffered within `max_body_bytes`, replayed
byte-identically to both upstreams. It exists and it is deliberate: the new upstream receives a *real*
write. Never add it without an explicit decision — and never add it without a recorded per-route
idempotence analysis (the mutation, the response-visible effect of double execution, and the corpus
constraint that keeps it true). The allowlist is a reminder that analysis is owed, not proof it was done.

→ [comparison & contracts](https://charliek.github.io/limen/guides/comparison-and-contracts/) · [`comparison` config](https://charliek.github.io/limen/reference/config-reference/#comparison) · [`shadow_methods`](https://charliek.github.io/limen/reference/config-reference/#comparisonshadow_methods-shadowing-a-write) · [contract format](https://charliek.github.io/limen/reference/contract-reference/#format)

## 5. Verdict — the gate is a typed exit code

"Zero mismatches" is not a gate: it is satisfied equally by *compared everything, found nothing*, by
*compared nothing*, and by *compared plenty while the sink silently dropped every record*. Stop traffic,
then take the gate:

```bash
limen verdict -c campaign.config.yaml --canary --format json > verdict.json
```

| Exit | Meaning |
|---|---|
| `0` | Clean — drained, floors met, sink integral, zero non-canary mismatches. |
| `10` | Mismatches found (non-canary). |
| `20` | Floors unmet: a route below its floor (starved), or at its floor with sampled work that went uncompared — any skip reason, or a shadow that never answered (undermined) — or a config that floors nothing at all. |
| `30` | Sink integrity: dropped records, unparseable lines, counter routes absent from the config, sink/engine disagreement — or a canary that never landed. |
| `40` | Drain timeout — the pipeline never quiesced. |
| `50` | Required input is unavailable: control plane unreachable, sink unreadable, a required metric series absent, a refused canary trigger. |

The **highest code wins** and every check fails closed — an input that could not be read is never scored
as "0 mismatches". `--offline` skips drain, floors, integrity, and canary; an offline `0` is strictly
weaker and only the JSON `mode` field tells them apart. Two disciplines decide whether a clean exit means
anything. **Floors prove something was compared** — they count comparisons, not coverage, so
`min_comparisons: 20` is met by 20 of 20 exactly as by 20 of 20,000, provided none of those 20 went
uncompared on a later leg (a skip or a failed shadow undermines the route regardless). **The canary proves
the recording pipeline is live**, since an empty sink and a correctly empty sink render identically;
`--canary` needs `debug.sink_canary: true` in the **running proxy's** config or the trigger is refused
(exit `50`), never silently skipped. Run it in *every* campaign. An undermined route's fix is always three
steps: change the knob the verdict names, **restart limen**, and re-drive the floors — these counters
never reset within a process, so re-running `verdict` alone stays `20`.

**Zero tolerance:** verdict compares no mismatch *rate* against a threshold — any remaining unexplained
non-canary mismatch is exit `10`. Resolve each (fix `new`, a narrow contract rule, or an
`intentional-change` recorded in the contract). There is no rate allowance.

→ [prove your lens bites](https://charliek.github.io/limen/guides/prove-your-lens-bites/) · [floors](https://charliek.github.io/limen/guides/prove-your-lens-bites/#1-floors-prove-something-was-compared) · [the sink canary](https://charliek.github.io/limen/guides/prove-your-lens-bites/#2-the-sink-canary-prove-the-recording-pipeline-is-live) · [CLI → `verdict`](https://charliek.github.io/limen/reference/cli/#verdict)

## 6. Report — the campaign bundle

Take the verdict **first**, then render the page from the artifacts it left, so the page can never
disagree with the gate. Both captures are optional inputs, and each flag is passed only if its capture
produced content — an empty file is "provided but unparseable", a FAILURE on the page, strictly worse than
not passing the flag at all:

```bash
limen verdict -c campaign.config.yaml --canary --format json > verdict.json \
  && verdict_exit=0 || verdict_exit=$?

page_flags=()
if curl -sf http://127.0.0.1:9090/observe/profile > profile.json && [ -s profile.json ]; then
  page_flags+=(--profile profile.json)
fi
if curl -sf http://127.0.0.1:9090/metrics > metrics.txt && [ -s metrics.txt ]; then
  page_flags+=(--metrics metrics.txt)
fi

limen report --dir ./campaign-diffs \
  --verdict verdict.json \
  --config  campaign.config.yaml \
  "${page_flags[@]}" \
  --format html --out status.html

exit "$verdict_exit"
```

`status.html` is one self-contained page — no JavaScript, no external fetches — that cross-checks the
artifacts rather than trusting them. A missing input is INCOMPLETE; an unreadable one, or a cross-check
drift, is FAILURE; an empty sink directory is INCOMPLETE, never clean. `--route`/`--since` are refused
with `--format html`, and the artifact flags are refused with `human`/`json`. **The page is not the
gate** — it exits `0` whenever a page was produced, including a page of nothing but failures.

→ [CLI → `report`](https://charliek.github.io/limen/reference/cli/#report) · [the HTML status page](https://charliek.github.io/limen/reference/cli/#the-html-status-page) · [runbook §8.3](https://charliek.github.io/limen/runbook/#83-the-shadow-gate-limen-verdict-not-a-reading-of-the-counters)

## 7. Rollout — the ladder, live

Once a route's verdict is `0` and its latency/error budget is green it moves to `percentage_split` and
rises 0 → 1 → 5 → 25 → 50 → 100, pausing at each step to recheck the budget on the traffic now actually
hitting new. This is tested doctrine, not a pointer to plausible machinery: the [rollout
simulation](https://charliek.github.io/limen/guides/flags-and-rollout/#tuning) ran this exact ladder live
against two real backends, 2026-08-16.

**The ladder, mechanically:**

```yaml
rollout:
  percentage_flag: "migration.get-user.rollout_percentage"
  default_percentage: 0
  assignment_key: { header: "x-tenant-id", fallback: "request_random" }
```

Pin `assignment_key.header` on real traffic — the unkeyed `request_random` fallback draws a fresh key per
request, which collapses per-key determinism into noise. Raise `percentage_flag` at runtime (no redeploy);
after each step, recheck the latency/error budget and, for write routes, the read-back drift check. Raising
the flag only ever *adds* keys to new (exact for a fixed key set, not statistical) — see [flags & rollout →
raising the rollout](https://charliek.github.io/limen/guides/flags-and-rollout/#raising-the-rollout) and its
[Tuning](https://charliek.github.io/limen/guides/flags-and-rollout/#tuning) section for `stale_ttl_ms` /
`refresh_interval_ms` trade-offs and a worked `primary_ms` sizing example.

**Read the status page's rollout section**, not just the metrics: `limen report --format html` renders each
`percentage_split` route's resolved target, observed share with per-side provenance, breaker state, and
transition counts, plus every `failover_to_legacy` row — see [CLI → the HTML status
page](https://charliek.github.io/limen/reference/cli/#the-html-status-page). `limen_rollout_resolved_target_percentage{route}`
is the flag-resolved *target* on its own if you're reading `/metrics` directly — deliberately not the
effective share, since an open breaker doesn't move it.

**The rollback levers, in order of automaticity:** the circuit breaker opens automatically on a threshold
breach and needs no human action; lowering the rollout flag is the primary deliberate lever (instant, no
redeploy, and returns *exactly* the earlier key set — proven by the simulation's rollback drill, not just
assumed); `legacy_only` is the full stop. See [resilience & failover](https://charliek.github.io/limen/guides/resilience/#circuit-breaker).

**The `failover_safe` decision.** `failover_safe: true` is an idempotency attestation, and it now applies
under `percentage_split` as well as `failover_to_legacy` — a split-chosen-new request replays to legacy on
fast failure (5xx, refused/reset connection) client-invisibly, sharing one `primary_ms` budget with the new
attempt (a timeout has already spent that budget and is **not** replayed). Turning it on isn't free even
when it never fires — buffered request/response bodies, latency-to-first-byte, a masked new-side 5xx — see
the cost table in [resilience & failover → the cost of turning `failover_safe`
on](https://charliek.github.io/limen/guides/resilience/#the-cost-of-turning-failover_safe-on) before flipping
it on a route mid-rollout.

**What you'll see when things go wrong** (proven live by the simulation's drills, not just fixture-asserted):

- **New backend dies mid-ladder** → the breaker opens on every affected split route
  (`limen_breaker_transitions_total` counts the closed→open transition); `failover_safe` routes keep
  serving `200`s via client-invisible replay; non-`failover_safe` routes fail **visibly** (a limen-synthesized
  `502`/`504`, never a silent replay) — Invariant 4's two arms, independently observable.
- **Backend recovers** → the breaker walks open→half_open→closed per route, visible in the transition
  counters before you'd need to compare traffic; the recovered split resumes the exact same key population
  it had before the kill.
- **Flag source goes stale** (past `stale_ttl_ms`) → every `percentage_split` route routes to legacy —
  Invariant 1, fail-safe — and `limen_rollout_resolved_target_percentage` reads `0` with the staleness gauges
  saying why, until the flag source recovers and the exact prior split returns.

## Safety invariants — never violate these

Numbered to match the canonical load-bearing invariant list in limen's `CLAUDE.md` — cite these numbers,
don't recount them; a subset in a different order here previously drifted from that list.

- **Invariant 3: never shadow a write without the explicit opt-in.** Only `GET`/`HEAD` are eligible unless
  the route lists `comparison.shadow_methods` with `POST`, `PUT`, and/or `PATCH` (`DELETE` is not eligible);
  absent it, a write is never sent to `new`. Listing a method is not itself a safety proof — the route still
  needs a recorded per-route idempotence analysis before it opts in.
- **Invariant 1: ambiguity defaults to legacy / relay-only.** Unhealthy new upstream, open breaker, stale
  flags, ambiguous config, unconfirmed candidate — all resolve toward legacy. A route whose source you have
  not read is `relay_only`.
- **Invariant 6 (bounded buffers): streaming/SSE routes are relay-only for comparison purposes.**
  `text/event-stream` is skipped by content type before a byte is buffered
  (`comparison_skipped{reason="event_stream"}`) and a trickling body hits the buffer deadline
  (`response_buffer_timeout`) — backstops, not a plan. Never enable comparison on a streaming route and read
  the skips as coverage.
- **Invariant 4: never replay a failed in-flight request against legacy** unless the route is explicitly
  `failover_safe: true` — under `percentage_split` as well as `failover_to_legacy` now. Routing *subsequent*
  requests to legacy via the breaker is fine; retrying the one that may already have hit `new` is not, and
  even an attested-safe replay is bounded by the one `primary_ms` budget it shares with the new attempt (a
  timeout has already spent it and is not replayed).
