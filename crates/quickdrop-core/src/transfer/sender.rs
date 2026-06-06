//! Sender side of the transfer engine.
//!
//! Establishes a TLS connection, performs the application handshake,
//! transmits the manifest, then streams every file 1 MiB at a time
//! using length-prefixed framing. Honors per-file resume offsets
//! returned by the receiver.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use uuid::Uuid;

use crate::discovery::{DeviceType, OsKind};
use crate::identity::DeviceIdentity;
use crate::pairing::TrustStore;
use crate::transfer::handshake::{self, PeerHandshake};
use crate::transfer::hash;
use crate::transfer::protocol::{
    FileEnd, FileStart, Manifest, ManifestItem, Request, Response, TransferEnd, TransferStatus,
};
use crate::transport::{self, MAX_CONTROL_FRAME};
use crate::{Error, Result};

/// Single source path to be sent. Either a file or a directory.
#[derive(Debug, Clone)]
pub struct SendItem {
    pub path: PathBuf,
}

/// Progress callback signature: `(bytes_sent_total, current_file_index)`.
pub type Progress = dyn Fn(u64, u32, &str) + Send + Sync;

/// Pick a streaming chunk size for a file based on its size. Bigger
/// files amortise framing/syscall overhead with bigger frames:
///
/// * `< 50 MiB`  → 512 KiB
/// * `< 1 GiB`   → 4 MiB
/// * `>= 1 GiB`  → 16 MiB
///
/// The result is always `<= MAX_CONTROL_FRAME`, so the receiver (which
/// rejects oversized frames) accepts every chunk. The wire format is
/// unchanged — chunks stay length-prefixed and resumable.
pub(crate) fn chunk_size_for(file_size: u64) -> usize {
    const MIB: u64 = 1024 * 1024;
    if file_size < 50 * MIB {
        512 * 1024
    } else if file_size < 1024 * MIB {
        4 * MIB as usize
    } else {
        16 * MIB as usize
    }
}

#[derive(Debug, Clone)]
pub struct SenderConfig {
    pub device_name: String,
    pub os: OsKind,
    pub device_type: DeviceType,
    pub trust: TrustStore,
}

/// Build a [`Manifest`] from one or more local paths. Walks directories
/// recursively and computes BLAKE3 of every file before any bytes hit
/// the wire — guarantees end-to-end integrity but does mean very large
/// inputs spend time hashing locally first.
pub async fn build_manifest(items: &[SendItem]) -> Result<(Manifest, Vec<PathBuf>)> {
    let transfer_id = Uuid::new_v4();
    let mut entries: Vec<(String, PathBuf, u64)> = Vec::new();
    for item in items {
        if !item.path.exists() {
            return Err(Error::NotFound(item.path.display().to_string()));
        }
        if item.path.is_file() {
            let name = item
                .path
                .file_name()
                .ok_or_else(|| Error::Internal("file without name".into()))?
                .to_string_lossy()
                .to_string();
            let size = tokio::fs::metadata(&item.path).await?.len();
            entries.push((name, item.path.clone(), size));
        } else if item.path.is_dir() {
            let root_name = item
                .path
                .file_name()
                .ok_or_else(|| Error::Internal("dir without name".into()))?
                .to_string_lossy()
                .to_string();
            walk_dir(&item.path, &root_name, &mut entries).await?;
        }
    }
    if entries.is_empty() {
        return Err(Error::NotFound("no files to send".into()));
    }
    let mut manifest_items = Vec::with_capacity(entries.len());
    let mut total_bytes = 0u64;
    let mut local_paths = Vec::with_capacity(entries.len());
    for (rel, abs, size) in &entries {
        let h = hash::blake3_file(abs).await?;
        let modified_ms = tokio::fs::metadata(abs)
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64);
        manifest_items.push(ManifestItem {
            rel_path: rel.clone(),
            size: *size,
            blake3_hex: h,
            modified_ms,
        });
        total_bytes += size;
        local_paths.push(abs.clone());
    }
    Ok((
        Manifest {
            transfer_id,
            items: manifest_items,
            total_bytes,
        },
        local_paths,
    ))
}

