//! Top-level error types that cross module boundaries up to the binary.
//!
//! Library modules generally define their own typed errors (`thiserror`) where
//! precision matters; those convert into this enum at the edges, and the binary
//! uses [`anyhow`](https://docs.rs/anyhow) for ergonomic top-level propagation.

use thiserror::Error;

/// Errors that can surface from the library to the binary entrypoint.
#[derive(Debug, Error)]
pub enum Error {
    /// Configuration could not be loaded or failed semantic validation.
    #[error("configuration error: {0}")]
    Config(String),

    /// A behavioral contract could not be loaded, resolved, or validated.
    #[error("contract error: {0}")]
    Contract(String),

    /// An underlying I/O operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Crate-wide result alias defaulting to [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
