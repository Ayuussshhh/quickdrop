//! Transfer engine: framed TLS protocol, sender, receiver, resume.

pub mod handshake;
pub mod hash;
pub mod manager;
pub mod protocol;
pub mod receiver;
pub mod sender;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One row in the UI's "active transfers" list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferProgress {
    pub transfer_id: Uuid,
    pub direction: Direction,
    pub peer_name: String,
    pub peer_id: Uuid,
    pub completed_items: u32,
    pub total_items: u32,
    pub bytes_done: u64,
    pub total_bytes: u64,
    pub speed_bps: u64,
    pub state: TransferState,
    /// Human-readable detail, e.g. current file name.
    pub note: String,
    pub started_at_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Send,
    Receive,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    Pending,
    Active,
    Completed,
    Failed,
    Cancelled,
}
