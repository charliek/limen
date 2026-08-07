//! Observe mode: passive profiling of the traffic limen already relays.
//!
//! The recorder, the bounded per-route aggregates and the profile document
//! land here; today the module owns the one thing config validation and the
//! control plane have to agree on before either exists — the path the profile
//! is served from.

/// Control-plane path the observe profile is served from.
///
/// Lives here rather than beside the handler because
/// [`crate::config::validate`] needs it too: the metrics path is
/// operator-supplied and registered on the same router, and axum panics at
/// router *build* time on a duplicate route. Validating the collision turns
/// that abort into a refuse-to-start (invariant 7).
pub const OBSERVE_PROFILE_PATH: &str = "/observe/profile";
