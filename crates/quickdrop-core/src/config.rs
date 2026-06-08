//! Application configuration.
//!
//! Resolves the QuickDrop data directory, default destination folder,
//! and friendly device name. All paths are surfaced through a single
//! [`Paths`] struct so the rest of the codebase never hardcodes
//! `%APPDATA%` or `C:\QuickDrop\`.

use std::path::{Path, PathBuf};

use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// All filesystem locations QuickDrop uses at runtime.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `%APPDATA%\QuickDrop\` — settings, sled DB, logs.
    pub app_data: PathBuf,
    /// `%APPDATA%\QuickDrop\db\` — sled database root.
    pub db_dir: PathBuf,
    /// `%APPDATA%\QuickDrop\logs\` — rotating log files.
    pub log_dir: PathBuf,
    /// Default receive destination, e.g. `C:\QuickDrop\`.
    pub default_dest: PathBuf,
}

impl Paths {
    pub fn resolve() -> Result<Self> {
        let proj = ProjectDirs::from("com", "QuickDrop", "QuickDrop")
            .ok_or_else(|| Error::Config("could not resolve project dirs".into()))?;
        let app_data = proj.data_dir().to_path_buf();
        let db_dir = app_data.join("db");
        let log_dir = app_data.join("logs");

        // Default destination: <user home drive>\QuickDrop on Windows,
        // `~/QuickDrop` elsewhere. Avoids polluting Documents/Downloads.
        let default_dest = default_dest_dir()?;

        for d in [&app_data, &db_dir, &log_dir, &default_dest] {
            ensure_dir(d)?;
        }

        Ok(Self {
            app_data,
            db_dir,
            log_dir,
            default_dest,
        })
    }
}

fn default_dest_dir() -> Result<PathBuf> {
    if cfg!(windows) {
        // Place under the user profile drive root: C:\QuickDrop.
        let base = BaseDirs::new()
            .ok_or_else(|| Error::Config("no base dirs".into()))?;
        let home = base.home_dir();
        // Take the drive letter root of the home dir, e.g. "C:\".
        let root: PathBuf = home
            .components()
            .next()
            .map(|c| Path::new(c.as_os_str()).to_path_buf())
            .unwrap_or_else(|| PathBuf::from("C:\\"));
        Ok(root.join("QuickDrop"))
    } else {
        let base = BaseDirs::new()
            .ok_or_else(|| Error::Config("no base dirs".into()))?;
        Ok(base.home_dir().join("QuickDrop"))
    }
}

fn ensure_dir(p: &Path) -> Result<()> {
    if !p.exists() {
        std::fs::create_dir_all(p)?;
    }
    Ok(())
}

/// Well-known user destination folders offered when the receiver picks
/// where an incoming transfer should land. Each is best-effort; a
/// missing OS folder falls back to the user's home directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DestOptions {
    pub downloads: PathBuf,
    pub desktop: PathBuf,
    pub documents: PathBuf,
}

/// Resolve the user's Downloads folder, falling back to `~/Downloads`
/// then the home directory. Used as the default receive location.
pub fn downloads_dir() -> Option<PathBuf> {
    use directories::UserDirs;
    let dirs = UserDirs::new()?;
    if let Some(d) = dirs.download_dir() {
        return Some(d.to_path_buf());
    }
    Some(dirs.home_dir().join("Downloads"))
}

/// Resolve Downloads / Desktop / Documents for the receive-destination
/// picker. Any folder the OS can't report falls back to the home dir.
pub fn dest_options() -> DestOptions {
    use directories::UserDirs;
    let dirs = UserDirs::new();
    let home = dirs
        .as_ref()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let pick = |sub: Option<&Path>, fallback: &str| {
        sub.map(|p| p.to_path_buf())
            .unwrap_or_else(|| home.join(fallback))
    };
    DestOptions {
        downloads: pick(dirs.as_ref().and_then(|d| d.download_dir()), "Downloads"),
        desktop: pick(dirs.as_ref().and_then(|d| d.desktop_dir()), "Desktop"),
        documents: pick(dirs.as_ref().and_then(|d| d.document_dir()), "Documents"),
    }
}

/// User-editable settings persisted to `<app_data>/settings.json`.
/// Kept tiny on purpose. Anything large (peer trust, transfer history)
/// lives in sled, not here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Friendly name shown to other devices, e.g. "Ayush Desktop".
    pub device_name: String,
    /// Override for the default receive folder. `None` ⇒ use [`Paths::default_dest`].
    pub destination: Option<PathBuf>,
    /// If true, sort received files into Images/Videos/Documents/Archives/Other.
    pub sort_by_category: bool,
    /// Auto-accept transfers from already-trusted peers (no prompt).
    pub auto_accept_trusted: bool,
    /// Start QuickDrop on Windows login.
    pub start_on_login: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            device_name: default_device_name(),
            destination: None,
            sort_by_category: true,
            auto_accept_trusted: true,
            start_on_login: false,
        }
    }
}

fn default_device_name() -> String {
    // Best-effort hostname; falls back to a generic label.
    if let Ok(name) = std::env::var("COMPUTERNAME") {
        if !name.is_empty() {
            return name;
        }
    }
    if let Ok(name) = std::env::var("HOSTNAME") {
        if !name.is_empty() {
            return name;
        }
    }
    "QuickDrop Device".to_string()
}

impl Settings {
    pub fn load_or_default(paths: &Paths) -> Self {
        let f = paths.app_data.join("settings.json");
        match std::fs::read(&f) {
            Ok(bytes) => match serde_json::from_slice::<Settings>(&bytes) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "settings.json malformed, using defaults");
                    Settings::default()
                }
            },
            Err(_) => Settings::default(),
        }
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let f = paths.app_data.join("settings.json");
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(f, bytes)?;
        Ok(())
    }
}
