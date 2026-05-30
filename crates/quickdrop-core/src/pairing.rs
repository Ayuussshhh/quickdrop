//! Pairing & trust management.
//!
//! This module provides:
//!
//! * [`TrustStore`] — a sled-backed registry of peers we have agreed
//!   to trust. Lookups are O(1) by `Uuid`; we also expose a
//!   fingerprint→peer index so the TLS layer can authorise an
//!   incoming connection without first knowing the peer's UUID.
//! * [`compute_sas`] — derives a 6-digit Short Authentication String
//!   from the two participants' public keys. Both sides display the
//!   same number; a single tap on either side accepts the pairing.
//!
//! The full pairing **flow** (UI prompts, accept/reject events, etc.)
//! lands in Step 9 once the transport layer (Steps 4–5) is in place.
//! Everything in this file is pure data + crypto and is fully tested.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sled::Transactional;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::db::Db;
use crate::discovery::{DeviceType, OsKind};
use crate::identity::Fingerprint;
use crate::{Error, Result};

/// Sled tree name. Bumping the version segment is a one-way migration
/// — existing trust state in the old tree will be ignored.
const TREE_PEERS: &str = "trust/peers/v1";
const TREE_FP_INDEX: &str = "trust/fp_index/v1";

/// A peer the local device has agreed to trust. Persisted verbatim in
/// the trust DB.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedPeer {
    pub id: Uuid,
    pub name: String,
    pub os: OsKind,
    pub device_type: DeviceType,
    pub fingerprint: Fingerprint,
    /// Raw 32-byte Ed25519 verifying key of the peer.
    pub verifying_key: [u8; 32],
    /// Wall-clock millis when this peer was first paired.
    pub paired_at_ms: u64,
    /// Wall-clock millis of the last successful contact.
    pub last_seen_ms: u64,
}

impl TrustedPeer {
    pub fn touch_last_seen(&mut self) {
        self.last_seen_ms = now_ms();
    }
}

/// Persistent registry of trusted peers.
#[derive(Debug, Clone)]
pub struct TrustStore {
    peers: sled::Tree,
    fp_index: sled::Tree,
}

impl TrustStore {
    pub fn open(db: &Db) -> Result<Self> {
        let peers = db.inner.open_tree(TREE_PEERS)?;
        let fp_index = db.inner.open_tree(TREE_FP_INDEX)?;
        Ok(Self { peers, fp_index })
    }

    /// Insert or update a trusted peer. Idempotent.
    pub fn upsert(&self, peer: &TrustedPeer) -> Result<()> {
        let key = peer.id.as_bytes();
        let bytes = serde_json::to_vec(peer)?;
        // Critical: keep the fingerprint index in lock-step with the
        // primary tree so a crash between writes never leaves a
        // dangling fingerprint pointing at a missing peer. sled's
        // transaction API guarantees atomicity across both trees.
        let res: std::result::Result<(), sled::transaction::TransactionError<()>> =
            (&self.peers, &self.fp_index).transaction(|(peers_tx, fp_tx)| {
                peers_tx.insert(key as &[u8], bytes.as_slice())?;
                fp_tx.insert(peer.fingerprint.as_bytes() as &[u8], key as &[u8])?;
                Ok(())
            });
        res.map_err(transaction_err)?;
        self.peers.flush()?;
        Ok(())
    }

    pub fn get(&self, id: Uuid) -> Result<Option<TrustedPeer>> {
        match self.peers.get(id.as_bytes())? {
            Some(ivec) => Ok(Some(serde_json::from_slice(&ivec)?)),
            None => Ok(None),
        }
    }

    pub fn get_by_fingerprint(&self, fp: &Fingerprint) -> Result<Option<TrustedPeer>> {
        let id_bytes = match self.fp_index.get(fp.as_bytes())? {
            Some(b) => b,
            None => return Ok(None),
        };
        let arr: [u8; 16] = id_bytes
            .as_ref()
            .try_into()
            .map_err(|_| Error::Internal("fp_index: corrupt id length".into()))?;
        self.get(Uuid::from_bytes(arr))
    }

    pub fn is_trusted(&self, fp: &Fingerprint) -> Result<bool> {
        Ok(self.fp_index.contains_key(fp.as_bytes())?)
    }

    pub fn remove(&self, id: Uuid) -> Result<bool> {
        let existing = match self.peers.get(id.as_bytes())? {
            Some(b) => b,
            None => return Ok(false),
        };
        let peer: TrustedPeer = serde_json::from_slice(&existing)?;
        let res: std::result::Result<(), sled::transaction::TransactionError<()>> =
            (&self.peers, &self.fp_index).transaction(|(peers_tx, fp_tx)| {
                peers_tx.remove(id.as_bytes() as &[u8])?;
                fp_tx.remove(peer.fingerprint.as_bytes() as &[u8])?;
                Ok(())
            });
        res.map_err(transaction_err)?;
        self.peers.flush()?;
        Ok(true)
    }

