# CLI

Limen exposes six subcommands. All output is structured and scriptable; the
proxy refuses to start (or validate) on invalid input.

```text
limen <COMMAND> [OPTIONS]
```

| Command | Purpose |
|---|---|
| `run` | Bind the data-plane and control-plane listeners and serve. |
| `validate-config` | Semantically validate a configuration file. |
| `print-routes` | Print the resolved routing table for a configuration. |
| `check-contract` | Validate a behavioral contract and its JSONPath compliance. |
| `report` | Summarize the mismatches collected in a `diff_sink` directory. |
| `verdict` | Render a typed campaign verdict from the config, the live control plane, and the sink. |

## `run`

```bash
limen run --config limen.config.yaml
```

| Option | Env | Default | Description |
|---|---|---|---|
| `-c, --config <PATH>` | `LIMEN_CONFIG` | `limen.config.yaml` | Path to the configuration file. |

Loads and validates the configuration, then binds both listeners. On
`SIGTERM`/`SIGINT` it shuts down gracefully: stops accepting, drains in-flight
primary requests up to `server.graceful_shutdown_timeout_ms`, and exits cleanly.

## `validate-config`

```bash
limen validate-config --config limen.config.yaml
```

Performs **semantic** validation, not just a parse: well-formed upstream URLs,
in-range rollout percentages, sane timeouts, unique route IDs, known route
modes, resolvable contract references, the contract-vs-inline conflict rule,
JSONPath-subset compliance, and valid fail-safe / budget values. Failures name
the offending field and route. Exits non-zero on any error.

## `print-routes`

```bash
limen print-routes --config limen.config.yaml
```

Prints the resolved routing table — each route's id, match (methods +
path-prefix), mode, upstreams, and effective comparison policy — after layering
and contract merge. Useful for confirming what Limen will actually do before
starting it.

## `check-contract`

```bash
limen check-contract ./contracts/device-service.contract.yaml
```

