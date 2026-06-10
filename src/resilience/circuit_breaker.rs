//! Per-route, per-(new-)upstream circuit breaker (spec §9.1).
//!
//! The breaker guards the *new* upstream. It is **closed** by default; it
//! **opens** when the failure rate exceeds the threshold after at least
//! `min_requests`; while **open** the new upstream is avoided (the decision
//! layer routes to legacy); after `open_duration` it goes **half-open** and
//! admits a few trial requests — a success closes it, a failure reopens it.
//!
//! Failures are 5xx responses, connection failures, and timeouts (the caller
//! classifies the outcome and calls [`record`](CircuitBreaker::record)).
//!
//! [`allow`](CircuitBreaker::allow) returns an [`Admission`] token when it
//! admits an attempt (reserving a half-open trial slot if half-open). The caller
//! **must** settle that token exactly once — [`record`](CircuitBreaker::record)
//! with the outcome, or [`release`](CircuitBreaker::release) if the attempt is
//! abandoned before reaching new. The token carries the breaker *generation* it
//! was issued under, so a slow attempt that settles after the breaker has since
//! transitioned (e.g. admitted while closed, settling during a later half-open
//! window) is ignored rather than corrupting the new window's accounting.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::model::CircuitBreakerConfig;

/// A short, stable state label for metrics/logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Requests flow to new normally.
    Closed,
    /// New is avoided; requests route to legacy.
    Open,
    /// A few trial requests are admitted to test recovery.
    HalfOpen,
}

impl BreakerState {
    /// A stable lowercase label.
    pub fn as_str(self) -> &'static str {
        match self {
            BreakerState::Closed => "closed",
            BreakerState::Open => "open",
            BreakerState::HalfOpen => "half_open",
        }
    }
}

/// An opaque admission token from [`allow`](CircuitBreaker::allow). It records
/// the breaker generation the attempt was admitted under so its settlement can
/// be matched to the right episode (see the module docs). Settle it exactly once
/// via [`record`](CircuitBreaker::record) or [`release`](CircuitBreaker::release).
#[derive(Debug, Clone, Copy)]
pub struct Admission {
    generation: u64,
}

#[derive(Debug)]
enum Phase {
    Closed,
    Open { until: Instant },
    HalfOpen,
}

#[derive(Debug)]
struct Inner {
    phase: Phase,
    // Bumped on every phase transition; stamps each `Admission` so a stale
    // settlement (from a prior episode) can be detected and ignored.
    generation: u64,
    // Closed-window counters (reset each evaluation window).
    total: u32,
    failures: u32,
    // Half-open trial counters.
    half_open_in_flight: u32,
    half_open_successes: u32,
}

/// A per-route circuit breaker. Cheaply shareable behind an `Arc`.
#[derive(Debug)]
pub struct CircuitBreaker {
    inner: Mutex<Inner>,
    failure_rate_threshold: f64,
    min_requests: u32,
    open_duration: Duration,
    half_open_max: u32,
}

impl CircuitBreaker {
    /// Build a breaker from its route config.
    pub fn new(config: &CircuitBreakerConfig) -> Self {
        Self {
            inner: Mutex::new(Inner {
                phase: Phase::Closed,
                generation: 0,
                total: 0,
                failures: 0,
                half_open_in_flight: 0,
                half_open_successes: 0,
            }),
            failure_rate_threshold: config.failure_rate_threshold,
            min_requests: config.min_requests.max(1),
            open_duration: Duration::from_millis(config.open_duration_ms),
            half_open_max: config.half_open_max_requests.max(1),
        }
    }

    /// Whether to attempt the new upstream now. While open, returns `None`
    /// (route to legacy) until `open_duration` elapses, then admits up to
    /// `half_open_max` trial requests. A `Some(admission)` result reserves a
    /// trial slot (when half-open), so the caller must settle it exactly once
    /// via [`record`](Self::record) or [`release`](Self::release).
    pub fn allow(&self) -> Option<Admission> {
        let mut inner = self.lock();
        match inner.phase {
            Phase::Closed => Some(Admission {
                generation: inner.generation,
            }),
            Phase::Open { until } => {
                if Instant::now() >= until {
                    inner.phase = Phase::HalfOpen;
                    inner.generation += 1;
                    inner.half_open_in_flight = 1;
                    inner.half_open_successes = 0;
                    Some(Admission {
                        generation: inner.generation,
                    })
                } else {
                    None
                }
            }
            Phase::HalfOpen => {
                if inner.half_open_in_flight < self.half_open_max {
                    inner.half_open_in_flight += 1;
                    Some(Admission {
                        generation: inner.generation,
                    })
                } else {
                    None
                }
            }
        }
    }

