# Installation

Limen is a single Rust binary. The supported way to build it is with the
toolchain pinned in the repository.

## Prerequisites

- [mise](https://mise.jdx.dev) — manages the pinned Rust toolchain.
- A C toolchain and `pkg-config` are **not** required: Limen uses
  [rustls](https://github.com/rustls/rustls) for TLS, so there is no OpenSSL
  build dependency.

The Rust version is pinned to **1.97.1** in both `.mise.toml` and
`rust-toolchain.toml`, so every contributor and CI run builds with the same
compiler.

## Build from source

```bash
git clone https://github.com/charliek/limen
cd limen

mise install                  # install the pinned Rust toolchain
mise exec -- make build       # debug binary  -> target/debug/limen
mise exec -- make release     # optimized binary -> target/release/limen
```

Verify the binary:

```bash
target/debug/limen --version
target/debug/limen --help
```

!!! note "Why `mise exec --`"
    Cargo is provided by the mise-managed toolchain rather than a system-wide
    install. `mise exec -- <command>` runs the command with that toolchain on
    `PATH`; its child processes inherit it, so `mise exec -- make check` works
    too. If you use `mise activate` in your shell, you can drop the prefix.

## Quality gate

The same checks CI enforces:

```bash
mise exec -- make check       # fmt-check + clippy (-D warnings) + tests
```

Or individually:

```bash
mise exec -- cargo fmt --all -- --check
mise exec -- cargo clippy --all-targets -- -D warnings
mise exec -- cargo test --all
```

## Documentation site

The docs you are reading build with `mkdocs-material` via `uv`:

```bash
mise exec -- make docs-serve   # live reload at http://127.0.0.1:7071
mise exec -- make docs         # one-shot build into site-build/
```

## Next steps

- [Quickstart](quickstart.md) — proxy a single route.
- [Setup](../development/setup.md) — the development workflow.
