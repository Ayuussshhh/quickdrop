//! QuickDrop core library.
//!
//! This crate contains all platform-agnostic logic for the QuickDrop
//! application: device discovery, pairing/trust, the transfer engine,
//! file handling, persistence, and configuration. The Tauri shell
//! (`src-tauri`) is a thin wrapper that exposes these modules to the
//! UI via Tauri commands and events.
//!
//! Modules are intentionally created empty in this scaffolding step
//! (Phase 2 / Step 1). Each subsequent step will fill exactly one
//! module so behavior is incremental and testable.

#![forbid(unsafe_code)]
#![warn(rust_2018_idioms, missing_debug_implementations)]

pub mod config;
pub mod db;
pub mod discovery;
pub mod error;
pub mod files;
pub mod identity;
pub mod logging;
pub mod os;
pub mod pairing;
pub mod transfer;
pub mod transport;

pub use error::{Error, Result};

/// Crate version, surfaced to the UI via `app_info` so the about
/// dialog never drifts from the binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
