//! Routing: route matching, upstream decisioning, and rollout hashing.
//!
//! For each request Limen matches a route (longest path-prefix wins), resolves
//! the route mode, and decides which upstream serves as primary given the mode,
//! the rollout percentage, and circuit-breaker state (Section 3.4).
//!
//! Submodules:
//! - [`matcher`] — method + path matching, template tier then prefix tier
//!   (Phase 2).
//! - [`template`] — `{param}` path templates: parsing, matching, and the
//!   overlap algebra that keeps a route table unambiguous.
//! - [`decision`] — mode + rollout + breaker → upstream choice (Phase 2/5/6).
//! - [`rollout`] — deterministic hashing and bucket assignment (Phase 5).
//! - [`resolve`] — startup resolution of each route's comparison policy.

pub mod decision;
pub mod matcher;
pub mod resolve;
pub mod rollout;
pub mod template;

pub use decision::{decide_primary, primary_upstream, PrimaryDecision, Upstream};
pub use matcher::{CompiledRoute, PathMatcher, RouteComparison, RouteTable};
pub use resolve::resolve_comparisons;
pub use template::CompiledTemplate;
