//! Wire protocol message definitions.
//!
//! The shape of these structs is the contract between sender and
//! receiver across versions. Adding fields is allowed (msgpack
//! tolerates unknown fields when deserialized into `serde(default)`),
//! but renaming or removing fields is a breaking change and must
//! bump [`PROTOCOL_VERSION`].

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::discovery::{DeviceType, OsKind};
use crate::identity::Fingerprint;

pub const PROTOCOL_VERSION: u16 = 1;

/// Default chunk size on the wire. Tuned for gigabit LAN throughput
/// vs. progress-event granularity.
pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024; // 1 MiB

/// Sent first by both sides immediately after TLS completes. Carries
/// the public key + a fresh nonce; both ends sign their counterparty's
/// nonce in the second `Auth` message to prove possession of the
/// secret key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    pub id: Uuid,
    pub name: String,
    pub os: OsKind,
    pub device_type: DeviceType,
    pub fingerprint: Fingerprint,
    /// Raw 32-byte Ed25519 verifying key.
    pub verifying_key: [u8; 32],
    /// 16 random bytes the *peer* must sign.
    pub nonce: [u8; 16],
    pub app_version: String,
}

/// Second handshake message. Signature is over
/// `AUTH_DOMAIN || peer_hello_nonce`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Auth {
    /// 64-byte ed25519 signature.
    pub signature: Vec<u8>,
}

pub const AUTH_DOMAIN: &[u8] = b"quickdrop:auth:v1";

/// Top-level role announcement after authentication. The client (the
/// sender) announces what it wants to do; receiver responds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// Sender wants to send a transfer.
    Send {
        transfer_id: Uuid,
        manifest: Manifest,
    },
    /// Sender is requesting a pairing handshake (no transfer yet).
    Pair { sas_nonce: [u8; 16] },
}

/// Receiver's response to a [`Request`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// Accept the send. `start_offsets[i]` is the byte from which the
    /// receiver wants file `i` to resume; `0` means "start fresh".
    Accept { start_offsets: Vec<u64> },
    /// Pairing accepted (peer recorded as trusted on receiver side).
    PairingAccepted,
    /// Reject the request (transfer or pairing).
    Reject { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestItem {
    /// Path relative to the transfer root, using forward slashes.
    pub rel_path: String,
    /// Total size of the file in bytes.
    pub size: u64,
    /// BLAKE3 hash of the entire file, hex-encoded. Verified end-to-end.
    pub blake3_hex: String,
    /// Last-modified time in milliseconds since UNIX epoch (best-effort).
    #[serde(default)]
    pub modified_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub transfer_id: Uuid,
    pub items: Vec<ManifestItem>,
    pub total_bytes: u64,
}

/// Per-file framing. Sent before each file's chunks; the receiver
/// uses this to know exactly how many chunk bytes to consume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileStart {
    pub index: u32,
    /// Byte offset the sender will start streaming from. May be > 0
    /// for a resume.
    pub start_offset: u64,
}

/// Sent at the end of each file's data so the receiver can finalise
/// the file and compare hashes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEnd {
    pub index: u32,
    /// BLAKE3 of the bytes the *sender* actually transmitted in this
    /// stream (not necessarily the whole file when resuming).
    pub stream_blake3_hex: String,
}

/// End-of-transfer marker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferEnd {
    pub transfer_id: Uuid,
    pub status: TransferStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferStatus {
    Completed,
    Failed,
    Cancelled,
}
