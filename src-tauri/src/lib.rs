//! QuickDrop Tauri shell.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use quickdrop_core::config::{Paths, Settings};
use quickdrop_core::db::Db;
use quickdrop_core::discovery::{DeviceType, DiscoveryConfig, DiscoveryService, OsKind, Peer};
use quickdrop_core::history::{HistoryStore, TransferRecord};
use quickdrop_core::identity::{DeviceIdentity, Fingerprint};
use quickdrop_core::logging::{self, LogGuard};
use quickdrop_core::pairing::{DeviceRole, TrustStore, TrustedPeer};
use quickdrop_core::spaces::{MemberRole, Space, SpaceStore, SpaceType};
use quickdrop_core::transfer::manager::TransferManager;
use quickdrop_core::transfer::protocol::{Manifest, TransferStatus};
use quickdrop_core::transfer::receiver::{
    AcceptDecision, PairDecision, ReceiverConfig, ReceiverHandle, ReceiverHost,
};
use quickdrop_core::transfer::sender::{self, SendItem, SenderConfig};
use quickdrop_core::transfer::{handshake::PeerHandshake, Direction, TransferProgress, TransferState};
use quickdrop_share::session::{ShareOptions, ShareSession};
use quickdrop_share::{ShareConfig, ShareService, ShareTicket};
use serde::Serialize;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager,
};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_opener::OpenerExt;
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use uuid::Uuid;

mod context_menu;

/// Application state shared across Tauri commands.
pub struct AppState {
    pub paths: Paths,
    pub settings: Arc<RwLock<Settings>>,
    pub identity: Arc<DeviceIdentity>,
    pub trust: TrustStore,
    pub manager: TransferManager,
    /// Durable transfer history (survives restarts).
    pub history: HistoryStore,
    /// QuickDrop Spaces foundation store.
    pub spaces: SpaceStore,
    /// Locked discovery handle so we can rebind on settings change.
    pub discovery: AsyncMutex<Option<DiscoveryService>>,
    pub receiver: AsyncMutex<Option<ReceiverHandle>>,
    /// Port the TLS receiver is currently listening on. `0` until the
    /// receiver has bound during bootstrap. Exposed via `app_info` for
    /// diagnostics.
    pub listen_port: std::sync::atomic::AtomicU16,
    /// QuickDrop Share: lazily-started embedded HTTP server for
    /// browser-based receiving. `None` until the first share is created.
    pub share: AsyncMutex<Option<ShareService>>,
    /// Live cache of last-known peers so commands can resolve a peer
    /// id → addresses without an `await` on the watcher.
    pub peers_cache: Arc<RwLock<Vec<Peer>>>,
    /// Pending UI prompts (incoming pair / transfer requests). Map from
    /// prompt id → oneshot reply channel.
    pub pending_prompts: Arc<std::sync::Mutex<std::collections::HashMap<Uuid, PromptReply>>>,
    pub _db: Db,
    pub _log_guard: LogGuard,
}

#[derive(Debug)]
pub enum PromptReply {
    Pair(oneshot::Sender<PairDecision>),
    /// Transfer prompt: carries the sending peer's id so an accepted
    /// "remember this folder" choice can be persisted against it.
    Transfer(Uuid, oneshot::Sender<AcceptDecision>),
}

#[derive(Debug, Serialize)]
pub struct AppInfo {
    pub version: &'static str,
    pub device_name: String,
    pub device_id: String,
    pub fingerprint: String,
    pub destination: String,
    pub app_data: String,
    pub listen_port: u16,
}

#[tauri::command]
fn app_info(state: tauri::State<'_, Arc<AppState>>) -> AppInfo {
    let s = state.settings.read().unwrap();
    let dest = s
        .destination
        .clone()
        .unwrap_or_else(|| state.paths.default_dest.clone());
    AppInfo {
        version: quickdrop_core::VERSION,
        device_name: s.device_name.clone(),
        device_id: state.identity.id().to_string(),
        fingerprint: state.identity.fingerprint().to_string(),
        destination: dest.to_string_lossy().into_owned(),
        app_data: state.paths.app_data.to_string_lossy().into_owned(),
        listen_port: state
            .listen_port
            .load(std::sync::atomic::Ordering::Relaxed),
    }
}

#[tauri::command]
fn list_peers(state: tauri::State<'_, Arc<AppState>>) -> Vec<Peer> {
    state.peers_cache.read().unwrap().clone()
}

#[derive(Debug, Serialize)]
struct TrustedPeerView {
    id: String,
    name: String,
    fingerprint: String,
    paired_at_ms: u64,
    last_seen_ms: u64,
    role: DeviceRole,
    auto_accept: bool,
    auto_save: bool,
    dest_override: Option<String>,
}

