//! Share sessions and their lifecycle.
//!
//! A [`SessionManager`] owns every active [`ShareSession`]. Sessions
//! are keyed by a cryptographically random, URL-safe id (the only
//! secret protecting a share). All authorization — expiry, download
//! budget, optional password — is enforced *here*, on the host, never
//! on the browser side. The browser only ever learns "yes" or "404".
//!
//! ## Threat model (why each field exists)
//! - **URL guessing / enumeration**: `session_id` is 256 bits of CSPRNG
//!   entropy, hex-encoded. Unknown/expired/revoked ids all return the
//!   same `SessionNotFound`, so there is no oracle.
//! - **Unauthorized downloads**: optional `password_hash` gates the
//!   stream; comparison is constant-time.
//! - **Replay / link leakage**: `expires_at` and `max_downloads` bound
//!   how long and how often a leaked link is useful.
//! - **Path traversal / file enumeration**: the served path is fixed at
//!   creation and stored server-side only. The browser never supplies a
//!   path, so there is nothing to traverse.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Length of a session id in random bytes (256 bits). Hex-encoded to
/// 64 url-safe characters.
const SESSION_ID_BYTES: usize = 32;

/// Milliseconds since the UNIX epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Options supplied by the host when creating a share.
#[derive(Debug, Clone)]
pub struct ShareOptions {
    /// Absolute path to the file being shared.
    pub file_path: PathBuf,
    /// How long the share stays alive, in seconds.
    pub ttl_secs: u64,
    /// Maximum number of completed downloads; `0` means unlimited.
    pub max_downloads: u32,
    /// Optional plaintext password. Hashed immediately; never stored.
    pub password: Option<String>,
}

impl Default for ShareOptions {
    fn default() -> Self {
        Self {
            file_path: PathBuf::new(),
            ttl_secs: 30 * 60, // 30 minutes
            max_downloads: 0,
            password: None,
        }
    }
}

/// Internal, authoritative session record. Lives behind an `Arc` so a
/// long-running download can hold a snapshot while the manager map is
/// unlocked.
#[derive(Debug)]
pub struct SessionEntry {
    pub id: String,
    pub file_name: String,
    pub file_path: PathBuf,
    pub file_size: u64,
    pub content_type: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub max_downloads: u32,
    pub download_count: AtomicU32,
    pub revoked: AtomicBool,
    /// `blake3(password)` if password-protected, else `None`.
    password_hash: Option<[u8; 32]>,
}

impl SessionEntry {
    fn is_expired(&self, now: u64) -> bool {
        now >= self.expires_at
    }

    fn is_alive(&self, now: u64) -> bool {
        !self.revoked.load(Ordering::Relaxed) && !self.is_expired(now)
    }

    /// Check the supplied password (if any) in constant time.
    pub fn check_password(&self, supplied: Option<&str>) -> bool {
        match &self.password_hash {
            None => true,
            Some(expected) => {
                let got = blake3::hash(supplied.unwrap_or("").as_bytes());
                constant_time_eq(got.as_bytes(), expected)
            }
        }
    }

    /// Host-facing snapshot (includes the local file path).
    pub fn snapshot(&self) -> ShareSession {
        ShareSession {
            session_id: self.id.clone(),
            file_name: self.file_name.clone(),
            file_size: self.file_size,
            created_at: self.created_at,
            expires_at: self.expires_at,
            download_count: self.download_count.load(Ordering::Relaxed),
            max_downloads: self.max_downloads,
            file_path: self.file_path.clone(),
            password_protected: self.password_hash.is_some(),
        }
    }

    /// Browser-facing snapshot (no local path, no internal flags that
    /// would aid an attacker).
    pub fn public_view(&self) -> PublicSession {
        PublicSession {
            session_id: self.id.clone(),
            file_name: self.file_name.clone(),
            file_size: self.file_size,
            expires_at: self.expires_at,
            password_protected: self.password_hash.is_some(),
        }
    }
}

