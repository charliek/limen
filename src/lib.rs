//! Limen — a reverse proxy for safely migrating HTTP traffic from a legacy
//! service to a new implementation.
//!
//! Limen sits in front of two upstreams — `legacy` (the current source of
//! truth) and `new` (the replacement) — and moves traffic between them without
//! changing user-facing behavior. It can shadow read traffic to the new
//! service, compare responses against legacy, roll traffic over by percentage,
//! and fail safely back to legacy whenever anything is uncertain.
//!
//! The crate is split into a thin binary ([`src/main.rs`](../src/main.rs)) and
//! this library so the proxy's logic is testable without binding sockets or
//! driving real upstreams. The module layout follows the implementation spec:
//!
//! | Module          | Responsibility                                             |
//! |-----------------|------------------------------------------------------------|
//! | [`cli`]         | `clap` subcommands and dispatch.                           |
//! | [`config`]      | Operational config model, layered loading, validation.    |
//! | [`contract`]    | The shared behavioral contract: model, loading, merge.    |
//! | [`http`]        | Data-plane server, upstream client, streaming proxy core.  |
//! | [`routing`]     | Route matching, upstream decisioning, rollout hashing.     |
//! | [`compare`]     | Response normalization, hashing, diffing, redaction.       |
//! | [`flags`]       | Feature-flag providers (static / file / redis).            |
//! | [`resilience`]  | Circuit breaker, timeouts, shadow concurrency limiting.    |
//! | [`observability`] | Metrics, structured logging, request-id propagation.     |
//! | [`health`]      | `/health/live` and `/health/ready` endpoints + readiness.  |
//! | [`error`]       | Top-level error types crossing module boundaries.          |
//!
//! Two modules sit outside that map because they implement operator commands
//! rather than a data-plane concern: [`verdict`] (`limen verdict`) and the
//! [`suggest`]/[`draft`] pair (`limen suggest-routes`), which classify an
//! observe-mode profile and render a draft configuration from it.

pub mod cli;
pub mod compare;
pub mod config;
pub mod contract;
pub mod draft;
pub mod error;
pub mod flags;
pub mod health;
pub mod http;
pub mod observability;
pub mod resilience;
pub mod routing;
pub mod suggest;
pub mod verdict;

pub use error::{Error, Result};
