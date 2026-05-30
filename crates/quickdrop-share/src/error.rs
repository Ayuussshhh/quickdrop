//! Error types for QuickDrop Share.

use std::io;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced by the share subsystem.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] io::Error),

    /// The requested path is not a regular file we can serve.
    #[error("not a regular file: {0}")]
    NotAFile(String),

    /// No usable LAN IPv4 address was found to advertise.
    #[error("no local network address found")]
    NoLocalAddress,

    /// The HTTP listener could not bind any address.
    #[error("failed to bind share server: {0}")]
    Bind(String),

    /// QR rendering failed.
    #[error("qr generation failed: {0}")]
    Qr(String),

    /// Session was not found / expired / revoked. Surfaced as HTTP 404
    /// so the browser side never learns *why* (no enumeration oracle).
    #[error("session not found")]
    SessionNotFound,

    /// Session exists but the supplied password was wrong/missing.
    #[error("unauthorized")]
    Unauthorized,

    /// Session exists but its download budget is exhausted.
    #[error("download limit reached")]
    LimitReached,
}
