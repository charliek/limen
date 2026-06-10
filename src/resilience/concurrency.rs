//! Shadow concurrency limiting (spec §9.3).
//!
//! Shadow requests are best-effort: when too many are already in flight, new
//! ones are **skipped** (not queued unboundedly), protecting both the proxy and
//! the new upstream under load. A `0` limit disables shadowing's concurrency cap
//! (treated as effectively unbounded via a large permit count is avoided; `0`
//! means "no limit configured" → always allow).

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// A bounded gate on concurrent in-flight shadow requests. Cheaply cloneable;
/// all clones share the same permit pool.
#[derive(Clone)]
pub struct ShadowLimiter {
    semaphore: Option<Arc<Semaphore>>,
}

impl ShadowLimiter {
    /// Create a limiter allowing `max` concurrent shadows. `max == 0` means no
    /// limit (every shadow is allowed).
    pub fn new(max: usize) -> Self {
        Self {
            semaphore: (max > 0).then(|| Arc::new(Semaphore::new(max))),
        }
    }

    /// Try to reserve a shadow slot without waiting. Returns a permit to hold
    /// for the shadow's lifetime, or `None` if the limit is currently saturated
    /// (the caller should skip the shadow rather than queue). With no limit
    /// configured, always returns a permit.
    pub fn try_acquire(&self) -> Option<ShadowPermit> {
        match &self.semaphore {
            None => Some(ShadowPermit { _permit: None }),
            Some(sem) => sem
                .clone()
                .try_acquire_owned()
                .ok()
                .map(|permit| ShadowPermit {
                    _permit: Some(permit),
                }),
        }
    }
}

/// Held for the duration of a shadow request; releases the slot on drop.
pub struct ShadowPermit {
    _permit: Option<OwnedSemaphorePermit>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_limit_always_acquires() {
        let limiter = ShadowLimiter::new(0);
        let permits: Vec<_> = (0..100).filter_map(|_| limiter.try_acquire()).collect();
        assert_eq!(permits.len(), 100);
    }

    #[test]
    fn saturation_skips() {
        let limiter = ShadowLimiter::new(2);
        let _p1 = limiter.try_acquire().expect("first permit");
        let _p2 = limiter.try_acquire().expect("second permit");
        assert!(limiter.try_acquire().is_none(), "third should be refused");
    }

    #[test]
    fn dropping_a_permit_frees_a_slot() {
        let limiter = ShadowLimiter::new(1);
        let p = limiter.try_acquire().expect("permit");
        assert!(limiter.try_acquire().is_none());
        drop(p);
        assert!(limiter.try_acquire().is_some(), "slot freed after drop");
    }
}