    /// Release a half-open trial slot reserved by [`allow`](Self::allow) without
    /// recording an outcome — for when an admitted attempt is abandoned before
    /// it reaches the new upstream (e.g. the request is rejected locally). A
    /// no-op if the breaker has since transitioned (stale admission) or is not
    /// half-open, since only half-open `allow`s reserve a slot.
    pub fn release(&self, admission: Admission) {
        let mut inner = self.lock();
        if admission.generation == inner.generation && matches!(inner.phase, Phase::HalfOpen) {
            inner.half_open_in_flight = inner.half_open_in_flight.saturating_sub(1);
        }
    }

    /// Record the outcome of an attempt admitted by [`allow`](Self::allow). If
    /// the breaker has transitioned since the attempt was admitted (the
    /// admission's generation is stale), the outcome belonged to a now-closed
    /// episode and is ignored, so it cannot corrupt the current window.
    pub fn record(&self, admission: Admission, success: bool) {
        let mut inner = self.lock();
        if admission.generation != inner.generation {
            return;
        }
        match inner.phase {
            Phase::Closed => {
                inner.total += 1;
                if !success {
                    inner.failures += 1;
                }
                if inner.total >= self.min_requests {
                    let rate = f64::from(inner.failures) / f64::from(inner.total);
                    if rate > self.failure_rate_threshold {
                        inner.phase = Phase::Open {
                            until: Instant::now() + self.open_duration,
                        };
                        inner.generation += 1;
                    }
                    // Start a fresh window either way.
                    inner.total = 0;
                    inner.failures = 0;
                }
            }
            Phase::HalfOpen => {
                inner.half_open_in_flight = inner.half_open_in_flight.saturating_sub(1);
                if success {
                    inner.half_open_successes += 1;
                    if inner.half_open_successes >= self.half_open_max {
                        inner.phase = Phase::Closed;
                        inner.generation += 1;
                        inner.total = 0;
                        inner.failures = 0;
                        inner.half_open_successes = 0;
                    }
                } else {
                    inner.phase = Phase::Open {
                        until: Instant::now() + self.open_duration,
                    };
                    inner.generation += 1;
                    inner.half_open_successes = 0;
                }
            }
            // A matching-generation record while open cannot occur (entering
            // open bumps the generation), but ignore defensively.
            Phase::Open { .. } => {}
        }
    }

