//! The embedded HTTP server that browsers connect to.
//!
//! Built on `axum`/`hyper` so it can grow toward the larger QuickDrop
//! vision (browser access, shared workspaces, cloud relay) without a
//! rewrite. Today it exposes exactly four routes plus a root:
//!
//! | Method | Path                  | Purpose                              |
//! |--------|-----------------------|--------------------------------------|
//! | GET    | `/share/:id`          | Mobile landing page (HTML)           |
//! | GET    | `/api/session/:id`    | Session metadata (JSON, no path)     |
//! | GET    | `/download/:id`       | Stream the file (range-capable)      |
//! | DELETE | `/share/:id?token=..` | Host-authenticated "stop sharing"    |
//!
//! Files are streamed straight from disk with `ReaderStream`, so a
//! 10 GB share uses kilobytes of memory. A single `Range` request is
//! honoured (HTTP 206), which already gives pause/resume on big files.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, MethodRouter};
use axum::Router;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::io::ReaderStream;

use crate::error::{Error, Result};
use crate::net::{self, ShareEndpoint};
use crate::session::{looks_like_session_id, SessionEntry, SessionManager};
use crate::{html, qr};

/// Default port for QuickDrop Share. If taken, the server falls back
/// to an OS-assigned ephemeral port and reports the real one.
pub const DEFAULT_SHARE_PORT: u16 = 8080;

/// Configuration for [`ShareServer::start`].
#[derive(Debug, Clone)]
pub struct ShareConfig {
    /// Address to bind. Defaults to `0.0.0.0` so phones on the LAN can
    /// reach it.
    pub bind_addr: IpAddr,
    /// Preferred port. `0` requests an ephemeral port immediately.
    pub preferred_port: u16,
    /// Optional mDNS hostname label for friendly URLs (no `.local`).
    pub hostname: Option<String>,
    /// How often expired sessions are swept.
    pub sweep_interval: Duration,
}

impl Default for ShareConfig {
    fn default() -> Self {
        Self {
            bind_addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            preferred_port: DEFAULT_SHARE_PORT,
            hostname: None,
            sweep_interval: Duration::from_secs(5),
        }
    }
}

#[derive(Clone)]
struct AppState {
    sessions: SessionManager,
    /// Random per-server token gating destructive HTTP routes (DELETE).
    /// Never sent to browsers; the host UI calls [`SessionManager::remove`]
    /// directly instead.
    admin_token: Arc<str>,
}

/// A running share server. Drop or call [`stop`](ShareServer::stop) to
/// shut it down and release the port.
#[derive(Debug)]
pub struct ShareServer {
    sessions: SessionManager,
    port: u16,
    /// mDNS hostname label (without `.local`), stored so live endpoint
    /// scans can include it. Endpoints are never cached — always derived
    /// from the current network interfaces so hotspot/network switches
    /// are reflected immediately.
    hostname: Option<String>,
    shutdown: Option<oneshot::Sender<()>>,
    serve_handle: Option<JoinHandle<()>>,
    sweep_handle: Option<JoinHandle<()>>,
}

impl ShareServer {
    /// Bind and start serving. Returns once the listener is bound so
    /// the caller immediately knows the real port.
    pub async fn start(cfg: ShareConfig) -> Result<Self> {
        let listener = bind(cfg.bind_addr, cfg.preferred_port).await?;
        let port = listener
            .local_addr()
            .map_err(|e| Error::Bind(e.to_string()))?
            .port();

        let sessions = SessionManager::new();
        let admin_token: Arc<str> = Arc::from(random_token().as_str());
        let state = AppState {
            sessions: sessions.clone(),
            admin_token,
        };

        let app = router(state.clone());

        let (tx, rx) = oneshot::channel::<()>();
        let serve_handle = tokio::spawn(async move {
            let server = axum::serve(listener, app).with_graceful_shutdown(async move {
                let _ = rx.await;
            });
            if let Err(e) = server.await {
                tracing::error!(error = %e, "share server stopped with error");
            }
        });

        // Background sweeper, tied to the server lifetime.
        let sweep_sessions = sessions.clone();
        let sweep_interval = cfg.sweep_interval;
        let sweep_handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(sweep_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                sweep_sessions.sweep();
            }
        });

        let hostname = cfg.hostname.clone();
        tracing::info!(port, "QuickDrop Share server listening");

        Ok(Self {
            sessions,
            port,
            hostname,
            shutdown: Some(tx),
            serve_handle: Some(serve_handle),
            sweep_handle: Some(sweep_handle),
        })
    }

    /// The session manager — create/list/remove shares through this.
    pub fn sessions(&self) -> &SessionManager {
        &self.sessions
    }

    /// The actual bound port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// All reachable base URLs, most-preferred first.
    /// Scans live network interfaces every call so hotspot/network
    /// switches are reflected without restarting the server.
    pub fn endpoints(&self) -> Vec<ShareEndpoint> {
        net::endpoints(self.port, self.hostname.as_deref())
    }

    /// The single best IP-based URL to put in a QR code, e.g.
    /// `http://192.168.1.42:8080`. Returns `None` if no private IPv4
    /// interface is up — never falls back to a `.local` hostname because
    /// Android phones cannot resolve mDNS `.local` names reliably.
    pub fn primary_base_url(&self) -> Option<String> {
        self.endpoints()
            .into_iter()
            .find(|e| !e.is_hostname)
            .map(|e| e.base_url)
    }

    /// Full landing URL for a session on the primary endpoint.
    pub fn share_url(&self, session_id: &str) -> Option<String> {
        self.primary_base_url()
            .map(|b| format!("{b}/share/{session_id}"))
    }

    /// Gracefully stop the server and release the port.
    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.sweep_handle.take() {
            h.abort();
        }
        if let Some(h) = self.serve_handle.take() {
            let _ = h.await;
        }
    }
}

