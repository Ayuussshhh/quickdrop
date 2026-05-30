//! # QuickDrop Share
//!
//! Ephemeral, browser-based file sharing over the LAN — the receiver
//! needs **no app, no account, no internet, no pairing**: just a phone
//! on the same Wi-Fi and a camera to scan a QR code.
//!
//! This crate is intentionally self-contained and independent of the
//! desktop transfer engine (`quickdrop-core`). It is the seed for the
//! broader vision (mobile support, browser access, shared workspaces,
//! team collaboration, cloud relay), so it is built on `axum`/`hyper`
//! and a clean session abstraction that those features can extend.
//!
//! ## Quick start
//! ```no_run
//! use quickdrop_share::{ShareService, ShareConfig, session::ShareOptions};
//! # async fn run() -> quickdrop_share::Result<()> {
//! let service = ShareService::start(ShareConfig::default()).await?;
//! let share = service.share(ShareOptions {
//!     file_path: "/path/to/photo.jpg".into(),
//!     ttl_secs: 600,
//!     ..Default::default()
//! })?;
//! println!("Open on your phone: {}", share.url);
//! println!("{}", share.qr_terminal); // scannable QR in the terminal
//! # Ok(()) }
//! ```

#![forbid(unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod error;
pub mod html;
pub mod net;
pub mod qr;
pub mod server;
pub mod session;

pub use error::{Error, Result};
pub use net::ShareEndpoint;
pub use server::{qr_svg, ShareConfig, ShareServer, DEFAULT_SHARE_PORT};
pub use session::{PublicSession, SessionManager, ShareOptions, ShareSession};

use std::sync::Arc;

/// A single URL entry in a [`ShareTicket`], with a human-readable label.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TicketUrl {
    pub url: String,
    pub label: String,
    pub is_hostname: bool,
}

/// Everything the UI needs to present a freshly-created share.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ShareTicket {
    /// Server-side session record.
    pub session: ShareSession,
    /// Best landing URL (first IP-based URL, for the QR).
    pub url: String,
    /// All reachable landing URLs, most-preferred first, with labels.
    pub urls: Vec<TicketUrl>,
    /// Inline `<svg>` QR code for `url` (embed directly in the DOM).
    pub qr_svg: String,
    /// Unicode QR for terminals/logs (handy for headless testing).
    pub qr_terminal: String,
}

/// High-level facade combining the [`ShareServer`] with QR/URL helpers.
///
/// This is the type the Tauri shell holds in its app state: one server
/// for the whole app, many short-lived shares.
#[derive(Debug)]
pub struct ShareService {
    server: Arc<ShareServer>,
}

impl ShareService {
    /// Start the embedded server.
    pub async fn start(cfg: ShareConfig) -> Result<Self> {
        let server = ShareServer::start(cfg).await?;
        Ok(Self {
            server: Arc::new(server),
        })
    }

    /// Bound port (useful for diagnostics / mDNS advertisement).
    pub fn port(&self) -> u16 {
        self.server.port()
    }

    /// All reachable base URLs, most-preferred first.
    pub fn endpoints(&self) -> Vec<ShareEndpoint> {
        self.server.endpoints()
    }

    /// Publish a file and return a fully-rendered [`ShareTicket`].
    pub fn share(&self, opts: ShareOptions) -> Result<ShareTicket> {
        let entry = self.server.sessions().create(opts)?;
        let id = entry.id.clone();
        let eps = self.server.endpoints();

        // IP-based URLs first — phones need a numeric IP; .local mDNS
        // hostnames are unreliable on Android and don't work at all on
        // some networks. The QR always encodes the first IP URL.
        let mut urls: Vec<TicketUrl> = eps
            .iter()
            .filter(|e| !e.is_hostname)
            .map(|e| TicketUrl {
                url: format!("{}/share/{id}", e.base_url),
                label: e.label.clone(),
                is_hostname: false,
            })
            .collect();
        // Append .local hostname URLs at the end.
        for e in eps.iter().filter(|e| e.is_hostname) {
            urls.push(TicketUrl {
                url: format!("{}/share/{id}", e.base_url),
                label: e.label.clone(),
                is_hostname: true,
            });
        }

        // QR always encodes the first IP URL — never a hostname.
        let url = urls
            .iter()
            .find(|u| !u.is_hostname)
            .map(|u| u.url.clone())
            .ok_or(Error::NoLocalAddress)?;

        Ok(ShareTicket {
            session: entry.snapshot(),
            qr_svg: qr::svg(&url)?,
            qr_terminal: qr::terminal(&url)?,
            urls,
            url,
        })
    }

    /// List all live shares (host view).
    pub fn list(&self) -> Vec<ShareSession> {
        self.server.sessions().list()
    }

    /// Stop sharing a single file.
    pub fn stop(&self, session_id: &str) -> bool {
        self.server.sessions().remove(session_id)
    }

    /// Access the underlying session manager.
    pub fn sessions(&self) -> &SessionManager {
        self.server.sessions()
    }
}
