//! Readiness evaluation (spec §10.3).
//!
//! `/health/ready` reports whether Limen can safely serve. Readiness is allowed
//! to **degrade** rather than hard-fail when a dependency is unhealthy but the
//! proxy remains safe — e.g. a stale flag provider that has fallen back to the
//! fail-safe mode. A degraded proxy still serves (legacy), so it reports ready.
//!
//! In Phase 2 configuration is validated at startup and there are no runtime
//! providers yet, so readiness is always [`Readiness::Ready`]. Phase 5/7 feed
//! provider-staleness signals into [`evaluate`].

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

/// Evaluate current readiness. Phase 2: config is validated before serving and
/// there are no runtime providers, so the proxy is always ready.
pub fn evaluate() -> Readiness {
    Readiness::Ready
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
    fn phase2_is_ready() {
        assert_eq!(evaluate(), Readiness::Ready);
    }
}
