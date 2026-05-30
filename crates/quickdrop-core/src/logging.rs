//! Logging initialization.
//!
//! Writes rolling daily logs to `<app_data>/logs/quickdrop.log` and
//! mirrors them to stderr in debug builds. Filter via the
//! `QUICKDROP_LOG` env var (e.g. `QUICKDROP_LOG=debug,sled=warn`).
//! Defaults to `info`.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

/// Returned guard must be kept alive for the lifetime of the process.
/// Dropping it flushes the file appender.
#[must_use = "drop flushes pending log lines; bind to a long-lived variable"]
#[derive(Debug)]
pub struct LogGuard(#[allow(dead_code)] WorkerGuard);

pub fn init(log_dir: &Path) -> LogGuard {
    let file_appender = tracing_appender::rolling::daily(log_dir, "quickdrop.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = EnvFilter::try_from_env("QUICKDROP_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,quickdrop_core=debug"));

    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(false)
        .with_writer(file_writer);

    #[cfg(debug_assertions)]
    let stderr_layer = Some(
        fmt::layer()
            .with_ansi(true)
            .with_target(true)
            .with_writer(std::io::stderr)
            .with_filter(EnvFilter::new("info,quickdrop_core=debug")),
    );
    #[cfg(not(debug_assertions))]
    let stderr_layer: Option<Box<dyn Layer<_> + Send + Sync>> = None;

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer);

    if let Some(stderr) = stderr_layer {
        registry.with(stderr).init();
    } else {
        registry.init();
    }

    tracing::info!(
        version = crate::VERSION,
        log_dir = %log_dir.display(),
        "quickdrop-core logging initialized"
    );

    LogGuard(guard)
}