fn walk_dir<'a>(
    root: &'a Path,
    rel_prefix: &'a str,
    out: &'a mut Vec<(String, PathBuf, u64)>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let mut rd = tokio::fs::read_dir(root).await?;
        while let Some(entry) = rd.next_entry().await? {
            let ft = entry.file_type().await?;
            let name = entry.file_name().to_string_lossy().to_string();
            let rel = format!("{rel_prefix}/{name}");
            let path = entry.path();
            if ft.is_dir() {
                walk_dir(&path, &rel, out).await?;
            } else if ft.is_file() {
                let size = entry.metadata().await?.len();
                out.push((rel, path, size));
            }
        }
        Ok(())
    })
}

/// Build the manifest, then connect and stream. Convenience wrapper
/// around [`send_prepared`] for callers that do not already have a
/// manifest (e.g. tests). Production send paths should build the
/// manifest once and call [`send_prepared`] directly to avoid hashing
/// every file twice.
pub async fn send_to(
    addr: std::net::SocketAddr,
    cfg: SenderConfig,
    identity: Arc<DeviceIdentity>,
    items: Vec<SendItem>,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
) -> Result<(PeerHandshake, Manifest)> {
    let (manifest, local_paths) = build_manifest(&items).await?;
    send_prepared(addr, cfg, identity, manifest, local_paths, progress, cancel).await
}

/// Connect to `addr`, run the protocol, send a pre-built `manifest`
/// and its `local_paths` (parallel to `manifest.items`). `progress`
/// is invoked from the same task; never call back into the sender
/// from inside it.
pub async fn send_prepared(
    addr: std::net::SocketAddr,
    cfg: SenderConfig,
    identity: Arc<DeviceIdentity>,
    manifest: Manifest,
    local_paths: Vec<PathBuf>,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
) -> Result<(PeerHandshake, Manifest)> {
    // 1. TCP + TLS + handshake
    let tcp = TcpStream::connect(addr)
        .await
        .map_err(|e| Error::Transport(format!("connect {addr}: {e}")))?;
    tcp.set_nodelay(true).ok();
    let connector = transport::connector(transport::client_config()?);
    let mut tls: TlsStream<TcpStream> = connector
        .connect(transport::sni(), tcp)
        .await
        .map_err(|e| Error::Transport(format!("tls connect: {e}")))?;
    let peer = handshake::perform(
        &mut tls,
        &identity,
        cfg.device_name.clone(),
        cfg.os,
        cfg.device_type,
    )
    .await?;

    // 2. Send Request::Send using the caller-supplied manifest.
    let request = Request::Send {
        transfer_id: manifest.transfer_id,
        manifest: manifest.clone(),
    };
    transport::write_msg(&mut tls, &request).await?;
    let resp: Response = transport::read_msg(&mut tls).await?;
    let start_offsets = match resp {
        Response::Accept { start_offsets } => start_offsets,
        Response::Reject { reason } => return Err(Error::PeerRejected(reason)),
        Response::PairingAccepted => {
            return Err(Error::Protocol("unexpected PairingAccepted".into()));
        }
    };
    if start_offsets.len() != manifest.items.len() {
        return Err(Error::Protocol("offsets length != items length".into()));
    }

    // 3. Stream every file.
    let mut total_sent: u64 = 0;
    let started = Instant::now();
    for (idx, (item, local)) in manifest.items.iter().zip(local_paths.iter()).enumerate() {
        if cancel.load(Ordering::Relaxed) {
            let _ = transport::write_msg(
                &mut tls,
                &TransferEnd {
                    transfer_id: manifest.transfer_id,
                    status: TransferStatus::Cancelled,
                },
            )
            .await;
            tls.shutdown().await.ok();
            return Err(Error::Cancelled);
        }

        let start_off = start_offsets[idx];
        if start_off > item.size {
            return Err(Error::Protocol(format!(
                "peer requested offset {start_off} > size {} for {}",
                item.size, item.rel_path
            )));
        }
        transport::write_msg(
            &mut tls,
            &FileStart {
                index: idx as u32,
                start_offset: start_off,
            },
        )
        .await?;

        let mut f = File::open(local).await?;
        if start_off > 0 {
            f.seek(SeekFrom::Start(start_off)).await?;
        }
        let mut hasher = blake3::Hasher::new();
        // Adaptive chunk size: larger files use larger frames to cut
        // per-chunk framing + syscall overhead on the hot path. Capped
        // at the receiver's MAX_CONTROL_FRAME so the peer never rejects.
        let chunk = chunk_size_for(item.size).min(MAX_CONTROL_FRAME);
        let mut buf = vec![0u8; chunk];
        let mut remaining = item.size - start_off;
        while remaining > 0 {
            if cancel.load(Ordering::Relaxed) {
                let _ = transport::write_msg(
                    &mut tls,
                    &TransferEnd {
                        transfer_id: manifest.transfer_id,
                        status: TransferStatus::Cancelled,
                    },
                )
                .await;
                tls.shutdown().await.ok();
                return Err(Error::Cancelled);
            }
            let want = (remaining as usize).min(buf.len());
            let n = f.read(&mut buf[..want]).await?;
            if n == 0 {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("local file shorter than manifest: {}", item.rel_path),
                )));
            }
            hasher.update(&buf[..n]);
            transport::write_len(&mut tls, n as u32).await?;
            tls.write_all(&buf[..n])
                .await
                .map_err(|e| Error::Transport(format!("write chunk: {e}")))?;
            remaining -= n as u64;
            total_sent += n as u64;
            (progress)(total_sent, idx as u32, &item.rel_path);
        }
        // 0-length terminator chunk — explicit end-of-file marker.
        transport::write_len(&mut tls, 0).await?;

        transport::write_msg(
            &mut tls,
            &FileEnd {
                index: idx as u32,
                stream_blake3_hex: hasher.finalize().to_hex().to_string(),
            },
        )
        .await?;
    }

    transport::write_msg(
        &mut tls,
        &TransferEnd {
            transfer_id: manifest.transfer_id,
            status: TransferStatus::Completed,
        },
    )
    .await?;
    tls.flush().await.ok();
    tracing::info!(
        transfer = %manifest.transfer_id,
        bytes = total_sent,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "send complete"
    );
    Ok((peer, manifest))
}