    /// Returns all trusted peers, sorted by `paired_at_ms` ascending.
    pub fn all(&self) -> Result<Vec<TrustedPeer>> {
        let mut out = Vec::new();
        for kv in self.peers.iter() {
            let (_, v) = kv?;
            out.push(serde_json::from_slice::<TrustedPeer>(&v)?);
        }
        out.sort_by_key(|p| p.paired_at_ms);
        Ok(out)
    }

    pub fn touch(&self, id: Uuid) -> Result<()> {
        if let Some(mut p) = self.get(id)? {
            p.touch_last_seen();
            self.upsert(&p)?;
        }
        Ok(())
    }
}

fn transaction_err(e: sled::transaction::TransactionError<()>) -> Error {
    match e {
        sled::transaction::TransactionError::Abort(_) => {
            Error::Internal("trust store transaction unexpectedly aborted".into())
        }
        sled::transaction::TransactionError::Storage(inner) => Error::Db(inner),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// Short Authentication String (SAS)
// ---------------------------------------------------------------------

/// 6-digit code derived from two public keys + a per-session nonce.
///
/// The construction is symmetric: feeding `(a, b, n)` and `(b, a, n)`
/// yields the same digits, so both ends of the pairing display an
/// identical number. The user only needs to confirm that the codes
/// match; no typing required.
///
/// `nonce` should be fresh per pairing attempt so a passive attacker
/// who has captured an old SAS exchange can't replay it. In our case
/// it will be a 16-byte random sent in HELLO.
pub fn compute_sas(local_pub: &[u8; 32], remote_pub: &[u8; 32], nonce: &[u8]) -> String {
    // Sort the two public keys to make the input order-independent.
    let (a, b) = if local_pub <= remote_pub {
        (local_pub, remote_pub)
    } else {
        (remote_pub, local_pub)
    };
    let mut h = Sha256::new();
    h.update(b"quickdrop:sas:v1");
    h.update(a);
    h.update(b);
    h.update(nonce);
    let digest = h.finalize();
    // Take 4 leading bytes → u32 → mod 1_000_000 → 6 zero-padded digits.
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    format!("{:06}", n % 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(dir.path()).expect("open db");
        (dir, db)
    }

    fn sample_peer(id: Uuid, name: &str, fp: [u8; 16]) -> TrustedPeer {
        TrustedPeer {
            id,
            name: name.into(),
            os: OsKind::Windows,
            device_type: DeviceType::Laptop,
            fingerprint: Fingerprint(fp),
            verifying_key: [0u8; 32],
            paired_at_ms: 1,
            last_seen_ms: 1,
        }
    }

    #[test]
    fn upsert_get_and_remove() {
        let (_dir, db) = temp_db();
        let store = TrustStore::open(&db).unwrap();
        let id = Uuid::new_v4();
        let p = sample_peer(id, "Alice", [9u8; 16]);
        store.upsert(&p).unwrap();
        let got = store.get(id).unwrap().expect("peer should exist");
        assert_eq!(got.name, "Alice");
        assert!(store.is_trusted(&p.fingerprint).unwrap());

        let by_fp = store.get_by_fingerprint(&p.fingerprint).unwrap();
        assert_eq!(by_fp.unwrap().id, id);

        assert!(store.remove(id).unwrap());
        assert!(store.get(id).unwrap().is_none());
        assert!(!store.is_trusted(&p.fingerprint).unwrap());
        // remove again returns false (idempotent)
        assert!(!store.remove(id).unwrap());
    }

    #[test]
    fn all_returns_sorted() {
        let (_dir, db) = temp_db();
        let store = TrustStore::open(&db).unwrap();
        let mut a = sample_peer(Uuid::new_v4(), "A", [1u8; 16]);
        let mut b = sample_peer(Uuid::new_v4(), "B", [2u8; 16]);
        let mut c = sample_peer(Uuid::new_v4(), "C", [3u8; 16]);
        a.paired_at_ms = 100;
        b.paired_at_ms = 50;
        c.paired_at_ms = 200;
        store.upsert(&a).unwrap();
        store.upsert(&b).unwrap();
        store.upsert(&c).unwrap();
        let names: Vec<String> = store.all().unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["B", "A", "C"]);
    }

    #[test]
    fn sas_is_symmetric_and_deterministic() {
        let mut a = [0u8; 32];
        a[0] = 1;
        let mut b = [0u8; 32];
        b[0] = 2;
        let nonce = [42u8; 16];
        let s1 = compute_sas(&a, &b, &nonce);
        let s2 = compute_sas(&b, &a, &nonce);
        assert_eq!(s1, s2);
        assert_eq!(s1.len(), 6);
        assert!(s1.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn sas_changes_with_nonce() {
        let a = [1u8; 32];
        let b = [2u8; 32];
        let s1 = compute_sas(&a, &b, &[0u8; 16]);
        let s2 = compute_sas(&a, &b, &[1u8; 16]);
        assert_ne!(s1, s2);
    }
}

