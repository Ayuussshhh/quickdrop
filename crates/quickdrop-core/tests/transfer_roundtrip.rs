//! End-to-end integration tests for the transfer engine.
//!
//! Spins up an in-process receiver bound to a random localhost port,
//! then runs the sender against it. Verifies bytes + BLAKE3 + resume.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use quickdrop_core::config::Settings;
use quickdrop_core::db::Db;
use quickdrop_core::discovery::{DeviceType, OsKind};
use quickdrop_core::identity::{DeviceIdentity, KeyStore};
use quickdrop_core::pairing::TrustStore;
use quickdrop_core::transfer::manager::TransferManager;
use quickdrop_core::transfer::protocol::{Manifest, TransferStatus};
use quickdrop_core::transfer::receiver::{
    self, AcceptDecision, PairDecision, ReceiverConfig, ReceiverHost,
};
use quickdrop_core::transfer::sender::{self, SendItem, SenderConfig};
use quickdrop_core::transfer::{handshake::PeerHandshake, Direction, TransferState};
use uuid::Uuid;

struct InMemKeyStore {
    inner: std::sync::Mutex<Option<String>>,
}
impl InMemKeyStore {
    fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(None),
        }
    }
}
impl KeyStore for InMemKeyStore {
    fn get(&self) -> quickdrop_core::Result<Option<String>> {
        Ok(self.inner.lock().unwrap().clone())
    }
    fn set(&self, encoded: &str) -> quickdrop_core::Result<()> {
        *self.inner.lock().unwrap() = Some(encoded.to_string());
        Ok(())
    }
    fn delete(&self) -> quickdrop_core::Result<()> {
        *self.inner.lock().unwrap() = None;
        Ok(())
    }
}

struct AcceptAllHost;
impl ReceiverHost for AcceptAllHost {
    fn on_transfer_request<'a>(
        &'a self,
        _peer: &'a PeerHandshake,
        _manifest: &'a Manifest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AcceptDecision> + Send + 'a>> {
        Box::pin(async { AcceptDecision::Accept })
    }
    fn on_pair_request<'a>(
        &'a self,
        _peer: &'a PeerHandshake,
        _sas: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PairDecision> + Send + 'a>> {
        Box::pin(async { PairDecision::Accept })
    }
    fn on_progress(
        &self,
        _t: Uuid,
        _p: &PeerHandshake,
        _b: u64,
        _tot: u64,
        _i: u32,
        _r: &str,
    ) {
    }
    fn on_transfer_end(
        &self,
        _t: Uuid,
        _p: &PeerHandshake,
        _s: TransferStatus,
        _f: Vec<PathBuf>,
    ) {
    }
}

fn install_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[tokio::test]
async fn roundtrip_single_file_verifies_blake3() {
    install_provider();
    let tmp = tempdir();
    let dest = tmp.join("recv");
    std::fs::create_dir_all(&dest).unwrap();
    let db_dir = tmp.join("db");
    std::fs::create_dir_all(&db_dir).unwrap();

    let db = Db::open(&db_dir).unwrap();
    let trust = TrustStore::open(&db).unwrap();
    let identity_recv = Arc::new(
        DeviceIdentity::load_or_create_with(&InMemKeyStore::new()).unwrap(),
    );
    let identity_send = Arc::new(
        DeviceIdentity::load_or_create_with(&InMemKeyStore::new()).unwrap(),
    );

    let settings = Arc::new(RwLock::new(Settings {
        device_name: "recv".into(),
        destination: Some(dest.clone()),
        sort_by_category: false,
        auto_accept_trusted: true,
        ..Default::default()
    }));

    let cfg = ReceiverConfig {
        device_name: "recv".into(),
        os: OsKind::Windows,
        device_type: DeviceType::Desktop,
        trust: trust.clone(),
        settings,
        default_dest: dest.clone(),
    };
    let host: Arc<dyn ReceiverHost> = Arc::new(AcceptAllHost);
    let (port, _handle) = receiver::start(cfg, identity_recv, host).await.unwrap();

    // Build a 3 MiB random file.
    let src_dir = tmp.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let src_file = src_dir.join("data.bin");
    let payload: Vec<u8> = (0..(3 * 1024 * 1024_u32)).map(|i| (i % 251) as u8).collect();
    std::fs::write(&src_file, &payload).unwrap();
    let expected = blake3::hash(&payload).to_hex().to_string();

    let send_cfg = SenderConfig {
        device_name: "send".into(),
        os: OsKind::Windows,
        device_type: DeviceType::Desktop,
        trust: TrustStore::open(&Db::open(&tmp.join("sdb")).unwrap()).unwrap(),
    };
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let progress: Arc<sender::Progress> = Arc::new(|_, _, _: &str| {});
    sender::send_to(
        addr,
        send_cfg,
        identity_send,
        vec![SendItem { path: src_file.clone() }],
        progress,
        cancel,
    )
    .await
    .expect("send_to");

    // Wait briefly for receiver to finalize.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let received = dest.join("data.bin");
    assert!(received.exists(), "destination file missing");
    let got = std::fs::read(&received).unwrap();
    assert_eq!(got, payload, "bytes mismatch");
    let got_hash = blake3::hash(&got).to_hex().to_string();
    assert_eq!(got_hash, expected, "hash mismatch");
}

#[tokio::test]
async fn manager_register_publishes_snapshot() {
    let (mgr, mut rx) = TransferManager::new();
    rx.borrow_and_update();
    let _cancel = mgr.register(
        Uuid::new_v4(),
        Direction::Send,
        Uuid::new_v4(),
        "peer".into(),
        1,
        100,
    );
    rx.changed().await.unwrap();
    let v = rx.borrow().clone();
    assert_eq!(v.len(), 1);
    assert!(matches!(v[0].state, TransferState::Pending));
}

fn tempdir() -> PathBuf {
    let p = std::env::temp_dir().join(format!("qdtest-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
}