    /// The current state, for metrics/logs. A pure read — it never mutates the
    /// breaker (unlike [`allow`](Self::allow), which performs the actual
    /// open→half-open transition). Once the open window has elapsed it reports
    /// `HalfOpen`, the state the next admitted request will observe.
    pub fn state(&self) -> BreakerState {
        let inner = self.lock();
        match inner.phase {
            Phase::Closed => BreakerState::Closed,
            Phase::HalfOpen => BreakerState::HalfOpen,
            Phase::Open { until } if Instant::now() >= until => BreakerState::HalfOpen,
            Phase::Open { .. } => BreakerState::Open,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A reserved breaker attempt: a breaker handle paired with the [`Admission`]
/// token from [`allow`](CircuitBreaker::allow). It encapsulates the
/// settle-exactly-once obligation so callers don't thread tokens by hand —
/// settle via [`record`](Self::record) or [`release`](Self::release).
#[derive(Debug, Clone)]
pub struct BreakerReservation {
    breaker: Arc<CircuitBreaker>,
    admission: Admission,
}

impl BreakerReservation {
    /// Pair a breaker with an admission token it issued.
    pub fn new(breaker: Arc<CircuitBreaker>, admission: Admission) -> Self {
        Self { breaker, admission }
    }

    /// Record the attempt's outcome on the breaker.
    pub fn record(&self, success: bool) {
        self.breaker.record(self.admission, success);
    }

    /// Release the reserved slot without recording an outcome.
    pub fn release(&self) {
        self.breaker.release(self.admission);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(threshold: f64, min: u32, open_ms: u64, half_open: u32) -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            enabled: true,
            failure_rate_threshold: threshold,
            min_requests: min,
            open_duration_ms: open_ms,
            half_open_max_requests: half_open,
        }
    }

    /// Admit an attempt, panicking if the breaker refuses (test convenience).
    fn admit(cb: &CircuitBreaker) -> Admission {
        cb.allow().expect("breaker should admit")
    }

    #[test]
    fn opens_when_failure_rate_exceeds_threshold() {
        let cb = CircuitBreaker::new(&config(0.5, 4, 30_000, 2));
        // 3 failures + 1 success over 4 = 75% > 50% -> opens.
        for _ in 0..3 {
            cb.record(admit(&cb), false);
        }
        cb.record(admit(&cb), true);
        assert_eq!(cb.state(), BreakerState::Open);
        assert!(cb.allow().is_none(), "open breaker refuses new");
    }

    #[test]
    fn stays_closed_below_threshold() {
        let cb = CircuitBreaker::new(&config(0.5, 4, 30_000, 2));
        // 1 failure + 3 successes = 25% <= 50% -> stays closed.
        cb.record(admit(&cb), false);
        cb.record(admit(&cb), true);
        cb.record(admit(&cb), true);
        cb.record(admit(&cb), true);
        assert_eq!(cb.state(), BreakerState::Closed);
        assert!(cb.allow().is_some());
    }

    #[test]
    fn open_transitions_to_half_open_then_closes_on_success() {
        let cb = CircuitBreaker::new(&config(0.5, 2, 10, 2));
        cb.record(admit(&cb), false);
        cb.record(admit(&cb), false); // 100% over 2 -> open
        assert_eq!(cb.state(), BreakerState::Open);

        std::thread::sleep(Duration::from_millis(20));
        // After open_duration, allow admits a trial (half-open).
        let t1 = cb.allow().expect("half-open trial 1");
        assert_eq!(cb.state(), BreakerState::HalfOpen);
        cb.record(t1, true);
        let t2 = cb.allow().expect("half-open trial 2");
        cb.record(t2, true); // half_open_max (2) successes -> closed
        assert_eq!(cb.state(), BreakerState::Closed);
    }

    #[test]
    fn half_open_failure_reopens() {
        let cb = CircuitBreaker::new(&config(0.5, 2, 10, 2));
        cb.record(admit(&cb), false);
        cb.record(admit(&cb), false); // open
        std::thread::sleep(Duration::from_millis(20));
        let trial = cb.allow().expect("half-open trial");
        cb.record(trial, false); // trial fails -> reopen
        assert_eq!(cb.state(), BreakerState::Open);
    }

    #[test]
    fn half_open_limits_concurrent_trials() {
        let cb = CircuitBreaker::new(&config(0.5, 2, 10, 1));
        cb.record(admit(&cb), false);
        cb.record(admit(&cb), false); // open
        std::thread::sleep(Duration::from_millis(20));
        assert!(cb.allow().is_some(), "first trial admitted");
        assert!(
            cb.allow().is_none(),
            "second trial refused while one is in flight (max 1)"
        );
    }

    #[test]
    fn release_frees_a_half_open_slot_without_counting_an_outcome() {
        let cb = CircuitBreaker::new(&config(0.5, 2, 10, 1));
        cb.record(admit(&cb), false);
        cb.record(admit(&cb), false); // open
        std::thread::sleep(Duration::from_millis(20));
        let trial = cb
            .allow()
            .expect("first trial admitted, reserving the only slot");
        assert!(cb.allow().is_none(), "slot is taken");
        // Abandoning the admitted attempt frees the slot — without closing or
        // reopening the breaker, so the next request can still trial new.
        cb.release(trial);
        assert_eq!(cb.state(), BreakerState::HalfOpen);
        assert!(cb.allow().is_some(), "released slot is available again");
    }

    #[test]
    fn release_is_a_noop_while_closed() {
        let cb = CircuitBreaker::new(&config(0.5, 4, 30_000, 2));
        let a = admit(&cb);
        cb.release(a);
        assert_eq!(cb.state(), BreakerState::Closed);
        assert!(cb.allow().is_some());
    }

    #[test]
    fn stale_admission_does_not_corrupt_a_later_half_open_window() {
        // Regression for the phase-mismatch race: an attempt admitted in one
        // episode that settles after the breaker has transitioned must not
        // touch the current window's accounting.
        let cb = CircuitBreaker::new(&config(0.5, 2, 10, 1));
        // Admitted while closed...
        let stale = cb.allow().expect("closed admits");
        // ...but before it settles, the breaker opens, then half-opens.
        cb.record(admit(&cb), false);
        cb.record(admit(&cb), false); // 2 failures over min_requests=2 -> open
        assert_eq!(cb.state(), BreakerState::Open);
        std::thread::sleep(Duration::from_millis(20));
        let trial = cb.allow().expect("open elapsed -> half-open trial");
        assert_eq!(cb.state(), BreakerState::HalfOpen);
        assert!(cb.allow().is_none(), "the only half-open slot is taken");

        // The stale closed-era settlement must be ignored: it must neither free
        // the in-flight trial's slot nor count toward closing the breaker.
        cb.record(stale, true);
        assert_eq!(cb.state(), BreakerState::HalfOpen, "still half-open");
        assert!(
            cb.allow().is_none(),
            "stale settle must not have freed the in-flight trial slot"
        );

        // The real trial closes the breaker on success.
        cb.record(trial, true);
        assert_eq!(cb.state(), BreakerState::Closed);
    }
}
