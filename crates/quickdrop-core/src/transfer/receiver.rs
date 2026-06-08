//! Receiving side of the transfer engine.
//!
//! Owns the TLS listener and accepts incoming connections. For each
//! connection:
//!
//! 1. Run the application handshake.
//! 2. Decide whether the peer is allowed to either pair or send.
//! 3. For a `Send`, sanitise every `rel_path`, decide resume offsets
//!    by looking at any pre-existing `.qdpart` file, then stream
//!    chunks to disk and verify BLAKE3 at the end of each file.
//! 4. Atomically move `.qdpart` → final on success.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::fs::OpenOptions;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{server::TlsStream, TlsAcceptor};

use crate::config::Settings;
use crate::discovery::{DeviceType, OsKind};
use crate::files;
use crate::identity::DeviceIdentity;
use crate::pairing::TrustStore;
use crate::transfer::handshake::{self, PeerHandshake};
use crate::transfer::protocol::{
    FileEnd, FileStart, Manifest, Request, Response, TransferEnd, TransferStatus,
};
use crate::transport::{self, MAX_CONTROL_FRAME};
use crate::{Error, Result};

/// Authoritative decision returned by the host for an incoming
/// transfer request. The receiver awaits this before replying
/// `Accept`/`Reject`.
#[derive(Debug, Clone)]
pub enum AcceptDecision {
    /// Accept the transfer. `dest` overrides the receive folder for
    /// this transfer only; `None` falls back to the configured
    /// destination. The host is responsible for choosing a trusted,
    /// local path — remote peers never influence it.
    Accept { dest: Option<PathBuf> },
    Reject(String),
}

/// Authoritative decision for an incoming pairing request.
#[derive(Debug, Clone)]
pub enum PairDecision {
    Accept,
    Reject(String),
}

/// Hooks the host (Tauri shell) provides to drive UI for incoming
/// requests. Trait object kept dyn-compatible.
pub trait ReceiverHost: Send + Sync + 'static {
    /// Called when a peer wants to send. Implementations may consult
    /// the trust store + auto-accept setting and return immediately,
    /// or block on a UI prompt.
    fn on_transfer_request<'a>(
        &'a self,
        peer: &'a PeerHandshake,
        manifest: &'a Manifest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AcceptDecision> + Send + 'a>>;

    /// Called when a peer wants to pair. The implementation should
    /// surface the SAS code to the user and ask for confirmation.
    fn on_pair_request<'a>(
        &'a self,
        peer: &'a PeerHandshake,
        sas: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PairDecision> + Send + 'a>>;

    /// Per-file progress callback: `(transfer_id, bytes_done, current_file_index, file_name)`.
    fn on_progress(
        &self,
        transfer_id: uuid::Uuid,
        peer: &PeerHandshake,
        bytes_done: u64,
        total_bytes: u64,
        current_file: u32,
        rel_path: &str,
    );

    /// Called once a transfer ends (success or failure).
    fn on_transfer_end(
        &self,
        transfer_id: uuid::Uuid,
        peer: &PeerHandshake,
        status: TransferStatus,
        files_written: Vec<PathBuf>,
    );
}

#[derive(Clone)]
pub struct ReceiverConfig {
    pub device_name: String,
    pub os: OsKind,
    pub device_type: DeviceType,
    pub trust: TrustStore,
    pub settings: Arc<std::sync::RwLock<Settings>>,
    pub default_dest: PathBuf,
}

impl std::fmt::Debug for ReceiverConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceiverConfig")
            .field("device_name", &self.device_name)
            .finish_non_exhaustive()
    }
}

