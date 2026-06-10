//! Upstream decisioning: which upstream serves as the client's *primary*.
//!
//! This module answers "who serves the client right now?" for a route.
//! [`primary_upstream`] is the **mode-level default**: the primary chosen from
//! the mode alone, which is all Phase 2 needs. Later phases *refine* this for
//! the two modes whose primary depends on runtime state, building on these
//! defaults rather than replacing them:
//! - shadowing (Phase 4) dispatches an *additional* request to the non-primary,
//!   leaving the primary chosen here untouched;
//! - rollout (Phase 5) refines `percentage_split` to select new for some keys
//!   (a richer decision that takes the route id, assignment key, and flag value);
//! - the circuit breaker / failover (Phase 6) can steer `failover_to_legacy`
//!   and `percentage_split` away from an unhealthy new upstream.
//!
//! The defaults all lean toward legacy when nothing else has spoken yet, which
//! is the project's load-bearing fail-safe posture.

use crate::config::model::RouteMode;

/// Which upstream a request is sent to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upstream {
    /// The legacy (current source of truth) upstream.
    Legacy,
    /// The new (replacement) upstream.
    New,
}

impl Upstream {
    /// A short, stable label for logs and metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            Upstream::Legacy => "legacy",
            Upstream::New => "new",
        }
    }
}

/// The primary upstream for a route, given only its mode.
///
/// - `legacy_only`, `shadow_legacy_primary` → legacy (legacy serves the client).
/// - `percentage_split` → legacy as the safe default (0% until rollout resolves
///   a percentage in Phase 5).
/// - `new_only`, `failover_to_legacy` → new (new is primary).
pub fn primary_upstream(mode: RouteMode) -> Upstream {
    match mode {
        RouteMode::LegacyOnly | RouteMode::ShadowLegacyPrimary | RouteMode::PercentageSplit => {
            Upstream::Legacy
        }
        RouteMode::NewOnly | RouteMode::FailoverToLegacy => Upstream::New,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_is_legacy_for_legacy_shadow_and_split() {
        assert_eq!(primary_upstream(RouteMode::LegacyOnly), Upstream::Legacy);
        assert_eq!(
            primary_upstream(RouteMode::ShadowLegacyPrimary),
            Upstream::Legacy
        );
        assert_eq!(
            primary_upstream(RouteMode::PercentageSplit),
            Upstream::Legacy
        );
    }

    #[test]
    fn primary_is_new_for_new_only_and_failover() {
        assert_eq!(primary_upstream(RouteMode::NewOnly), Upstream::New);
        assert_eq!(primary_upstream(RouteMode::FailoverToLegacy), Upstream::New);
    }
}
