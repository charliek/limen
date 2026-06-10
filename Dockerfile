# syntax=docker/dockerfile:1
#
# Multi-stage build for the Limen proxy. Produces a small Debian-slim runtime
# image containing only the static-ish `limen` binary.
#
#   docker build -t limen .
#   docker run --rm limen --help
#
# The build pins the same Rust toolchain as the repo (rust-toolchain.toml).

FROM rust:1.96-slim-bookworm AS build
WORKDIR /src
COPY . .
# `--locked` builds against the committed Cargo.lock for reproducibility.
RUN cargo build --release --locked --bin limen

FROM debian:bookworm-slim AS runtime
# ca-certificates for TLS to HTTPS upstreams (rustls also bundles webpki roots).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/limen /usr/local/bin/limen
# Data plane and control plane (see your config's server/metrics listen_addr).
EXPOSE 8080 9090
ENTRYPOINT ["limen"]
CMD ["--help"]