/// Spawn the listener task. Returns the bound port + a handle that,
/// when dropped, stops the listener and aborts in-flight transfers.
pub async fn start(
    cfg: ReceiverConfig,
    identity: Arc<DeviceIdentity>,
    host: Arc<dyn ReceiverHost>,
) -> Result<(u16, ReceiverHandle)> {
    let listener = TcpListener::bind("0.0.0.0:0")
        .await
        .map_err(|e| Error::Transport(format!("listener bind: {e}")))?;
    let port = listener.local_addr()?.port();

    let (cert, key) = transport::generate_self_signed()?;
    let server_cfg = transport::server_config(cert, key)?;
    let acceptor = transport::acceptor(server_cfg);

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_l = cancel.clone();

    let task = tokio::spawn(async move {
        loop {
            if cancel_l.load(Ordering::Relaxed) {
                break;
            }
            match listener.accept().await {
                Ok((tcp, peer_addr)) => {
                    tcp.set_nodelay(true).ok();
                    let acceptor = acceptor.clone();
                    let cfg = cfg.clone();
                    let identity = identity.clone();
                    let host = host.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            handle_connection(tcp, acceptor, cfg, identity, host).await
                        {
                            tracing::warn!(error = %e, %peer_addr, "incoming connection failed");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    });

    Ok((port, ReceiverHandle { cancel, task }))
}

pub struct ReceiverHandle {
    cancel: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for ReceiverHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceiverHandle").finish_non_exhaustive()
    }
}

impl ReceiverHandle {
    pub fn shutdown(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.task.abort();
    }
}

impl Drop for ReceiverHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn handle_connection(
    tcp: TcpStream,
    acceptor: TlsAcceptor,
    cfg: ReceiverConfig,
    identity: Arc<DeviceIdentity>,
    host: Arc<dyn ReceiverHost>,
) -> Result<()> {
    let mut tls: TlsStream<TcpStream> = acceptor
        .accept(tcp)
        .await
        .map_err(|e| Error::Transport(format!("tls accept: {e}")))?;
    let peer = handshake::perform(
        &mut tls,
        &identity,
        cfg.device_name.clone(),
        cfg.os,
        cfg.device_type,
    )
    .await?;

    let request: Request = transport::read_msg(&mut tls).await?;
    match request {
        Request::Send {
            transfer_id,
            manifest,
        } => {
            handle_send(&mut tls, &peer, transfer_id, manifest, &cfg, host).await
        }
        Request::Pair { sas_nonce } => {
            handle_pair(&mut tls, &peer, sas_nonce, &cfg, &identity, host).await
        }
    }
}

async fn handle_pair<S>(
    tls: &mut S,
    peer: &PeerHandshake,
    sas_nonce: [u8; 16],
    cfg: &ReceiverConfig,
    identity: &DeviceIdentity,
    host: Arc<dyn ReceiverHost>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Compute the SAS the user has to compare against.
    let sas = crate::pairing::compute_sas(
        &identity.verifying_key_bytes(),
        &peer.hello.verifying_key,
        &sas_nonce,
    );
    match host.on_pair_request(peer, &sas).await {
        PairDecision::Accept => {
            // Persist trust.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let trusted = crate::pairing::TrustedPeer {
                id: peer.hello.id,
                name: peer.hello.name.clone(),
                os: peer.hello.os,
                device_type: peer.hello.device_type,
                fingerprint: peer.hello.fingerprint,
                verifying_key: peer.hello.verifying_key,
                paired_at_ms: now,
                last_seen_ms: now,
                dest_override: None,
                role: crate::pairing::DeviceRole::default(),
                auto_accept: false,
                auto_save: false,
            };
            cfg.trust.upsert(&trusted)?;
            transport::write_msg(tls, &Response::PairingAccepted).await?;
        }
        PairDecision::Reject(reason) => {
            transport::write_msg(tls, &Response::Reject { reason }).await?;
        }
    }
    Ok(())
}

async fn handle_send<S>(
    tls: &mut S,
    peer: &PeerHandshake,
    transfer_id: uuid::Uuid,
    manifest: Manifest,
    cfg: &ReceiverConfig,
    host: Arc<dyn ReceiverHost>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if manifest.transfer_id != transfer_id {
        return Err(Error::Protocol("transfer_id mismatch".into()));
    }
    if manifest.items.is_empty() {
        return Err(Error::Protocol("empty manifest".into()));
    }
    if manifest.items.len() > 200_000 {
        return Err(Error::Protocol("manifest too large".into()));
    }

    // Snapshot the category-sort preference up front; the destination
    // root is decided only after the host (user) chooses one.
    let sort_by_category = cfg.settings.read().unwrap().sort_by_category;

    // Authorisation FIRST. The host decides whether to accept and, if
    // so, where the files should land. Doing this before any path math
    // means a rejected/timed-out request never touches the filesystem.
    let decision = host.on_transfer_request(peer, &manifest).await;
    let chosen_dest = match decision {
        AcceptDecision::Accept { dest } => dest,
        AcceptDecision::Reject(r) => {
            transport::write_msg(tls, &Response::Reject { reason: r.clone() }).await?;
            return Err(Error::PeerRejected(r));
        }
    };

    // Resolve the destination root: explicit host choice wins, then the
    // configured destination, then the built-in default.
    let dest_root = chosen_dest
        .or_else(|| cfg.settings.read().unwrap().destination.clone())
        .unwrap_or_else(|| cfg.default_dest.clone());

    // Validate every rel_path against the chosen root. A malicious
    // manifest can still only ever resolve *inside* dest_root.
    let mut sanitized: Vec<(PathBuf, PathBuf, std::ffi::OsString)> =
        Vec::with_capacity(manifest.items.len());
    for item in &manifest.items {
        let safe = files::sanitize_rel_path(&item.rel_path)?;
        let (dir, name) = files::resolve_dest(&dest_root, &safe, sort_by_category);
        sanitized.push((safe, dir, name));
    }

    // Touch trust last_seen.
    let _ = cfg.trust.touch(peer.hello.id);

    // Determine resume offsets — if `dir/name.qdpart` exists, we can
    // resume from its current size, otherwise start from 0.
    let mut start_offsets: Vec<u64> = Vec::with_capacity(manifest.items.len());
    let mut part_paths: Vec<PathBuf> = Vec::with_capacity(manifest.items.len());
    let mut final_paths: Vec<PathBuf> = Vec::with_capacity(manifest.items.len());
    for (i, item) in manifest.items.iter().enumerate() {
        let (_, dir, name) = &sanitized[i];
        std::fs::create_dir_all(dir)?;
        let final_path = files::unique_dest(dir, name);
        let part_path = part_path_for(&final_path);
        let off = match std::fs::metadata(&part_path) {
            Ok(m) if m.len() <= item.size => m.len(),
            _ => 0,
        };
        start_offsets.push(off);
        part_paths.push(part_path);
        final_paths.push(final_path);
    }

    transport::write_msg(
        tls,
        &Response::Accept {
            start_offsets: start_offsets.clone(),
        },
    )
    .await?;

    let mut bytes_done: u64 = start_offsets.iter().sum();
    let mut written_files: Vec<PathBuf> = Vec::new();

    for (i, item) in manifest.items.iter().enumerate() {
        let file_start: FileStart = transport::read_msg(tls).await?;
        if file_start.index as usize != i {
            return Err(Error::Protocol(format!(
                "FileStart out of order: got {} want {i}",
                file_start.index
            )));
        }
        if file_start.start_offset != start_offsets[i] {
            return Err(Error::Protocol(format!(
                "FileStart offset mismatch: got {} want {}",
                file_start.start_offset, start_offsets[i]
            )));
        }
        let part_path = &part_paths[i];
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .open(part_path)
            .await?;
        if file_start.start_offset > 0 {
            file.seek(SeekFrom::Start(file_start.start_offset)).await?;
        } else {
            file.set_len(0).await?;
        }
        let mut hasher = blake3::Hasher::new();
        let target_size = item.size;
        loop {
            let len = transport::read_len(tls).await?;
            if len == 0 {
                break;
            }
            if len as u64 > target_size {
                return Err(Error::Protocol("chunk larger than file".into()));
            }
            if len > MAX_CONTROL_FRAME {
                return Err(Error::Protocol("chunk too large".into()));
            }
            let mut buf = vec![0u8; len];
            tls.read_exact(&mut buf)
                .await
                .map_err(|e| Error::Transport(format!("read chunk: {e}")))?;
            hasher.update(&buf);
            file.write_all(&buf).await?;
            bytes_done += len as u64;
            host.on_progress(
                transfer_id,
                peer,
                bytes_done,
                manifest.total_bytes,
                i as u32,
                &item.rel_path,
            );
        }
        file.flush().await?;
        let cur_pos = file.stream_position().await?;
        if cur_pos != target_size {
            let _ = tokio::fs::remove_file(part_path).await;
            return Err(Error::Integrity(format!(
                "{}: wrote {cur_pos} bytes, manifest declared {target_size}",
                item.rel_path
            )));
        }

        let file_end: FileEnd = transport::read_msg(tls).await?;
        if file_end.index as usize != i {
            return Err(Error::Protocol("FileEnd index mismatch".into()));
        }
        let stream_actual = hasher.finalize().to_hex().to_string();
        if stream_actual != file_end.stream_blake3_hex {
            // The streamed bytes are corrupt, so the partial file can no
            // longer be trusted as a resume base. Discard it rather than
            // resuming onto a bad prefix on the next attempt.
            let _ = tokio::fs::remove_file(part_path).await;
            return Err(Error::Integrity(format!(
                "{}: stream hash mismatch",
                item.rel_path
            )));
        }

        // Verify the *whole* file against the manifest hash. For a fresh
        // transfer (offset 0) the stream hash already covers the entire
        // file. For a resume we must additionally hash the pre-existing
        // prefix, otherwise a corrupt `.qdpart` left by an earlier run
        // would pass undetected (silent corruption).
        let full_actual = if file_start.start_offset == 0 {
            stream_actual
        } else {
            hash_file_blake3(part_path).await?
        };
        if full_actual != item.blake3_hex {
            let _ = tokio::fs::remove_file(part_path).await;
            return Err(Error::Integrity(format!(
                "{}: full hash mismatch",
                item.rel_path
            )));
        }

        // Drop the handle BEFORE renaming on Windows.
        drop(file);
        files::finalize_part(part_path, &final_paths[i])?;
        written_files.push(final_paths[i].clone());
    }

    // Final TransferEnd from sender.
    let end: TransferEnd = transport::read_msg(tls).await?;
    if end.transfer_id != transfer_id {
        return Err(Error::Protocol("TransferEnd id mismatch".into()));
    }
    let overall_status = end.status;
    if overall_status != TransferStatus::Completed {
        // Clean up any successfully-written files? Sender said
        // cancelled/failed mid-stream — we trust their status.
        tracing::warn!(?overall_status, "transfer ended non-OK");
    }
    host.on_transfer_end(transfer_id, peer, overall_status, written_files);
    Ok(())
}

fn part_path_for(final_path: &std::path::Path) -> PathBuf {
    let mut s = final_path.as_os_str().to_owned();
    s.push(".qdpart");
    PathBuf::from(s)
}

/// Compute the BLAKE3 hash of a whole file on disk, hex-encoded.
/// Used to validate a resumed transfer end-to-end, including the
/// prefix that was already on disk before this run started.
async fn hash_file_blake3(path: &std::path::Path) -> Result<String> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}