/// Host-facing session snapshot, returned to the desktop UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareSession {
    pub session_id: String,
    pub file_name: String,
    pub file_size: u64,
    pub created_at: u64,
    pub expires_at: u64,
    pub download_count: u32,
    pub max_downloads: u32,
    pub file_path: PathBuf,
    pub password_protected: bool,
}

/// Browser-facing session metadata (`GET /api/session/:id`). Carefully
/// excludes `file_path` and any host-only detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicSession {
    pub session_id: String,
    pub file_name: String,
    pub file_size: u64,
    pub expires_at: u64,
    pub password_protected: bool,
}

/// Live registry of share sessions. Cheap to clone (`Arc` inside).
#[derive(Debug, Clone, Default)]
pub struct SessionManager {
    inner: Arc<RwLock<HashMap<String, Arc<SessionEntry>>>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new share for `opts.file_path`. Validates that the path
    /// is a regular, readable file before issuing an id.
    pub fn create(&self, opts: ShareOptions) -> Result<Arc<SessionEntry>> {
        let meta = std::fs::metadata(&opts.file_path)?;
        if !meta.is_file() {
            return Err(Error::NotAFile(opts.file_path.display().to_string()));
        }
        let file_name = opts
            .file_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "download".to_string());
        let content_type = mime_guess::from_path(&opts.file_path)
            .first_or_octet_stream()
            .essence_str()
            .to_string();

        let now = now_ms();
        let entry = Arc::new(SessionEntry {
            id: random_session_id(),
            file_name,
            file_path: opts.file_path,
            file_size: meta.len(),
            content_type,
            created_at: now,
            expires_at: now + opts.ttl_secs.saturating_mul(1000),
            max_downloads: opts.max_downloads,
            download_count: AtomicU32::new(0),
            revoked: AtomicBool::new(false),
            password_hash: opts
                .password
                .as_deref()
                .filter(|p| !p.is_empty())
                .map(|p| *blake3::hash(p.as_bytes()).as_bytes()),
        });
        self.inner
            .write()
            .unwrap()
            .insert(entry.id.clone(), entry.clone());
        tracing::info!(id = %entry.id, file = %entry.file_name, "share session created");
        Ok(entry)
    }

    /// Look up a *live* session. Expired/revoked sessions are treated as
    /// absent so callers cannot distinguish them.
    pub fn get_alive(&self, id: &str) -> Option<Arc<SessionEntry>> {
        let now = now_ms();
        let e = self.inner.read().unwrap().get(id).cloned()?;
        e.is_alive(now).then_some(e)
    }

    /// Reserve one download slot, enforcing the budget atomically.
    /// Returns the entry on success so the caller can stream it.
    pub fn try_begin_download(&self, id: &str, password: Option<&str>) -> Result<Arc<SessionEntry>> {
        let entry = self.get_alive(id).ok_or(Error::SessionNotFound)?;
        if !entry.check_password(password) {
            return Err(Error::Unauthorized);
        }
        if entry.max_downloads != 0 {
            // Atomically claim a slot; roll back if we overshot.
            let prev = entry.download_count.fetch_add(1, Ordering::SeqCst);
            if prev >= entry.max_downloads {
                entry.download_count.fetch_sub(1, Ordering::SeqCst);
                return Err(Error::LimitReached);
            }
        } else {
            entry.download_count.fetch_add(1, Ordering::SeqCst);
        }
        Ok(entry)
    }

    /// Explicitly revoke + drop a session ("Stop sharing").
    pub fn remove(&self, id: &str) -> bool {
        if let Some(e) = self.inner.write().unwrap().remove(id) {
            e.revoked.store(true, Ordering::Relaxed);
            tracing::info!(%id, "share session removed");
            true
        } else {
            false
        }
    }

    /// Host-facing list of all currently live sessions.
    pub fn list(&self) -> Vec<ShareSession> {
        let now = now_ms();
        let mut v: Vec<ShareSession> = self
            .inner
            .read()
            .unwrap()
            .values()
            .filter(|e| e.is_alive(now))
            .map(|e| e.snapshot())
            .collect();
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        v
    }

    /// Drop expired/revoked sessions. Called periodically by the server.
    pub fn sweep(&self) {
        let now = now_ms();
        self.inner
            .write()
            .unwrap()
            .retain(|_, e| e.is_alive(now));
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().is_empty()
    }
}

