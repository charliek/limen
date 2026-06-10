//! The HTTP data plane: server, upstream client, and the streaming proxy core.
//!
//! Limen runs two listeners (Section 3.2): the **data plane** serves proxied
//! client traffic, and the **control plane** serves `/metrics` and the health
//! endpoints. Bodies are handled on two deliberately separate paths
//! (Section 3.3): a default zero-copy streaming path, and a bounded
//! buffer-for-compare path used only when a request is sampled for comparison.
//!
//! Submodules:
//! - [`server`] — the data-plane and control-plane listeners + app state.
//! - [`client`] — the upstream client (TLS, timeouts, pooling).
//! - [`proxy`] — the streaming proxy core.
//! - [`body`] — bounded buffering helpers and body-limit enforcement.

pub mod body;
pub mod client;
pub mod proxy;
pub mod server;
pub mod shadow;

pub use server::{serve, AppState};