/// Initiator side of a pairing request. Connects, handshakes, sends
/// `Request::Pair`, displays the SAS to the *caller*, and waits for
/// the receiver's `PairingAccepted`. On success, persists the peer in
/// `cfg.trust`.
pub async fn pair_with(
    addr: std::net::SocketAddr,
    cfg: SenderConfig,
    identity: Arc<DeviceIdentity>,
    on_sas: Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<PeerHandshake> {
    let tcp = TcpStream::connect(addr)
        .await
        .map_err(|e| Error::Transport(format!("connect {addr}: {e}")))?;
    tcp.set_nodelay(true).ok();
    let connector = transport::connector(transport::client_config()?);
    let mut tls = connector
        .connect(transport::sni(), tcp)
        .await
        .map_err(|e| Error::Transport(format!("tls connect: {e}")))?;
    let peer = handshake::perform(
        &mut tls,
        &identity,
        cfg.device_name.clone(),
        cfg.os,
        cfg.device_type,
    )
    .await?;

    let mut sas_nonce = [0u8; 16];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut sas_nonce);
    let sas = crate::pairing::compute_sas(
        &identity.verifying_key_bytes(),
        &peer.hello.verifying_key,
        &sas_nonce,
    );
    on_sas(&sas);

    transport::write_msg(&mut tls, &Request::Pair { sas_nonce }).await?;
    let resp: Response = transport::read_msg(&mut tls).await?;
    match resp {
        Response::PairingAccepted => {
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
            Ok(peer)
        }
        Response::Reject { reason } => Err(Error::PeerRejected(reason)),
        Response::Accept { .. } => Err(Error::Protocol("unexpected Accept on pair".into())),
    }
}