Validates a contract file (YAML or JSON) against the schema and reports the
JSONPath-subset compliance of every path it contains. This lets the
AI → Pharos → Limen loop confirm a freshly drafted contract is Limen-consumable
before wiring it into a route. It produces the **same** verdict Pharos's
`check-contract` would, since both implement the identical
[JSONPath subset](contract-reference.md#supported-jsonpath-subset).

## `report`

```bash
limen report --dir ./limen-diffs
limen report --dir ./limen-diffs --route get-device --since 2026-07-28T00:00:00Z
limen report --dir ./limen-diffs --format json | jq '.routes[] | {route_id, count}'
```

| Option | Default | Description |
|---|---|---|
| `--dir <PATH>` | — | **Required.** The [`diff_sink.dir`](config-reference.md#diff_sink) directory to read. |
| `--route <ID>` | all routes | Only report this route id. |
| `--since <RFC3339>` | all time | Only mismatches at or after this instant (offsets honored, e.g. `2026-07-28T12:00:05+02:00`). |
| `--format <human\|json>` | `human` | `human` prints an aligned summary; `json` prints one document. |

Reads every `mismatches-*.jsonl` file in the directory and prints per-route
mismatch counts (total and by mismatch kind) plus the most recent examples per
route. **No config file is involved** — the sink directory is self-describing, so
a report runs anywhere the files are.

Unparseable lines are counted and reported (`malformed_lines`), never fatal: a
record torn by a killed process must not cost you the rest of the report.

```text
3 mismatch(es) across 2 route(s) (2 file(s) read)

ROUTE         COUNT  KINDS
get-device        2  body 2, set_cookie.value 1
list-devices      1  status 1

get-device — 2 most recent:
  2026-07-28T10:00:05Z  GET  /devices/42  0f2c…  body,set_cookie.value
  2026-07-28T10:00:00Z  GET  /devices/7   9ab1…  body
```

## `verdict`

```bash
limen verdict -c limen.config.yaml
limen verdict -c limen.config.yaml --canary --format json
limen verdict -c limen.config.yaml --offline --format json
```

`verdict` renders a typed campaign verdict — the operator-checked gate a
migration campaign wrapper branches on instead of parsing prose. It reads
exactly three limen-owned inputs: the config file (route matrix, comparison
floors, sink directory, control-plane address), the live control plane's
`/metrics`, and the sink directory. It waits for the shadow/sink pipeline to
quiesce (never a blind sleep), asserts every floored route compared at least
its configured minimum, reconciles the sink against the engine's own counters,
and — with `--canary` — proves the record→sink→flush path bites *right now*
rather than assuming it. Every decision fails closed: a required input that is
unavailable is never read as "0 mismatches."

| Option | Default | Description |
|---|---|---|
| `-c, --config <PATH>` | `limen.config.yaml` (env `LIMEN_CONFIG`) | The config file verdict reads its route matrix, floors, and defaults from. |
| `--dir <PATH>` | the config's `diff_sink.dir`, resolved exactly as `run` resolves it | Sink directory override. Neither a `diff_sink` block nor `--dir` is exit 50. |
| `--control-url <URL>` | derived from `metrics.listen_addr`, wildcard hosts (`0.0.0.0`/`::`) mapped to `127.0.0.1` | Control-plane base URL. The scrape path is the config's `metrics.path`, never a hardcoded `/metrics`. |
| `--canary` | off | Trigger `POST /debug/canary` before draining and require the injection to ride the pipeline end to end. Needs `debug.sink_canary: true` in the **running proxy's** config — see the note below. Conflicts with `--offline`. |
| `--offline` | off | Degraded report-only mode: skip drain, floors, sink integrity, and canary — evaluate the sink report alone. Restricts the exit code to 0/10/50. |
| `--drain-slack-ms <N>` | `2000` | Slack added to the longest route `timeouts.shadow_ms` to form the drain deadline. |
| `--drain-deadline-ms <N>` | (computed from `--drain-slack-ms`) | Advanced: replace the computed drain deadline entirely. |
| `--poll-interval-ms <N>` | `250` | Interval between `/metrics` polls while draining. |
| `--format <human\|json>` | `human` | `human` prints an aligned verdict block; `json` prints one document. |

### Preconditions

- **Traffic has stopped.** Drain is observed (two consecutive, value-identical
  balanced scrapes), not slept, but it only converges once nothing is still
  producing shadow/comparison activity.
- **The sink directory exists and was reset when the proxy under test
  started.** Verdict reconciles the sink against the engine's live counters
  per route; stale records from a previous run make that reconciliation fail
  — correctly, since it cannot tell a stale record from a lost one.
- **At most one canary injection per `--canary` invocation.** The canary check
  is relative (sink count == engine's `__limen_canary__` mismatch counter, and
  both ≥ 1), so it stays re-runnable across sink resets, but running
  `--canary` twice against the same undrained sink will not double-count
  correctly — one invocation, one injection.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Clean — drained, floors met, sink integral, zero non-canary mismatches. |
| `10` | Mismatches found (non-canary). |
| `20` | Floors unmet, including a config that floors nothing at all. |
| `30` | Sink-integrity failure: drops, per-route disagreement, malformed lines, or a missing/duplicate canary. |
| `40` | Drain timeout — the pipeline never quiesced within the deadline. |
| `50` | A required input was unavailable: control plane unreachable, sink dir unreadable, a required metric series absent, or a refused canary trigger. |
| `1` | Unexpected tooling error (anyhow). |
| `2` | CLI usage error (clap). |

When several conditions hold, **the highest code wins** — a worse tooling
condition dominates because it makes the lower-numbered answers untrustworthy
(a drain timeout makes the mismatch count and floor numbers unreliable, so 40
outranks 10 and 20). The JSON output still lists every check's individual
outcome regardless of which code won.

### JSON output

```json
{
  "mode": "online",
  "verdict": "clean",
  "exit_code": 0,
  "checks": {
    "drain": { "status": "pass", "detail": "pipeline quiesced (two stable balanced scrapes)" },
    "floors": { "status": "pass", "detail": "2 floored route(s) all at/above floor" },
    "sink_integrity": { "status": "pass", "detail": "sink and engine counters agree on every route; nothing dropped" },
    "canary": { "status": "pass", "detail": "canary rode compare → sink → flush end-to-end (1 record(s), counters agree)" },
    "mismatches": { "status": "pass", "detail": "zero non-canary mismatches recorded" }
  },
  "mismatches_total": 0,
  "canary_records": 1,
  "floors": [
    { "route_id": "get-device", "comparisons": 14, "floor": 1, "met": true },
    { "route_id": "list-devices", "comparisons": 6, "floor": 1, "met": true }
  ],
  "sink_mismatches_by_route": {},
  "informational": []
}
```

An input-unavailable verdict (exit 50) is a shorter, distinct document —
`{"mode": "unavailable", "verdict": "input-unavailable", "exit_code": 50,
"error": "…"}` — so a wrapper can tell a refused/impossible verdict from a
completed one at a glance.

### Notes

- **An offline exit 0 is weaker than an online one, and the exit code alone
  cannot tell them apart.** `--offline` only evaluates the sink report; it
  never drains, checks floors, reconciles the sink against the engine, or
  triggers the canary. The `mode` field in the JSON output (`"online"` vs
  `"offline"`) is the tell — a wrapper that cares about the difference must
  read it, not just the exit code.
- **`--canary` needs `debug.sink_canary: true` in the config the *running
  proxy* was started with**, not necessarily the config file passed to
  `verdict -c`. Verdict only reads its own config for the route matrix,
  floors, and address derivation; the canary trigger is an HTTP call to
  whatever process is listening at `--control-url`. If that process's own
  config has `debug.sink_canary` off (or absent), the trigger is refused and
  verdict exits 50 — never silently skipped.

## Configuration sources & precedence

For commands that take a config, sources layer with later overriding earlier
(spec §5.1):

1. Built-in defaults
2. Config file (`--config`)
3. Environment variables (e.g. `LIMEN_LISTEN_ADDR`, `LIMEN_FLAGS_PROVIDER`)
4. CLI arguments

## Global flags

| Flag | Description |
|---|---|
| `-h, --help` | Print help (use on any subcommand for its options). |
| `-V, --version` | Print the version. |

Logging verbosity follows the standard `RUST_LOG` environment variable
(defaults to `info`).
