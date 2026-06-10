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

use axum::http::HeaderMap;

use crate::config::model::RouteMode;
use crate::flags::FlagProvider;
use crate::resilience::BreakerReservation;
use crate::routing::matcher::CompiledRoute;
use crate::routing::rollout;

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

/// The chosen primary upstream plus the breaker reservation the proxy must
/// settle.
///
/// `breaker` is `Some` **only** when a breaker-guarded `new` attempt was
/// admitted — i.e. [`allow`](crate::resilience::CircuitBreaker::allow) issued an
/// admission (reserving a trial slot if half-open). Carrying the reservation
/// here ties the settlement obligation to the decision: whenever it is `Some`,
/// the proxy must settle it exactly once — [`record`](BreakerReservation::record)
/// on a real attempt, or [`release`](BreakerReservation::release) if it bails out
/// before reaching new.
#[derive(Debug, Clone)]
pub struct PrimaryDecision {
    /// The upstream that serves the client.
    pub upstream: Upstream,
    /// The breaker reservation to settle, if a `new` trial was admitted
    /// (otherwise `None`).
    pub breaker: Option<BreakerReservation>,
}

/// Decide the primary upstream for a request. This refines [`primary_upstream`]:
/// `percentage_split` resolves the rollout percentage from flags and picks by
/// assignment key; `failover_to_legacy` prefers new. Both consult the circuit
/// breaker (**pre-flight steering**): while the breaker is open, the request is
/// routed to legacy instead of new. The *mid-flight* failover retry (new failed
/// → replay legacy) lives in the proxy's error handling, not here.
pub async fn decide_primary(
    route: &CompiledRoute,
    headers: &HeaderMap,
    flags: &dyn FlagProvider,
) -> PrimaryDecision {
    match route.mode {
        RouteMode::PercentageSplit => percentage_split(route, headers, flags).await,
        RouteMode::FailoverToLegacy => gate_new(route),
        other => to_legacy(primary_upstream(other)),
    }
}

/// A decision for `upstream` with no breaker slot to settle.
fn to_legacy(upstream: Upstream) -> PrimaryDecision {
    PrimaryDecision {
        upstream,
        breaker: None,
    }
}

/// Gate a `new`-preferring choice through the circuit breaker: an open breaker
/// steers to legacy; otherwise attempt new and, if a breaker guards the route,
/// hand back its handle so the proxy settles the reserved slot. Shared by
/// `failover_to_legacy` and a split bucket that selects new.
fn gate_new(route: &CompiledRoute) -> PrimaryDecision {
    match &route.breaker {
        Some(breaker) => match breaker.allow() {
            // Admitted (a trial slot is reserved if half-open); the proxy must
            // settle the reservation.
            Some(admission) => PrimaryDecision {
                upstream: Upstream::New,
                breaker: Some(BreakerReservation::new(breaker.clone(), admission)),
            },
            // Breaker open — steer to legacy, nothing to settle.
            None => PrimaryDecision {
                upstream: Upstream::Legacy,
                breaker: None,
            },
        },
        None => PrimaryDecision {
            upstream: Upstream::New,
            breaker: None,
        },
    }
}

