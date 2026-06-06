//! Persistent transfer history.
//!
//! The [`TransferManager`](crate::transfer::manager::TransferManager) only
//! tracks *in-flight* transfers in memory; once a transfer finishes its row
//! is just a transient UI artifact. This module is the durable record of
//! everything that has completed (or failed), so the UI can show a "History"
//! view across restarts.
//!
//! Storage is a single sled tree. Records are keyed by a big-endian
//! `timestamp_ms` prefix followed by the transfer UUID, which makes
//! chronological iteration a cheap forward/reverse scan and keeps the key
//! unique even when two transfers share a millisecond.
//!
//! We deliberately store history **once**, at the completion callbacks in the
//! Tauri shell, rather than mirroring the live `TransferManager` — there is no
//! duplicate source of truth.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::Db;
use crate::transfer::{Direction, TransferState};
use crate::Result;

const TREE_HISTORY: &str = "history/records/v1";

/// One completed (or failed/cancelled) transfer, persisted for the
/// History view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferRecord {
    /// The transfer's UUID (same id the live manager used).
    pub id: Uuid,
    /// Display name: the single file, or `"N files"` for a batch.
    pub file_name: String,
    pub direction: Direction,
    pub peer_id: Uuid,
    /// Where the bytes came from (device name).
    pub source_device: String,
    /// Where the bytes went (device name).
    pub target_device: String,
    /// Wall-clock millis when the transfer finished.
    pub timestamp_ms: u64,
    /// Total bytes transferred.
    pub size: u64,
    pub status: TransferState,
    /// Absolute paths involved, for Open File / Open Folder / Resend.
    /// For sends these are the local source paths; for receives the
    /// files written to disk. May be empty if unknown.
    #[serde(default)]
    pub paths: Vec<PathBuf>,
}

/// Durable, append-mostly registry of completed transfers.
#[derive(Debug, Clone)]
pub struct HistoryStore {
    tree: sled::Tree,
}

impl HistoryStore {
    pub fn open(db: &Db) -> Result<Self> {
        let tree = db.inner.open_tree(TREE_HISTORY)?;
        Ok(Self { tree })
    }

    /// Persist a finished transfer. Idempotent per `(timestamp, id)` key.
    pub fn record(&self, rec: &TransferRecord) -> Result<()> {
        let key = key_for(rec.timestamp_ms, &rec.id);
        let bytes = serde_json::to_vec(rec)?;
        self.tree.insert(key, bytes)?;
        self.tree.flush()?;
        Ok(())
    }

    /// All records, newest first.
    pub fn list(&self) -> Result<Vec<TransferRecord>> {
        let mut out = Vec::new();
        for kv in self.tree.iter() {
            let (_, v) = kv?;
            out.push(serde_json::from_slice::<TransferRecord>(&v)?);
        }
        out.sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));
        Ok(out)
    }

    /// Remove a single record by id. Returns `true` if something was
    /// deleted.
    pub fn delete(&self, id: Uuid) -> Result<bool> {
        let mut victim = None;
        for kv in self.tree.iter() {
            let (k, v) = kv?;
            let rec: TransferRecord = serde_json::from_slice(&v)?;
            if rec.id == id {
                victim = Some(k);
                break;
            }
        }
        match victim {
            Some(k) => {
                self.tree.remove(k)?;
                self.tree.flush()?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Wipe all history.
    pub fn clear(&self) -> Result<()> {
        self.tree.clear()?;
        self.tree.flush()?;
        Ok(())
    }
}

fn key_for(timestamp_ms: u64, id: &Uuid) -> Vec<u8> {
    let mut key = Vec::with_capacity(8 + 16);
    key.extend_from_slice(&timestamp_ms.to_be_bytes());
    key.extend_from_slice(id.as_bytes());
    key
}

/// Current wall-clock time in millis since the UNIX epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> (tempfile::TempDir, HistoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let store = HistoryStore::open(&db).unwrap();
        (dir, store)
    }

    fn rec(id: Uuid, ts: u64) -> TransferRecord {
        TransferRecord {
            id,
            file_name: "a.txt".into(),
            direction: Direction::Send,
            peer_id: Uuid::new_v4(),
            source_device: "me".into(),
            target_device: "them".into(),
            timestamp_ms: ts,
            size: 10,
            status: TransferState::Completed,
            paths: vec![PathBuf::from("/tmp/a.txt")],
        }
    }

    #[test]
    fn record_list_newest_first() {
        let (_d, s) = temp();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        s.record(&rec(a, 100)).unwrap();
        s.record(&rec(b, 200)).unwrap();
        let all = s.list().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, b); // newest first
        assert_eq!(all[1].id, a);
    }

    #[test]
    fn delete_and_clear() {
        let (_d, s) = temp();
        let a = Uuid::new_v4();
        s.record(&rec(a, 1)).unwrap();
        assert!(s.delete(a).unwrap());
        assert!(!s.delete(a).unwrap());
        s.record(&rec(Uuid::new_v4(), 2)).unwrap();
        s.clear().unwrap();
        assert!(s.list().unwrap().is_empty());
    }
}
