# Testing

Limen's correctness and safety guarantees are backed by three test layers
(spec §16). All run under `cargo test --all`.

## Unit tests

Co-located with the code (`#[cfg(test)] mod tests`). They cover the pure logic:

- **Route matching** — exact, prefix, longest-prefix-wins, method specificity,
  no-match, duplicate-ID rejection.
- **Config validation** — the full matrix of invalid inputs (bad URLs,
  out-of-range percentages, unknown modes, contract-vs-inline conflicts,
  non-idempotent `failover_to_legacy` without `failover_safe`, out-of-range
  budgets).
- **Contract** — load, reference resolution, defaults+per-route merge,
  conflict rejection, JSONPath-subset enforcement.
- **Rollout decision** — 0% → legacy, 100% → new, key stability, distribution,
  missing-key fallback, open-circuit override.
- **Flag providers** — static/file/Redis behavior, last-known-good, staleness
  → fail-safe.
- **Normalization & hashing** — key-order independence, ignore/redact paths,
  array sorting, unordered arrays, timestamp/enum normalization, hash equality.
- **JSON diff** — added/removed/changed fields, bounded count and value length,
  redaction applied before output.
- **Circuit breaker** — the closed/open/half-open transitions.

## Integration tests

Under `tests/`, driving the proxy against [`wiremock`](https://docs.rs/wiremock)
or lightweight `axum` test servers standing in for legacy and new:

| Test | Asserts |
|---|---|
| `legacy_only` / `new_only` | correct upstream served; the other gets zero requests. |
| `shadow_match` / `shadow_mismatch` | client always gets legacy; comparison records match / emits a redacted diff. |
| `shadow_timeout` | new sleeps past the shadow timeout; client is served quickly and unaffected. |
| `percentage_rollout` | same key is stable; many keys distribute within tolerance. |
| `circuit_breaker` | repeated new-side 5xx open the circuit; traffic returns to legacy. |
| `failover_safe` | replay happens only when `failover_safe: true`. |
| `flag_reload` | a file/Redis flag change takes effect without restart. |
| `stale_flag_failsafe` | last-known-good before TTL, fail-safe after. |
| `graceful_shutdown` | in-flight primary completes; the process exits cleanly. |

## Security & privacy tests

- Header redaction (`authorization`, `cookie`) — secrets absent from logs.
- JSON-field redaction (`$.token`, `$.user.email`) — masked in diff output.
- Metric cardinality — many unique IDs never appear as labels.

## Performance benchmarks

`benches/` holds [criterion](https://docs.rs/criterion) benchmarks that report
against the SLO table (spec §12): streaming-path added latency, buffer-for-compare
added latency, and the architectural guarantee that shadow dispatch adds no
client-visible latency.

```bash
mise exec -- make bench
```

## Running a focused subset

```bash
mise exec -- cargo test --all                    # everything
mise exec -- cargo test routing::                # one module's unit tests
mise exec -- cargo test --test shadow_mismatch   # one integration test file
```