impl Drop for ShareServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.sweep_handle.take() {
            h.abort();
        }
        if let Some(h) = self.serve_handle.take() {
            h.abort();
        }
    }
}

/// Build a QR SVG for an arbitrary share URL. Re-exported convenience
/// so callers don't need the `qr` module directly.
pub fn qr_svg(url: &str) -> Result<String> {
    qr::svg(url)
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/share/:id", share_routes())
        .route("/api/session/:id", get(api_session))
        .route("/download/:id", get(download))
        .with_state(state)
}

fn share_routes() -> MethodRouter<AppState> {
    get(landing).delete(delete_share)
}

async fn index() -> impl IntoResponse {
    Html("<!doctype html><meta charset=utf-8><title>QuickDrop Share</title><p>QuickDrop Share is running.")
}

async fn landing(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    if !looks_like_session_id(&id) {
        return not_found_html();
    }
    match st.sessions.get_alive(&id) {
        Some(entry) => Html(html::landing_page(&entry.public_view())).into_response(),
        None => not_found_html(),
    }
}

async fn api_session(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    if !looks_like_session_id(&id) {
        return not_found_json();
    }
    match st.sessions.get_alive(&id) {
        Some(entry) => Json(entry.public_view()).into_response(),
        None => not_found_json(),
    }
}

#[derive(Debug, Deserialize)]
struct DownloadQuery {
    #[serde(default)]
    pw: Option<String>,
}

async fn download(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DownloadQuery>,
    headers: HeaderMap,
) -> Response {
    if !looks_like_session_id(&id) {
        return error_response(&Error::SessionNotFound);
    }
    let entry = match st.sessions.try_begin_download(&id, q.pw.as_deref()) {
        Ok(e) => e,
        Err(e) => return error_response(&e),
    };
    match stream_file(&entry, &headers).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(id = %id, error = %e, "download stream failed");
            error_response(&e)
        }
    }
}

#[derive(Debug, Deserialize)]
struct AdminQuery {
    #[serde(default)]
    token: Option<String>,
}

async fn delete_share(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<AdminQuery>,
) -> Response {
    // Destructive: only the host (who knows the admin token) may revoke
    // over HTTP. Browser receivers cannot kill other people's shares.
    if q.token.as_deref() != Some(&st.admin_token) {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    if st.sessions.remove(&id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        error_response(&Error::SessionNotFound)
    }
}

/// Stream a file, honouring a single `Range` header if present.
async fn stream_file(entry: &SessionEntry, headers: &HeaderMap) -> Result<Response> {
    let total = entry.file_size;
    let mut file = tokio::fs::File::open(&entry.file_path).await?;

    let disposition = content_disposition(&entry.file_name);
    let ctype = HeaderValue::from_str(&entry.content_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));

    if let Some((start, end)) = parse_range(headers, total) {
        let len = end - start + 1;
        file.seek(SeekFrom::Start(start)).await?;
        let stream = ReaderStream::new(file.take(len));
        let body = Body::from_stream(stream);
        let mut resp = Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .body(body)
            .map_err(|e| Error::Bind(e.to_string()))?;
        let h = resp.headers_mut();
        h.insert(header::CONTENT_TYPE, ctype);
        h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        h.insert(header::CONTENT_LENGTH, header_num(len));
        h.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{total}"))
                .unwrap_or_else(|_| HeaderValue::from_static("bytes */*")),
        );
        h.insert(header::CONTENT_DISPOSITION, disposition);
        return Ok(resp);
    }

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);
    let mut resp = Response::new(body);
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, ctype);
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::CONTENT_LENGTH, header_num(total));
    h.insert(header::CONTENT_DISPOSITION, disposition);
    Ok(resp)
}

