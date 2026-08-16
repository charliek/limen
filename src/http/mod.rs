//! The HTTP data plane: server, upstream client, and the streaming proxy core.
//!
//! Limen runs two listeners (Section 3.2): the **data plane** serves proxied
//! client traffic, and the **control plane** serves `/metrics` and the health
//! endpoints. Bodies are handled on two deliberately separate paths
//! (Section 3.3): a default zero-copy streaming path, and a bounded
//! buffer-for-compare path used only when a request is sampled for comparison.
//!
//! The mechanical half of all of this — which headers may cross a hop, how a
//! request path becomes an upstream URL, how a body is buffered without letting
//! an unbounded one buffer, how the upstream client is built — was extracted
//! from these files into [`stridelabs_http::proxy`] and is now consumed from
//! there rather than kept in a second copy. What remains in this module is the
//! part that is about *limen*: the `x-limen-*` header policy, the config shapes,
//! the rollout/shadow decisions those primitives are wired into.
//!
//! Submodules:
//! - [`server`] — the data-plane and control-plane listeners + app state.
//! - [`client`] — the upstream client (TLS, timeouts, pooling).
//! - [`proxy`] — the streaming proxy core.
//! - [`forwarded`] — `X-Forwarded-For`/`X-Forwarded-Proto` injection shared by
//!   the primary and shadow upstream requests.
//!
//! Bounded body buffering lives in [`stridelabs_http::proxy::buffer_or_stream`]
//! and its siblings; limen's own `body` module was this code, and was deleted
//! rather than duplicated when the crate grew the time-bounded variant.

pub mod client;
pub mod forwarded;
pub mod proxy;
pub mod server;
pub mod shadow;

pub use server::{serve, AppState};
