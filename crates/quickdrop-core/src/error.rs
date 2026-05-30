//! Unified error type for the core crate.
//!
//! All fallible operations return [`Result<T>`]. Variants are coarse on
//! purpose: callers branch on category (IO, protocol, integrity, …),
//! not on the underlying source error. Sources are preserved via
//! `#[source]` so `tracing` captures full context.

use std::io;

/// Crate-wide result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("serialization error: {0}")]
    Serde(String),

    #[error("database error: {0}")]
    Db(#[from] sled::Error),

    #[error("discovery error: {0}")]
    Discovery(String),

    #[error("transport error: {0}")]
    Transport(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    /// Raised when on-the-fly hash differs from the manifest-declared
    /// hash. Always treated as fatal for the affected file; the partial
    /// `.qdpart` file is removed.
    #[error("integrity check failed: {0}")]
    Integrity(String),

    #[error("peer rejected transfer: {0}")]
    PeerRejected(String),

    #[error("peer not trusted: {0}")]
    NotTrusted(String),

    #[error("operation cancelled")]
    Cancelled,

    #[error("not found: {0}")]
    NotFound(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("internal invariant violated: {0}")]
    Internal(String),
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Serde(e.to_string())
    }
}

impl From<rmp_serde::encode::Error> for Error {
    fn from(e: rmp_serde::encode::Error) -> Self {
        Error::Serde(e.to_string())
    }
}

impl From<rmp_serde::decode::Error> for Error {
    fn from(e: rmp_serde::decode::Error) -> Self {
        Error::Serde(e.to_string())
    }
}
