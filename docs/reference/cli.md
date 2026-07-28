# CLI

Limen exposes five subcommands. All output is structured and scriptable; the
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
