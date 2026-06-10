//! Routing: route matching, upstream decisioning, and rollout hashing.
//!
//! For each request Limen matches a route (longest path-prefix wins), resolves
//! the route mode, and decides which upstream serves as primary given the mode,
//! the rollout percentage, and circuit-breaker state (Section 3.4).
//!
//! Submodules:
//! - [`matcher`] — method + longest-prefix matching (Phase 2).
//! - [`decision`] — mode + rollout + breaker → upstream choice (Phase 2/5/6).
//! - [`rollout`] — deterministic hashing and bucket assignment (Phase 5).
//! - [`resolve`] — startup resolution of each route's comparison policy.

pub mod decision;
pub mod matcher;
pub mod resolve;
pub mod rollout;

pub use decision::{decide_primary, primary_upstream, Upstream};
pub use matcher::{CompiledRoute, RouteComparison, RouteTable};
pub use resolve::resolve_comparisons;