/// 256 bits of CSPRNG entropy, hex-encoded → 64 url-safe chars.
fn random_session_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; SESSION_ID_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Length-independent (for equal lengths) constant-time comparison.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Best-effort validation that a session id looks like one we issued.
/// Cheap pre-filter so obviously-bogus ids never touch the map.
pub fn looks_like_session_id(id: &str) -> bool {
    id.len() == SESSION_ID_BYTES * 2 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Helper for callers that need just the file name of a path.
pub fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(content: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content).unwrap();
        (dir, path)
    }

    #[test]
    fn session_id_is_random_and_well_formed() {
        let a = random_session_id();
        let b = random_session_id();
        assert_ne!(a, b);
        assert!(looks_like_session_id(&a));
        assert!(!looks_like_session_id("short"));
        assert!(!looks_like_session_id(&"z".repeat(64)));
    }

    #[test]
    fn create_rejects_non_files() {
        let mgr = SessionManager::new();
        let dir = tempfile::tempdir().unwrap();
        let opts = ShareOptions {
            file_path: dir.path().to_path_buf(),
            ..Default::default()
        };
        assert!(matches!(mgr.create(opts), Err(Error::NotAFile(_))));
    }

    #[test]
    fn expiry_hides_session() {
        let (_d, path) = temp_file(b"hi");
        let mgr = SessionManager::new();
        let e = mgr
            .create(ShareOptions {
                file_path: path,
                ttl_secs: 0,
                ..Default::default()
            })
            .unwrap();
        // ttl 0 → already expired.
        assert!(mgr.get_alive(&e.id).is_none());
        assert!(matches!(
            mgr.try_begin_download(&e.id, None),
            Err(Error::SessionNotFound)
        ));
    }

    #[test]
    fn download_budget_is_enforced() {
        let (_d, path) = temp_file(b"hi");
        let mgr = SessionManager::new();
        let e = mgr
            .create(ShareOptions {
                file_path: path,
                ttl_secs: 60,
                max_downloads: 2,
                ..Default::default()
            })
            .unwrap();
        assert!(mgr.try_begin_download(&e.id, None).is_ok());
        assert!(mgr.try_begin_download(&e.id, None).is_ok());
        assert!(matches!(
            mgr.try_begin_download(&e.id, None),
            Err(Error::LimitReached)
        ));
    }

    #[test]
    fn password_is_required_and_constant_time_checked() {
        let (_d, path) = temp_file(b"hi");
        let mgr = SessionManager::new();
        let e = mgr
            .create(ShareOptions {
                file_path: path,
                ttl_secs: 60,
                password: Some("swordfish".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(matches!(
            mgr.try_begin_download(&e.id, None),
            Err(Error::Unauthorized)
        ));
        assert!(matches!(
            mgr.try_begin_download(&e.id, Some("wrong")),
            Err(Error::Unauthorized)
        ));
        assert!(mgr.try_begin_download(&e.id, Some("swordfish")).is_ok());
    }

    #[test]
    fn remove_revokes() {
        let (_d, path) = temp_file(b"hi");
        let mgr = SessionManager::new();
        let e = mgr
            .create(ShareOptions {
                file_path: path,
                ttl_secs: 60,
                ..Default::default()
            })
            .unwrap();
        assert!(mgr.remove(&e.id));
        assert!(mgr.get_alive(&e.id).is_none());
        assert!(!mgr.remove(&e.id));
    }
}
