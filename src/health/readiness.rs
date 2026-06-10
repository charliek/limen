//! Readiness evaluation (spec §10.3).
//!
//! `/health/ready` reports whether Limen can safely serve. Readiness is allowed
//! to **degrade** rather than hard-fail when a dependency is unhealthy but the
//! proxy remains safe — e.g. a stale flag provider that has fallen back to the
//! fail-safe mode. A degraded proxy still serves (legacy), so it reports ready.
//!
//! Configuration is validated at startup, so readiness reflects the runtime
//! flag provider: fresh → [`Readiness::Ready`]; stale-but-fail-safe →
//! [`Readiness::Degraded`] (still serving legacy). The proxy has no state that
//! makes it unsafe to serve once started, so [`Readiness::Unready`] is reserved
//! for future dependencies that can hard-fail.

/// The proxy's readiness state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// Fully healthy.
    Ready,
    /// Serving safely on a fallback (e.g. stale flags → legacy_only).
    Degraded,
    /// Not safe to serve.
    Unready,
}

impl Readiness {
    /// Whether the proxy should accept traffic in this state. Both `Ready` and
    /// `Degraded` serve; only `Unready` does not.
    pub fn is_serving(self) -> bool {
        matches!(self, Readiness::Ready | Readiness::Degraded)
    }

    /// A short, stable label for the readiness body and logs.
    pub fn label(self) -> &'static str {
        match self {
            Readiness::Ready => "ready",
            Readiness::Degraded => "degraded",
            Readiness::Unready => "unready",
        }
    }
}

/// Evaluate current readiness from the flag provider's health. A stale provider
/// has fallen back to the fail-safe mode (legacy), which is still safe to serve,
/// so it reports [`Readiness::Degraded`] rather than failing the check. `None`
/// (a provider that reports no health, e.g. static) is treated as healthy.
pub fn evaluate(flags: Option<&crate::flags::FlagProviderHealth>) -> Readiness {
    match flags {
        Some(health) if health.stale => Readiness::Degraded,
        _ => Readiness::Ready,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_and_degraded_serve_unready_does_not() {
        assert!(Readiness::Ready.is_serving());
        assert!(Readiness::Degraded.is_serving());
        assert!(!Readiness::Unready.is_serving());
    }

    #[test]
    fn fresh_or_no_provider_is_ready_stale_is_degraded() {
        use crate::flags::FlagProviderHealth;
        let fresh = FlagProviderHealth {
            stale: false,
            last_success_age_ms: Some(0),
            consecutive_failures: 0,
        };
        let stale = FlagProviderHealth {
            stale: true,
            last_success_age_ms: Some(99_999),
            consecutive_failures: 5,
        };
        assert_eq!(evaluate(None), Readiness::Ready);
        assert_eq!(evaluate(Some(&fresh)), Readiness::Ready);
        assert_eq!(evaluate(Some(&stale)), Readiness::Degraded);
    }
}