impl From<TrustedPeer> for TrustedPeerView {
    fn from(p: TrustedPeer) -> Self {
        Self {
            id: p.id.to_string(),
            name: p.name,
            fingerprint: p.fingerprint.to_string(),
            paired_at_ms: p.paired_at_ms,
            last_seen_ms: p.last_seen_ms,
            role: p.role,
            auto_accept: p.auto_accept,
            auto_save: p.auto_save,
            dest_override: p.dest_override.map(|d| d.to_string_lossy().into_owned()),
        }
    }
}

#[tauri::command]
fn list_trusted_peers(
    state: tauri::State<'_, Arc<AppState>>,
) -> std::result::Result<Vec<TrustedPeerView>, String> {
    state
        .trust
        .all()
        .map(|peers| peers.into_iter().map(Into::into).collect())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn forget_peer(
    state: tauri::State<'_, Arc<AppState>>,
    peer_id: String,
) -> std::result::Result<bool, String> {
    let id = Uuid::parse_str(&peer_id).map_err(|e| format!("invalid peer id: {e}"))?;
    state.trust.remove(id).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_transfers(state: tauri::State<'_, Arc<AppState>>) -> Vec<TransferProgress> {
    state.manager.snapshot()
}

#[tauri::command]
fn cancel_transfer(state: tauri::State<'_, Arc<AppState>>, transfer_id: String) -> bool {
    if let Ok(id) = Uuid::parse_str(&transfer_id) {
        state.manager.cancel(id)
    } else {
        false
    }
}

/// Derive a friendly history display name from the first file's name and
/// the total item count: the single file, or `"N files"` for a batch.
fn history_file_name(first: Option<&str>, total: u32) -> String {
    match (first, total) {
        (Some(name), 1) => name.to_string(),
        (Some(name), n) if n > 1 => format!("{name} + {} more", n - 1),
        (_, n) => format!("{n} files"),
    }
}

// ---------------------------------------------------------------------
// Instant Transfer Mode + Device Roles (per-device trust settings)
// ---------------------------------------------------------------------

#[tauri::command]
fn set_device_role(
    state: tauri::State<'_, Arc<AppState>>,
    peer_id: String,
    role: DeviceRole,
) -> std::result::Result<bool, String> {
    let id = Uuid::parse_str(&peer_id).map_err(|e| format!("invalid peer id: {e}"))?;
    state.trust.set_role(id, role).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_device_prefs(
    state: tauri::State<'_, Arc<AppState>>,
    peer_id: String,
    auto_accept: bool,
    auto_save: bool,
) -> std::result::Result<bool, String> {
    let id = Uuid::parse_str(&peer_id).map_err(|e| format!("invalid peer id: {e}"))?;
    state
        .trust
        .set_instant_prefs(id, auto_accept, auto_save)
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------
// Transfer history
// ---------------------------------------------------------------------

#[tauri::command]
fn list_history(
    state: tauri::State<'_, Arc<AppState>>,
) -> std::result::Result<Vec<TransferRecord>, String> {
    state.history.list().map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_history_entry(
    state: tauri::State<'_, Arc<AppState>>,
    entry_id: String,
) -> std::result::Result<bool, String> {
    let id = Uuid::parse_str(&entry_id).map_err(|e| format!("invalid id: {e}"))?;
    state.history.delete(id).map_err(|e| e.to_string())
}

#[tauri::command]
fn clear_history(state: tauri::State<'_, Arc<AppState>>) -> std::result::Result<(), String> {
    state.history.clear().map_err(|e| e.to_string())
}

/// Open a file with the OS default handler.
#[tauri::command]
fn open_path(app: AppHandle, path: String) -> std::result::Result<(), String> {
    app.opener()
        .open_path(path, None::<&str>)
        .map_err(|e| e.to_string())
}

/// Reveal a file in its containing folder (Explorer/Finder).
#[tauri::command]
fn reveal_path(app: AppHandle, path: String) -> std::result::Result<(), String> {
    app.opener()
        .reveal_item_in_dir(PathBuf::from(path))
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------
// QuickDrop Spaces (foundation)
// ---------------------------------------------------------------------

#[tauri::command]
fn list_spaces(state: tauri::State<'_, Arc<AppState>>) -> std::result::Result<Vec<Space>, String> {
    state.spaces.list().map_err(|e| e.to_string())
}

#[tauri::command]
fn create_space(
    state: tauri::State<'_, Arc<AppState>>,
    name: String,
    space_type: SpaceType,
) -> std::result::Result<Space, String> {
    let owner = state.identity.id();
    let owner_name = state.settings.read().unwrap().device_name.clone();
    state
        .spaces
        .create(name, space_type, owner, owner_name)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_space(
    state: tauri::State<'_, Arc<AppState>>,
    space_id: String,
) -> std::result::Result<bool, String> {
    let id = Uuid::parse_str(&space_id).map_err(|e| format!("invalid id: {e}"))?;
    state.spaces.delete(id).map_err(|e| e.to_string())
}

#[tauri::command]
fn add_space_member(
    state: tauri::State<'_, Arc<AppState>>,
    space_id: String,
    peer_id: String,
    name: String,
    role: MemberRole,
) -> std::result::Result<Space, String> {
    let sid = Uuid::parse_str(&space_id).map_err(|e| format!("invalid space id: {e}"))?;
    let pid = Uuid::parse_str(&peer_id).map_err(|e| format!("invalid peer id: {e}"))?;
    state
        .spaces
        .add_member(sid, pid, name, role)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_space_member(
    state: tauri::State<'_, Arc<AppState>>,
    space_id: String,
    peer_id: String,
) -> std::result::Result<Space, String> {
    let sid = Uuid::parse_str(&space_id).map_err(|e| format!("invalid space id: {e}"))?;
    let pid = Uuid::parse_str(&peer_id).map_err(|e| format!("invalid peer id: {e}"))?;
    state
        .spaces
        .remove_member(sid, pid)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn add_space_folder(
    state: tauri::State<'_, Arc<AppState>>,
    space_id: String,
    name: String,
    path: String,
) -> std::result::Result<Space, String> {
    let sid = Uuid::parse_str(&space_id).map_err(|e| format!("invalid space id: {e}"))?;
    let owner = state.identity.id();
    state
        .spaces
        .add_folder(sid, name, PathBuf::from(path), owner)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn space_activity(
    state: tauri::State<'_, Arc<AppState>>,
    space_id: String,
) -> std::result::Result<Vec<quickdrop_core::spaces::Activity>, String> {
    let id = Uuid::parse_str(&space_id).map_err(|e| format!("invalid id: {e}"))?;
    state.spaces.activity(id).map_err(|e| e.to_string())
}


#[tauri::command]
async fn send_files(
    app: AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    peer_id: String,
    paths: Vec<String>,
) -> std::result::Result<String, String> {
    let id = Uuid::parse_str(&peer_id).map_err(|e| format!("invalid peer id: {e}"))?;
    let peer = state
        .peers_cache
        .read()
        .unwrap()
        .iter()
        .find(|p| p.id == id)
        .cloned()
        .ok_or_else(|| "peer not found".to_string())?;
    let addr = peer
        .addrs
        .first()
        .copied()
        .ok_or_else(|| "peer has no address".to_string())?;
    let device_name = state.settings.read().unwrap().device_name.clone();
    // Captured for the transfer-history record written on completion.
    let my_name = device_name.clone();
    let hist_peer_name = peer.name.clone();
    let hist_peer_id = peer.id;
    let source_paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    let cfg = SenderConfig {
        device_name,
        os: OsKind::current(),
        device_type: DeviceType::Desktop,
        trust: state.trust.clone(),
    };
    let identity = state.identity.clone();
    let manager = state.manager.clone();
    let items: Vec<SendItem> = paths
        .into_iter()
        .map(|p| SendItem {
            path: PathBuf::from(p),
        })
        .collect();
    if items.is_empty() {
        return Err("no files selected".into());
    }

    // Build the manifest once: it drives both the UI totals and the
    // actual wire protocol, so the transfer_id stays consistent and we
    // hash every file only once.
    let (manifest, local_paths) = sender::build_manifest(&items)
        .await
        .map_err(|e| format!("build manifest: {e}"))?;
    let total_items = manifest.items.len() as u32;
    let total_bytes = manifest.total_bytes;
    let transfer_id = manifest.transfer_id;
    let hist_file_name = history_file_name(
        manifest
            .items
            .first()
            .map(|i| i.rel_path.rsplit('/').next().unwrap_or(&i.rel_path)),
        total_items,
    );
    let cancel = manager.register(
        transfer_id,
        Direction::Send,
        peer.id,
        peer.name.clone(),
        total_items,
        total_bytes,
    );
    let manager_progress = manager.clone();
    let progress: Arc<sender::Progress> = Arc::new(move |bytes, idx, name: &str| {
        manager_progress.update_progress(transfer_id, bytes, idx, name);
    });

    let app_for_task = app.clone();
    let manager_done = manager.clone();
    let history = state.history.clone();
    tokio::spawn(async move {
        let res = sender::send_prepared(
            addr,
            cfg,
            identity,
            manifest,
            local_paths,
            progress,
            cancel.clone(),
        )
        .await;
        let record_status;
        match res {
            Ok(_) => {
                record_status = TransferState::Completed;
                manager_done.finish(transfer_id, TransferState::Completed);
                let _ = app_for_task.emit("transfers://updated", manager_done.snapshot());
            }
            Err(e) => {
                tracing::warn!(error = %e, "send failed");
                let st = if matches!(e, quickdrop_core::Error::Cancelled) {
                    TransferState::Cancelled
                } else {
                    TransferState::Failed
                };
                record_status = st;
                manager_done.finish(transfer_id, st);
                let _ = app_for_task.emit("transfers://updated", manager_done.snapshot());
                let _ = app_for_task.emit("transfers://error", e.to_string());
            }
        }
        // Persist a durable history record for this send.
        let rec = TransferRecord {
            id: transfer_id,
            file_name: hist_file_name,
            direction: Direction::Send,
            peer_id: hist_peer_id,
            source_device: my_name,
            target_device: hist_peer_name,
            timestamp_ms: quickdrop_core::history::now_ms(),
            size: total_bytes,
            status: record_status,
            paths: source_paths,
        };
        if let Err(e) = history.record(&rec) {
            tracing::warn!(error = %e, "failed to record send history");
        }
        let _ = app_for_task.emit("history://updated", ());
    });

    let _ = app.emit("transfers://updated", manager.snapshot());
    Ok(transfer_id.to_string())
}

#[tauri::command]
async fn pair_with(
    app: AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    peer_id: String,
) -> std::result::Result<(), String> {
    let id = Uuid::parse_str(&peer_id).map_err(|e| format!("invalid peer id: {e}"))?;
    let peer = state
        .peers_cache
        .read()
        .unwrap()
        .iter()
        .find(|p| p.id == id)
        .cloned()
        .ok_or_else(|| "peer not found".to_string())?;
    let addr = peer
        .addrs
        .first()
        .copied()
        .ok_or_else(|| "peer has no address".to_string())?;
    let device_name = state.settings.read().unwrap().device_name.clone();
    let cfg = SenderConfig {
        device_name,
        os: OsKind::current(),
        device_type: DeviceType::Desktop,
        trust: state.trust.clone(),
    };
    let identity = state.identity.clone();
    let app_sas = app.clone();
    let on_sas: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |sas: &str| {
        let _ = app_sas.emit(
            "pairing://sas",
            serde_json::json!({ "peer_id": id.to_string(), "sas": sas }),
        );
    });
    sender::pair_with(addr, cfg, identity, on_sas)
        .await
        .map_err(|e| e.to_string())?;
    let _ = app.emit("pairing://done", id.to_string());
    Ok(())
}

#[tauri::command]
fn answer_prompt(
    state: tauri::State<'_, Arc<AppState>>,
    prompt_id: String,
    accept: bool,
    reason: Option<String>,
    dest: Option<String>,
    remember: Option<bool>,
) -> std::result::Result<(), String> {
    let id = Uuid::parse_str(&prompt_id).map_err(|e| format!("invalid prompt id: {e}"))?;
    let entry = state
        .pending_prompts
        .lock()
        .unwrap()
        .remove(&id)
        .ok_or_else(|| "prompt not found".to_string())?;
    match entry {
        PromptReply::Pair(tx) => {
            let _ = tx.send(if accept {
                PairDecision::Accept
            } else {
                PairDecision::Reject(reason.unwrap_or_else(|| "user rejected".into()))
            });
        }
        PromptReply::Transfer(peer_id, tx) => {
            let dest_path = dest.filter(|d| !d.is_empty()).map(PathBuf::from);
            if accept && remember.unwrap_or(false) {
                // Persist the destination for this device (only takes
                // effect if the peer is already trusted).
                if let Err(e) = state.trust.set_dest(peer_id, dest_path.clone()) {
                    tracing::warn!(error = %e, "failed to remember destination");
                }
            }
            let _ = tx.send(if accept {
                AcceptDecision::Accept { dest: dest_path }
            } else {
                AcceptDecision::Reject(reason.unwrap_or_else(|| "user rejected".into()))
            });
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct PromptPayload {
    prompt_id: String,
    kind: &'static str,
    peer_id: String,
    peer_name: String,
    fingerprint: String,
    /// SAS code for pair prompts.
    sas: Option<String>,
    /// Manifest summary for transfer prompts.
    items: Option<u32>,
    total_bytes: Option<u64>,
    /// First file name (or only file) for transfer prompts.
    name: Option<String>,
    /// Remembered destination for this device, if any.
    remembered_dest: Option<String>,
    /// Suggested receive folders for the destination picker.
    downloads: Option<String>,
    desktop: Option<String>,
    documents: Option<String>,
    trusted: bool,
}

/// Adapter that bridges `ReceiverHost` callbacks into Tauri events.
struct TauriHost {
    app: AppHandle,
    state: Arc<AppState>,
}

impl ReceiverHost for TauriHost {
    fn on_transfer_request<'a>(
        &'a self,
        peer: &'a PeerHandshake,
        manifest: &'a Manifest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AcceptDecision> + Send + 'a>> {
        let app = self.app.clone();
        let state = self.state.clone();
        Box::pin(async move {
            let trusted = state
                .trust
                .is_trusted(&peer.hello.fingerprint)
                .unwrap_or(false);
            // Load the full trust record so we can honour per-device
            // Instant Transfer Mode (auto-accept / auto-save) and any
            // remembered destination folder.
            let record = state
                .trust
                .get_by_fingerprint(&peer.hello.fingerprint)
                .ok()
                .flatten();
            let remembered = record.as_ref().and_then(|p| p.dest_override.clone());
            let dev_auto_accept = record.as_ref().map(|p| p.auto_accept).unwrap_or(false);
            let dev_auto_save = record.as_ref().map(|p| p.auto_save).unwrap_or(false);
            let global_auto_accept = state.settings.read().unwrap().auto_accept_trusted;

            // Instant Transfer Mode: a trusted peer with per-device
            // auto-accept (or the global auto-accept fallback) skips the
            // approval prompt entirely. When auto-save is on we route to
            // the peer's remembered folder; otherwise the receiver falls
            // back to the configured default (dest = None). Untrusted
            // peers can never reach this path — the security model is
            // unchanged.
            if trusted && (dev_auto_accept || global_auto_accept) {
                let dest = if dev_auto_save { remembered } else { None };
                return AcceptDecision::Accept { dest };
            }

            let prompt_id = Uuid::new_v4();
            let (tx, rx) = oneshot::channel();
            state
                .pending_prompts
                .lock()
                .unwrap()
                .insert(prompt_id, PromptReply::Transfer(peer.hello.id, tx));
            let dests = quickdrop_core::config::dest_options();
            let first_name = manifest
                .items
                .first()
                .map(|i| i.rel_path.rsplit('/').next().unwrap_or(&i.rel_path).to_string());
            let _ = app.emit(
                "prompt://incoming",
                PromptPayload {
                    prompt_id: prompt_id.to_string(),
                    kind: "transfer",
                    peer_id: peer.hello.id.to_string(),
                    peer_name: peer.hello.name.clone(),
                    fingerprint: peer.hello.fingerprint.to_string(),
                    sas: None,
                    items: Some(manifest.items.len() as u32),
                    total_bytes: Some(manifest.total_bytes),
                    name: first_name,
                    remembered_dest: remembered.map(|p| p.to_string_lossy().into_owned()),
                    downloads: Some(dests.downloads.to_string_lossy().into_owned()),
                    desktop: Some(dests.desktop.to_string_lossy().into_owned()),
                    documents: Some(dests.documents.to_string_lossy().into_owned()),
                    trusted,
                },
            );
            // Show the window so the user can respond.
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.set_focus();
            }
            match tokio::time::timeout(std::time::Duration::from_secs(120), rx).await {
                Ok(Ok(d)) => d,
                _ => {
                    state.pending_prompts.lock().unwrap().remove(&prompt_id);
                    AcceptDecision::Reject("user did not respond in time".into())
                }
            }
        })
    }

    fn on_pair_request<'a>(
        &'a self,
        peer: &'a PeerHandshake,
        sas: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PairDecision> + Send + 'a>> {
        let app = self.app.clone();
        let state = self.state.clone();
        let sas = sas.to_string();
        Box::pin(async move {
            let prompt_id = Uuid::new_v4();
            let (tx, rx) = oneshot::channel();
            state
                .pending_prompts
                .lock()
                .unwrap()
                .insert(prompt_id, PromptReply::Pair(tx));
            let _ = app.emit(
                "prompt://incoming",
                PromptPayload {
                    prompt_id: prompt_id.to_string(),
                    kind: "pair",
                    peer_id: peer.hello.id.to_string(),
                    peer_name: peer.hello.name.clone(),
                    fingerprint: peer.hello.fingerprint.to_string(),
                    sas: Some(sas),
                    items: None,
                    total_bytes: None,
                    name: None,
                    remembered_dest: None,
                    downloads: None,
                    desktop: None,
                    documents: None,
                    trusted: false,
                },
            );
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.set_focus();
            }
            match tokio::time::timeout(std::time::Duration::from_secs(120), rx).await {
                Ok(Ok(d)) => d,
                _ => {
                    state.pending_prompts.lock().unwrap().remove(&prompt_id);
                    PairDecision::Reject("user did not respond in time".into())
                }
            }
        })
    }

    fn on_progress(
        &self,
        transfer_id: Uuid,
        peer: &PeerHandshake,
        bytes_done: u64,
        _total_bytes: u64,
        current_file: u32,
        rel_path: &str,
    ) {
        // Ensure a row exists for receive transfers.
        let snap = self.state.manager.snapshot();
        if !snap.iter().any(|t| t.transfer_id == transfer_id) {
            self.state.manager.register(
                transfer_id,
                Direction::Receive,
                peer.hello.id,
                peer.hello.name.clone(),
                0,
                0,
            );
        }
        self.state
            .manager
            .update_progress(transfer_id, bytes_done, current_file, rel_path);
        let _ = self
            .app
            .emit("transfers://updated", self.state.manager.snapshot());
    }

    fn on_transfer_end(
        &self,
        transfer_id: Uuid,
        _peer: &PeerHandshake,
        status: TransferStatus,
        files_written: Vec<PathBuf>,
    ) {
        let st = match status {
            TransferStatus::Completed => TransferState::Completed,
            TransferStatus::Cancelled => TransferState::Cancelled,
            TransferStatus::Failed => TransferState::Failed,
        };
        // Snapshot the row (for size) before finishing.
        let row = self
            .state
            .manager
            .snapshot()
            .into_iter()
            .find(|t| t.transfer_id == transfer_id);
        self.state.manager.finish(transfer_id, st);
        let _ = self
            .app
            .emit("transfers://updated", self.state.manager.snapshot());
        let _ = self.app.emit(
            "transfers://received",
            files_written
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
        );

        // Persist a durable history record for this receive.
        let size = row.as_ref().map(|r| r.bytes_done).unwrap_or(0);
        let file_name = history_file_name(
            files_written
                .first()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy())
                .as_deref(),
            files_written.len() as u32,
        );
        let my_name = self.state.settings.read().unwrap().device_name.clone();
        let rec = TransferRecord {
            id: transfer_id,
            file_name,
            direction: Direction::Receive,
            peer_id: _peer.hello.id,
            source_device: _peer.hello.name.clone(),
            target_device: my_name,
            timestamp_ms: quickdrop_core::history::now_ms(),
            size,
            status: st,
            paths: files_written,
        };
        if let Err(e) = self.state.history.record(&rec) {
            tracing::warn!(error = %e, "failed to record receive history");
        }
        let _ = self.app.emit("history://updated", ());
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Install rustls' default crypto provider once for the process.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let paths = Paths::resolve().expect("failed to resolve QuickDrop paths");
    let log_guard = logging::init(&paths.log_dir);
    let settings = Arc::new(RwLock::new(Settings::load_or_default(&paths)));

    let db = Db::open(&paths.db_dir).expect("failed to open QuickDrop database");
    let identity = Arc::new(
        DeviceIdentity::load_or_create().expect("failed to load or create device identity"),
    );
    tracing::info!(
        device_id = %identity.id(),
        fingerprint = %identity.fingerprint(),
        "device identity ready"
    );

    let trust = TrustStore::open(&db).expect("failed to open trust store");
    let history = HistoryStore::open(&db).expect("failed to open history store");
    let spaces = SpaceStore::open(&db).expect("failed to open spaces store");
    let (manager, _mgr_rx) = TransferManager::new();

    let state = Arc::new(AppState {
        paths,
        settings,
        identity,
        trust,
        manager,
        history,
        spaces,
        discovery: AsyncMutex::new(None),
        receiver: AsyncMutex::new(None),
        listen_port: std::sync::atomic::AtomicU16::new(0),
        share: AsyncMutex::new(None),
        peers_cache: Arc::new(RwLock::new(Vec::new())),
        pending_prompts: Arc::new(std::sync::Mutex::new(Default::default())),
        _db: db,
        _log_guard: log_guard,
    });

    tracing::info!("starting QuickDrop {}", quickdrop_core::VERSION);

    let state_for_setup = state.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            tracing::info!(?argv, "second instance launched, forwarding argv");
            handle_send_argv(app, &argv);
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            app_info,
            list_peers,
            list_trusted_peers,
            forget_peer,
            list_transfers,
            cancel_transfer,
            send_files,
            pair_with,
            answer_prompt,
            set_device_role,
            set_device_prefs,
            list_history,
            delete_history_entry,
            clear_history,
            open_path,
            reveal_path,
            list_spaces,
            create_space,
            delete_space,
            add_space_member,
            remove_space_member,
            add_space_folder,
            space_activity,
            install_context_menu,
            uninstall_context_menu,
            share_file,
            share_list,
            share_stop,
        ])
        .setup(move |app| {
            build_tray(app.handle())?;
            let app_handle = app.handle().clone();
            let state = state_for_setup.clone();

            // Spawn the network/discovery setup on the tokio runtime
            // tauri provides.
            tauri::async_runtime::spawn(async move {
                if let Err(e) = bootstrap(app_handle, state).await {
                    tracing::error!(error = %e, "bootstrap failed");
                }
            });

            // First-instance --hidden / --send handling.
            let argv: Vec<String> = std::env::args().collect();
            let hidden = argv.iter().any(|a| a == "--hidden");
            if argv.iter().any(|a| a == "--send") {
                handle_send_argv(app.handle(), &argv);
            } else if hidden {
                if let Some(win) = app.handle().get_webview_window("main") {
                    let _ = win.hide();
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

async fn bootstrap(app: AppHandle, state: Arc<AppState>) -> anyhow::Result<()> {
    // 1. Start the receiver (TLS listener) so we know the port to advertise.
    let recv_cfg = ReceiverConfig {
        device_name: state.settings.read().unwrap().device_name.clone(),
        os: OsKind::current(),
        device_type: DeviceType::Desktop,
        trust: state.trust.clone(),
        settings: state.settings.clone(),
        default_dest: state.paths.default_dest.clone(),
    };
    let host: Arc<dyn ReceiverHost> = Arc::new(TauriHost {
        app: app.clone(),
        state: state.clone(),
    });
    let (port, recv_handle) =
        quickdrop_core::transfer::receiver::start(recv_cfg, state.identity.clone(), host).await?;
    *state.receiver.lock().await = Some(recv_handle);
    state
        .listen_port
        .store(port, std::sync::atomic::Ordering::Relaxed);
    tracing::info!(port, "receiver listening");

    // 2. Start discovery, advertising that port.
    let trust_clone = state.trust.clone();
    let is_trusted: Arc<dyn Fn(&Fingerprint) -> bool + Send + Sync> =
        Arc::new(move |fp: &Fingerprint| trust_clone.is_trusted(fp).unwrap_or(false));
    let disc_cfg = DiscoveryConfig {
        device_name: state.settings.read().unwrap().device_name.clone(),
        os: OsKind::current(),
        device_type: DeviceType::Desktop,
        tcp_port: port,
    };
    let (svc, mut peers_rx) =
        DiscoveryService::start(state.identity.clone(), disc_cfg, is_trusted).await?;
    *state.discovery.lock().await = Some(svc);

    // 3. Pump peer updates → cache + Tauri event.
    let peers_cache = state.peers_cache.clone();
    let app_p = app.clone();
    tokio::spawn(async move {
        loop {
            {
                let snap = peers_rx.borrow().clone();
                *peers_cache.write().unwrap() = snap.clone();
                let _ = app_p.emit("peers://updated", snap);
            }
            if peers_rx.changed().await.is_err() {
                break;
            }
        }
    });

    Ok(())
}

#[tauri::command]
fn install_context_menu() -> Result<(), String> {
    context_menu::install().map_err(|e| e.to_string())
}

#[tauri::command]
fn uninstall_context_menu() -> Result<(), String> {
    context_menu::uninstall().map_err(|e| e.to_string())
}

/// Sanitize a device name into a DNS-safe mDNS label for share URLs.
fn share_hostname(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_lowercase();
    if s.is_empty() {
        "quickdrop".into()
    } else {
        s
    }
}

/// Ensure the embedded share server is running, returning nothing.
/// Starts it on first use so the port isn't held unless sharing is used.
async fn ensure_share_service(state: &Arc<AppState>) -> Result<(), String> {
    let mut guard = state.share.lock().await;
    if guard.is_none() {
        let hostname = share_hostname(&state.settings.read().unwrap().device_name);
        let cfg = ShareConfig {
            hostname: Some(hostname),
            ..Default::default()
        };
        let svc = ShareService::start(cfg).await.map_err(|e| e.to_string())?;
        let port = svc.port();
        tracing::info!(port, "share server started");
        // Open the LAN port in Windows Defender Firewall so phones on the
        // same Wi-Fi can actually reach us. Public networks block inbound
        // connections by default, which otherwise makes the share URL
        // "not load" on the phone. This prompts UAC at most once; the rule
        // then persists for future shares.
        let _ = tokio::task::spawn_blocking(move || ensure_firewall_rule(port)).await;
        *guard = Some(svc);
    }
    Ok(())
}

/// Best-effort: ensure a Windows Firewall inbound allow rule exists for the
/// share port. No-op on non-Windows. Creating the rule needs elevation, so we
/// only trigger a UAC prompt when the rule is missing.
#[cfg(target_os = "windows")]
fn ensure_firewall_rule(port: u16) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // The outer (unelevated) PowerShell checks for an existing rule and only
    // elevates a child process when one is missing, so repeated shares don't
    // re-prompt. The elevated child creates a persistent inbound TCP rule.
    let script = format!(
        "$ErrorActionPreference='SilentlyContinue'; \
         if (-not (Get-NetFirewallRule -DisplayName 'QuickDrop Share')) {{ \
           Start-Process powershell -Verb RunAs -WindowStyle Hidden -ArgumentList \
           '-NoProfile','-Command',\
           \"New-NetFirewallRule -DisplayName 'QuickDrop Share' -Direction Inbound \
           -Action Allow -Protocol TCP -LocalPort {port} -Profile Any | Out-Null\" \
         }}"
    );

    match std::process::Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
    {
        Ok(s) if s.success() => {
            tracing::info!(port, "firewall rule ensured for share port")
        }
        Ok(_) => tracing::warn!(
            port,
            "firewall rule could not be added (declined or no admin); \
             phones may be unable to connect until port {port} is allowed inbound"
        ),
        Err(e) => tracing::warn!(error = %e, "failed to run firewall helper"),
    }
}

#[cfg(not(target_os = "windows"))]
fn ensure_firewall_rule(_port: u16) {}

/// Publish a file over the embedded HTTP server and return a QR ticket.
#[tauri::command]
async fn share_file(
    state: tauri::State<'_, Arc<AppState>>,
    path: String,
    ttl_secs: Option<u64>,
    max_downloads: Option<u32>,
    password: Option<String>,
) -> Result<ShareTicket, String> {
    let state = state.inner().clone();
    ensure_share_service(&state).await?;
    let guard = state.share.lock().await;
    let svc = guard.as_ref().ok_or("share service not started")?;
    let opts = ShareOptions {
        file_path: PathBuf::from(path),
        ttl_secs: ttl_secs.unwrap_or(30 * 60),
        max_downloads: max_downloads.unwrap_or(0),
        password: password.filter(|p| !p.is_empty()),
    };
    svc.share(opts).map_err(|e| e.to_string())
}

/// List all live shares (host view).
#[tauri::command]
async fn share_list(
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<Vec<ShareSession>, String> {
    let guard = state.share.lock().await;
    Ok(guard.as_ref().map(|s| s.list()).unwrap_or_default())
}

/// Stop sharing a single file.
#[tauri::command]
async fn share_stop(
    state: tauri::State<'_, Arc<AppState>>,
    session_id: String,
) -> Result<bool, String> {
    let guard = state.share.lock().await;
    Ok(guard.as_ref().map(|s| s.stop(&session_id)).unwrap_or(false))
}

fn handle_send_argv(app: &AppHandle, argv: &[String]) {
    if let Some(idx) = argv.iter().position(|a| a == "--send") {
        let paths: Vec<String> = argv.iter().skip(idx + 1).cloned().collect();
        if !paths.is_empty() {
            let _ = app.emit("send://files", paths);
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.set_focus();
            }
        }
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show QuickDrop", true, None::<&str>)?;
    let dest = MenuItem::with_id(app, "open_dest", "Open destination folder", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &dest, &quit])?;

    let _tray = TrayIconBuilder::with_id("main")
        .tooltip("QuickDrop — ready to receive")
        .icon(app.default_window_icon().unwrap().clone())
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }
            "open_dest" => {
                if let Some(state) = app.try_state::<Arc<AppState>>() {
                    let dest = {
                        let s = state.settings.read().unwrap();
                        s.destination
                            .clone()
                            .unwrap_or_else(|| state.paths.default_dest.clone())
                    };
                    let _ = app
                        .opener()
                        .open_path(dest.to_string_lossy().to_string(), None::<&str>);
                }
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                if let Some(win) = tray.app_handle().get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }
        })
        .build(app)?;
    Ok(())
}

// Suppress unused warnings for AtomicBool import etc.
#[allow(dead_code)]
fn _typecheck() {
    let _: AtomicBool = AtomicBool::new(false);
}