/// Parse a single `bytes=start-end` range. Returns inclusive byte
/// bounds clamped to the file, or `None` to serve the whole file.
fn parse_range(headers: &HeaderMap, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let raw = headers.get(header::RANGE)?.to_str().ok()?;
    let spec = raw.strip_prefix("bytes=")?;
    // Only the first range is honoured.
    let first = spec.split(',').next()?.trim();
    let (s, e) = first.split_once('-')?;
    let (start, end) = match (s.trim(), e.trim()) {
        ("", "") => return None,
        ("", suffix) => {
            // bytes=-N → last N bytes
            let n: u64 = suffix.parse().ok()?;
            let n = n.min(total);
            (total - n, total - 1)
        }
        (start, "") => {
            let start: u64 = start.parse().ok()?;
            (start, total - 1)
        }
        (start, end) => {
            let start: u64 = start.parse().ok()?;
            let end: u64 = end.parse().ok()?;
            (start, end.min(total - 1))
        }
    };
    if start > end || start >= total {
        return None;
    }
    Some((start, end))
}

/// `Content-Disposition: attachment` with both an ASCII fallback and a
/// UTF-8 `filename*` for non-ASCII names.
fn content_disposition(name: &str) -> HeaderValue {
    let ascii: String = name
        .chars()
        .map(|c| if c.is_ascii() && c != '"' && c != '\\' { c } else { '_' })
        .collect();
    let encoded = percent_encode(name);
    let value = format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}");
    HeaderValue::from_str(&value)
        .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn header_num(n: u64) -> HeaderValue {
    HeaderValue::from_str(&n.to_string()).unwrap_or_else(|_| HeaderValue::from_static("0"))
}

fn not_found_html() -> Response {
    (StatusCode::NOT_FOUND, Html(html::not_found_page())).into_response()
}

fn not_found_json() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "session_not_found" })),
    )
        .into_response()
}

/// Map a domain error to an HTTP response. Note that *all* "you can't
/// have this" cases collapse to 404 unless the client proved knowledge
/// (wrong password → 401), so an attacker cannot enumerate ids.
fn error_response(e: &Error) -> Response {
    match e {
        Error::Unauthorized => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "unauthorized" })),
        )
            .into_response(),
        Error::LimitReached => (
            StatusCode::GONE,
            Json(serde_json::json!({ "error": "limit_reached" })),
        )
            .into_response(),
        _ => not_found_json(),
    }
}

/// Bind the preferred port, falling back to an ephemeral one.
async fn bind(addr: IpAddr, preferred: u16) -> Result<TcpListener> {
    if preferred != 0 {
        if let Ok(l) = TcpListener::bind(SocketAddr::new(addr, preferred)).await {
            return Ok(l);
        }
        tracing::warn!(port = preferred, "preferred share port busy; using ephemeral");
    }
    TcpListener::bind(SocketAddr::new(addr, 0))
        .await
        .map_err(|e| Error::Bind(e.to_string()))
}

fn random_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::RANGE, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn range_full_open_ended() {
        assert_eq!(parse_range(&hdr("bytes=100-"), 1000), Some((100, 999)));
    }

    #[test]
    fn range_closed() {
        assert_eq!(parse_range(&hdr("bytes=0-99"), 1000), Some((0, 99)));
    }

    #[test]
    fn range_suffix() {
        assert_eq!(parse_range(&hdr("bytes=-100"), 1000), Some((900, 999)));
    }

    #[test]
    fn range_clamped_and_rejected() {
        assert_eq!(parse_range(&hdr("bytes=0-99999"), 1000), Some((0, 999)));
        assert_eq!(parse_range(&hdr("bytes=2000-3000"), 1000), None);
        assert_eq!(parse_range(&hdr("garbage"), 1000), None);
    }

    #[test]
    fn disposition_handles_unicode() {
        let v = content_disposition("résumé 简历.pdf");
        let s = v.to_str().unwrap();
        assert!(s.contains("filename="));
        assert!(s.contains("filename*=UTF-8''"));
    }
}
