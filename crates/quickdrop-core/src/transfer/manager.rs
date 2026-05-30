//! Active-transfer registry. Lives for the lifetime of the app and
//! provides:
//!
//! * a list of in-flight `TransferProgress` rows for the UI;
//! * cancel handles keyed by transfer id;
//! * a single broadcast channel the Tauri shell forwards to the
//!   frontend as `transfers://updated`.
//!
//! The manager is intentionally synchronous in API (no `.await`
//! required just to add a row) so it can be called from inside hot
//! streaming loops without contention.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::watch;
use uuid::Uuid;

use crate::transfer::{Direction, TransferProgress, TransferState};

#[derive(Debug, Clone)]
pub struct TransferManager {
    inner: Arc<Mutex<HashMap<Uuid, Entry>>>,
    tx: watch::Sender<Vec<TransferProgress>>,
}

#[derive(Debug)]
struct Entry {
    progress: TransferProgress,
    cancel: Arc<AtomicBool>,
    last_progress_at: Instant,
    last_bytes: u64,
}

impl TransferManager {
    pub fn new() -> (Self, watch::Receiver<Vec<TransferProgress>>) {
        let (tx, rx) = watch::channel(Vec::new());
        (
            Self {
                inner: Arc::new(Mutex::new(HashMap::new())),
                tx,
            },
            rx,
        )
    }

    /// Register a new transfer in `Pending` state. Returns the cancel
    /// flag the caller should pass into the streaming routine.
    pub fn register(
        &self,
        transfer_id: Uuid,
        direction: Direction,
        peer_id: Uuid,
        peer_name: String,
        total_items: u32,
        total_bytes: u64,
    ) -> Arc<AtomicBool> {
        let now_ms = now_ms();
        let progress = TransferProgress {
            transfer_id,
            direction,
            peer_name,
            peer_id,
            completed_items: 0,
            total_items,
            bytes_done: 0,
            total_bytes,
            speed_bps: 0,
            state: TransferState::Pending,
            note: String::new(),
            started_at_ms: now_ms,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let mut m = self.inner.lock().unwrap();
        m.insert(
            transfer_id,
            Entry {
                progress,
                cancel: cancel.clone(),
                last_progress_at: Instant::now(),
                last_bytes: 0,
            },
        );
        self.publish(&m);
        cancel
    }

    pub fn set_state(&self, id: Uuid, state: TransferState) {
        let mut m = self.inner.lock().unwrap();
        if let Some(e) = m.get_mut(&id) {
            e.progress.state = state;
        }
        self.publish(&m);
    }

    pub fn update_progress(&self, id: Uuid, bytes_done: u64, file_index: u32, note: &str) {
        let mut m = self.inner.lock().unwrap();
        if let Some(e) = m.get_mut(&id) {
            let now = Instant::now();
            let elapsed = now.duration_since(e.last_progress_at).as_secs_f64();
            if elapsed > 0.25 {
                let delta = bytes_done.saturating_sub(e.last_bytes);
                let inst = (delta as f64 / elapsed) as u64;
                // EWMA smoothing.
                let prev = e.progress.speed_bps;
                e.progress.speed_bps = if prev == 0 { inst } else { (prev * 3 + inst * 7) / 10 };
                e.last_progress_at = now;
                e.last_bytes = bytes_done;
            }
            e.progress.bytes_done = bytes_done;
            e.progress.completed_items = file_index;
            e.progress.note = note.to_string();
            e.progress.state = TransferState::Active;
        }
        self.publish(&m);
    }

    pub fn finish(&self, id: Uuid, state: TransferState) {
        let mut m = self.inner.lock().unwrap();
        if let Some(e) = m.get_mut(&id) {
            e.progress.state = state;
            if matches!(state, TransferState::Completed) {
                e.progress.bytes_done = e.progress.total_bytes;
                e.progress.completed_items = e.progress.total_items;
            }
        }
        self.publish(&m);
    }

    pub fn cancel(&self, id: Uuid) -> bool {
        let m = self.inner.lock().unwrap();
        if let Some(e) = m.get(&id) {
            e.cancel.store(true, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    pub fn snapshot(&self) -> Vec<TransferProgress> {
        let m = self.inner.lock().unwrap();
        m.values().map(|e| e.progress.clone()).collect()
    }

    pub fn cleanup_finished(&self, max_age_secs: u64) {
        let now = now_ms();
        let mut m = self.inner.lock().unwrap();
        m.retain(|_, e| {
            !matches!(
                e.progress.state,
                TransferState::Completed | TransferState::Failed | TransferState::Cancelled
            ) || now.saturating_sub(e.progress.started_at_ms) < max_age_secs * 1000
        });
        self.publish(&m);
    }

    fn publish(&self, m: &HashMap<Uuid, Entry>) {
        let mut v: Vec<TransferProgress> = m.values().map(|e| e.progress.clone()).collect();
        v.sort_by_key(|p| p.started_at_ms);
        let _ = self.tx.send(v);
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