/// Resolve a `percentage_split` route to an upstream.
async fn percentage_split(
    route: &CompiledRoute,
    headers: &HeaderMap,
    flags: &dyn FlagProvider,
) -> PrimaryDecision {
    // Fail safe: stale flags apply the fail-safe mode (legacy_only is the only
    // mode today) regardless of percentage.
    if flags.health().stale {
        return to_legacy(Upstream::Legacy);
    }
    // A percentage_split route always has rollout config (validated); absent it,
    // fail safe to legacy.
    let Some(rollout) = &route.rollout else {
        return to_legacy(Upstream::Legacy);
    };
    let percentage = flags
        .get(&rollout.percentage_flag)
        .await
        .and_then(|v| v.as_f64())
        .unwrap_or(rollout.default_percentage)
        .clamp(0.0, 100.0);
    let key = rollout::assignment_key(
        rollout.assignment_key.header.as_deref(),
        rollout.assignment_key.fallback,
        headers,
    );
    if rollout::selects_new(rollout::bucket(&route.id, &key), percentage) {
        // The bucket selected new — still gate through the breaker.
        gate_new(route)
    } else {
        to_legacy(Upstream::Legacy)
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

    use crate::config::model::Config;
    use crate::flags::{FlagProvider, FlagProviderHealth, FlagValue};
    use crate::routing::{resolve_comparisons, RouteTable};
    use async_trait::async_trait;
    use std::path::Path;
    use std::time::Duration;

    /// A controllable in-memory provider for decision tests.
    struct Fake {
        stale: bool,
        percentage: Option<f64>,
    }

    #[async_trait]
    impl FlagProvider for Fake {
        async fn get(&self, _key: &str) -> Option<FlagValue> {
            self.percentage.map(FlagValue::Number)
        }
        fn health(&self) -> FlagProviderHealth {
            FlagProviderHealth {
                stale: self.stale,
                last_success_age_ms: Some(0),
                consecutive_failures: 0,
            }
        }
        async fn refresh(&self) {}
        fn refresh_interval(&self) -> Option<Duration> {
            None
        }
    }

    fn split_table() -> RouteTable {
        let config: Config = serde_yaml::from_str(
            r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: percentage_split
    rollout:
      percentage_flag: "f"
      default_percentage: 0
      assignment_key: { header: "x-tenant-id", fallback: request_random }
"#,
        )
        .unwrap();
        let comparisons = resolve_comparisons(&config, Path::new(".")).unwrap();
        RouteTable::build(&config, comparisons).unwrap()
    }

    fn headers(tenant: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-tenant-id", tenant.parse().unwrap());
        h
    }

    #[tokio::test]
    async fn percentage_split_zero_and_hundred() {
        let table = split_table();
        let route = table.match_route("GET", "/x").unwrap();

        let all_new = Fake {
            stale: false,
            percentage: Some(100.0),
        };
        let none_new = Fake {
            stale: false,
            percentage: Some(0.0),
        };
        assert_eq!(
            decide_primary(route, &headers("t"), &all_new)
                .await
                .upstream,
            Upstream::New
        );
        assert_eq!(
            decide_primary(route, &headers("t"), &none_new)
                .await
                .upstream,
            Upstream::Legacy
        );
    }

    #[tokio::test]
    async fn same_tenant_is_stable() {
        let table = split_table();
        let route = table.match_route("GET", "/x").unwrap();
        let flags = Fake {
            stale: false,
            percentage: Some(50.0),
        };
        let first = decide_primary(route, &headers("tenant-42"), &flags)
            .await
            .upstream;
        for _ in 0..10 {
            assert_eq!(
                decide_primary(route, &headers("tenant-42"), &flags)
                    .await
                    .upstream,
                first
            );
        }
    }

    #[tokio::test]
    async fn stale_flags_fail_safe_to_legacy_even_at_100_percent() {
        let table = split_table();
        let route = table.match_route("GET", "/x").unwrap();
        // Stale provider with a 100% rollout must still route to legacy.
        let stale = Fake {
            stale: true,
            percentage: Some(100.0),
        };
        assert_eq!(
            decide_primary(route, &headers("t"), &stale).await.upstream,
            Upstream::Legacy
        );
    }

    #[tokio::test]
    async fn missing_flag_uses_default_percentage() {
        let table = split_table();
        let route = table.match_route("GET", "/x").unwrap();
        // No flag value -> default_percentage (0) -> legacy.
        let no_value = Fake {
            stale: false,
            percentage: None,
        };
        assert_eq!(
            decide_primary(route, &headers("t"), &no_value)
                .await
                .upstream,
            Upstream::Legacy
        );
    }
}
