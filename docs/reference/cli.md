# CLI

Limen exposes four subcommands. All output is structured and scriptable; the
proxy refuses to start (or validate) on invalid input.

```
limen <COMMAND> [OPTIONS]
```

| Command | Purpose |
|---|---|
| `run` | Bind the data-plane and control-plane listeners and serve. |
| `validate-config` | Semantically validate a configuration file. |
| `print-routes` | Print the resolved routing table for a configuration. |
| `check-contract` | Validate a behavioral contract and its JSONPath compliance. |

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
[JSONPath subset](../limen_spec.md) (§7.4).

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
