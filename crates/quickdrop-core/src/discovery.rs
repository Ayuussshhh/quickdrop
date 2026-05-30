//! LAN device discovery.
//!
//! Two parallel mechanisms keep peers visible to each other on the
//! same LAN:
//!
//! 1. **mDNS service** (`_quickdrop._tcp.local.`) registered via
//!    `mdns-sd`. This is what AirDrop / Apple devices and most modern
//!    networks expect. TXT records carry the device id, name, OS,
//!    fingerprint, and listening port.
//! 2. **UDP broadcast** on port `54545`. mDNS is sometimes blocked
//!    (corporate Wi-Fi, broken routers, Windows Hyper-V vSwitches).
//!    A 3-second JSON announcement on `255.255.255.255:54545` is the
//!    safety net.
//!
//! Both feed into a single in-memory [`PeerTable`] keyed by device
//! UUID. A peer is "online" while its `last_seen_ms` is within
//! [`PEER_TTL_MS`]; the [`DiscoveryService::start`] watcher emits a
//! fresh `Vec<Peer>` every time the table changes.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::{watch, RwLock};
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep, MissedTickBehavior};
use uuid::Uuid;

use crate::identity::{DeviceIdentity, Fingerprint};
use crate::{Error, Result};

/// mDNS service type. The trailing dot is required by `mdns-sd`.
pub const SERVICE_TYPE: &str = "_quickdrop._tcp.local.";
/// UDP fallback port. Same on every device.
pub const UDP_BEACON_PORT: u16 = 54545;
/// How often we re-broadcast our presence on UDP.
pub const UDP_BEACON_INTERVAL: Duration = Duration::from_secs(3);
/// Peers older than this are considered offline and dropped from
/// the visible list.
pub const PEER_TTL_MS: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OsKind {
    Windows,
    Macos,
    Linux,
    Other,
}

impl OsKind {
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Macos => "macos",
            Self::Linux => "linux",
            Self::Other => "other",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "windows" => Self::Windows,
            "macos" => Self::Macos,
            "linux" => Self::Linux,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceType {
    Desktop,
    Laptop,
    Phone,
    Other,
}

impl DeviceType {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Laptop => "laptop",
            Self::Phone => "phone",
            Self::Other => "other",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "desktop" => Self::Desktop,
            "laptop" => Self::Laptop,
            "phone" => Self::Phone,
            _ => Self::Other,
        }
    }
}

/// A discovered peer. The same struct is used for both mDNS and UDP
/// sources — they merge by UUID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub id: Uuid,
    pub name: String,
    pub os: OsKind,
    pub device_type: DeviceType,
    pub addrs: Vec<SocketAddr>,
    pub fingerprint: Fingerprint,
    pub trusted: bool,
    /// Milliseconds since UNIX epoch of the last advertisement we saw.
    pub last_seen_ms: u64,
}

/// JSON payload of the UDP beacon. Kept tiny to fit a single packet
/// well under the IPv4 minimum MTU.
#[derive(Debug, Serialize, Deserialize)]
struct UdpBeacon {
    /// Magic so we can ignore unrelated UDP traffic on this port.
    magic: String,
    v: u16,
    id: Uuid,
    name: String,
    os: String,
    dt: String,
    fpr: String,
    port: u16,
}

const UDP_MAGIC: &str = "QDROP";

/// Live peer registry. Cheap to clone (Arc inside).
#[derive(Debug, Clone)]
pub struct PeerTable {
    inner: Arc<RwLock<HashMap<Uuid, Peer>>>,
    tx: watch::Sender<Vec<Peer>>,
}

impl PeerTable {
    pub fn new() -> (Self, watch::Receiver<Vec<Peer>>) {
        let (tx, rx) = watch::channel(Vec::new());
        (
            Self {
                inner: Arc::new(RwLock::new(HashMap::new())),
                tx,
            },
            rx,
        )
    }

    pub async fn upsert(&self, peer: Peer) {
        let mut m = self.inner.write().await;
        match m.get_mut(&peer.id) {
            Some(existing) => {
                for a in &peer.addrs {
                    if !existing.addrs.contains(a) {
                        existing.addrs.push(*a);
                    }
                }
                existing.name = peer.name;
                existing.os = peer.os;
                existing.device_type = peer.device_type;
                existing.fingerprint = peer.fingerprint;
                existing.trusted = peer.trusted;
                if peer.last_seen_ms > existing.last_seen_ms {
                    existing.last_seen_ms = peer.last_seen_ms;
                }
            }
            None => {
                m.insert(peer.id, peer);
            }
        }
        let snapshot = sorted_alive(&m);
        drop(m);
        let _ = self.tx.send(snapshot);
    }

    pub async fn sweep(&self) {
        let mut m = self.inner.write().await;
        let now = now_ms();
        m.retain(|_, p| now.saturating_sub(p.last_seen_ms) <= PEER_TTL_MS);
        let snapshot = sorted_alive(&m);
        drop(m);
        let _ = self.tx.send(snapshot);
    }

    pub async fn snapshot(&self) -> Vec<Peer> {
        let m = self.inner.read().await;
        sorted_alive(&m)
    }

    pub async fn get(&self, id: Uuid) -> Option<Peer> {
        self.inner.read().await.get(&id).cloned()
    }
}

fn sorted_alive(m: &HashMap<Uuid, Peer>) -> Vec<Peer> {
    let now = now_ms();
    let mut v: Vec<Peer> = m
        .values()
        .filter(|p| now.saturating_sub(p.last_seen_ms) <= PEER_TTL_MS)
        .cloned()
        .collect();
    v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    v
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Configuration for [`DiscoveryService::start`].
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    pub device_name: String,
    pub os: OsKind,
    pub device_type: DeviceType,
    /// Port the TLS listener accepts connections on.
    pub tcp_port: u16,
}

/// Owns the mDNS daemon, UDP socket, and a background sweeper. Drop
/// the service to stop advertising and free sockets.
pub struct DiscoveryService {
    table: PeerTable,
    daemon: ServiceDaemon,
    fullname: String,
    tasks: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for DiscoveryService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveryService")
            .field("fullname", &self.fullname)
            .finish_non_exhaustive()
    }
}

impl DiscoveryService {
    pub async fn start(
        identity: Arc<DeviceIdentity>,
        cfg: DiscoveryConfig,
        is_trusted: Arc<dyn Fn(&Fingerprint) -> bool + Send + Sync>,
    ) -> Result<(Self, watch::Receiver<Vec<Peer>>)> {
        let (table, rx) = PeerTable::new();
        let daemon =
            ServiceDaemon::new().map_err(|e| Error::Discovery(format!("mdns daemon: {e}")))?;

        // QuickDrop advertises and resolves over IPv4 only (see
        // `local_ipv4_addrs` / `bind_udp_broadcast`). Left enabled, the
        // daemon also binds every IPv6 interface and then floods the log
        // with `Cannot find valid addrs for TYPE_SRV/TYPE_A` ERRORs,
        // because our records carry no AAAA address to match the query.
        // Disabling the IPv6 interface class silences that noise without
        // affecting LAN discovery.
        if let Err(e) = daemon.disable_interface(IfKind::IPv6) {
            tracing::warn!(error = %e, "failed to disable mDNS IPv6 interfaces");
        }

        let host_label = sanitize_label(&cfg.device_name);
        let instance = format!(
            "{}-{}",
            host_label,
            &identity.id().simple().to_string()[..8]
        );
        let host_name = format!("{}.local.", host_label);
        let ips: Vec<IpAddr> = local_ipv4_addrs()
            .into_iter()
            .map(IpAddr::V4)
            .collect();
        if ips.is_empty() {
            tracing::warn!("no non-loopback IPv4 addresses found; mDNS will use loopback");
        }

        let mut props: HashMap<String, String> = HashMap::new();
        props.insert("id".into(), identity.id().to_string());
        props.insert("name".into(), cfg.device_name.clone());
        props.insert("os".into(), cfg.os.as_str().into());
        props.insert("dt".into(), cfg.device_type.as_str().into());
        props.insert("fpr".into(), hex::encode(identity.fingerprint().as_bytes()));
        props.insert("v".into(), crate::VERSION.to_string());

        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &host_name,
            &ips[..],
            cfg.tcp_port,
            Some(props),
        )
        .map_err(|e| Error::Discovery(format!("mdns service info: {e}")))?;
        let fullname = info.get_fullname().to_string();

        daemon
            .register(info)
            .map_err(|e| Error::Discovery(format!("mdns register: {e}")))?;

        // --- mDNS browser task ---
        let browser = daemon
            .browse(SERVICE_TYPE)
            .map_err(|e| Error::Discovery(format!("mdns browse: {e}")))?;
        let table_b = table.clone();
        let local_id = identity.id();
        let trusted_b = is_trusted.clone();
        let mdns_task = tokio::spawn(async move {
            while let Ok(event) = browser.recv_async().await {
                match event {
                    ServiceEvent::ServiceResolved(svc) => {
                        if let Some(peer) = peer_from_mdns(&svc, local_id, trusted_b.as_ref()) {
                            table_b.upsert(peer).await;
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        tracing::debug!(%fullname, "mdns service removed");
                    }
                    _ => {}
                }
            }
        });

        // --- UDP beacon task: sender + listener ---
        let udp = bind_udp_broadcast(UDP_BEACON_PORT)?;
        let udp = Arc::new(udp);

        let beacon = UdpBeacon {
            magic: UDP_MAGIC.into(),
            v: 1,
            id: identity.id(),
            name: cfg.device_name.clone(),
            os: cfg.os.as_str().into(),
            dt: cfg.device_type.as_str().into(),
            fpr: hex::encode(identity.fingerprint().as_bytes()),
            port: cfg.tcp_port,
        };
        let beacon_bytes = serde_json::to_vec(&beacon)?;
        let udp_send = udp.clone();
        let send_task = tokio::spawn(async move {
            let mut tick = interval(UDP_BEACON_INTERVAL);
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            sleep(Duration::from_millis(rand::random::<u64>() % 500)).await;
            loop {
                tick.tick().await;
                let dst: SocketAddr =
                    (Ipv4Addr::new(255, 255, 255, 255), UDP_BEACON_PORT).into();
                if let Err(e) = udp_send.send_to(&beacon_bytes, dst).await {
                    tracing::trace!(error = %e, "udp beacon send failed");
                }
            }
        });

        let udp_recv = udp.clone();
        let table_u = table.clone();
        let trusted_u = is_trusted.clone();
        let recv_task = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                match udp_recv.recv_from(&mut buf).await {
                    Ok((n, src)) => {
                        let Ok(b) = serde_json::from_slice::<UdpBeacon>(&buf[..n]) else {
                            continue;
                        };
                        if b.magic != UDP_MAGIC {
                            continue;
                        }
                        if b.id == local_id {
                            continue; // our own broadcast loopback
                        }
                        let Ok(fp_bytes) = hex::decode(&b.fpr) else {
                            continue;
                        };
                        if fp_bytes.len() != 16 {
                            continue;
                        }
                        let mut fp = [0u8; 16];
                        fp.copy_from_slice(&fp_bytes);
                        let fp = Fingerprint(fp);
                        let addr = SocketAddr::new(src.ip(), b.port);
                        let peer = Peer {
                            id: b.id,
                            name: b.name,
                            os: OsKind::parse(&b.os),
                            device_type: DeviceType::parse(&b.dt),
                            addrs: vec![addr],
                            fingerprint: fp,
                            trusted: (trusted_u)(&fp),
                            last_seen_ms: now_ms(),
                        };
                        table_u.upsert(peer).await;
                    }
                    Err(e) => {
                        tracing::trace!(error = %e, "udp recv error");
                        sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        });

        // --- Sweeper: drop stale peers periodically ---
        let table_s = table.clone();
        let sweep_task = tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(2));
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                table_s.sweep().await;
            }
        });

        tracing::info!(%fullname, port = cfg.tcp_port, "discovery started");

        Ok((
            Self {
                table,
                daemon,
                fullname,
                tasks: vec![mdns_task, send_task, recv_task, sweep_task],
            },
            rx,
        ))
    }

    pub fn table(&self) -> PeerTable {
        self.table.clone()
    }
}

impl Drop for DiscoveryService {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
        for t in self.tasks.drain(..) {
            t.abort();
        }
    }
}

fn peer_from_mdns(
    svc: &mdns_sd::ServiceInfo,
    local_id: Uuid,
    is_trusted: &(dyn Fn(&Fingerprint) -> bool + Send + Sync),
) -> Option<Peer> {
    let id_str = svc.get_property_val_str("id")?;
    let id = Uuid::parse_str(id_str).ok()?;
    if id == local_id {
        return None;
    }
    let name = svc
        .get_property_val_str("name")
        .map(|s| s.to_string())
        .unwrap_or_else(|| svc.get_hostname().trim_end_matches('.').to_string());
    let os = svc
        .get_property_val_str("os")
        .map(OsKind::parse)
        .unwrap_or(OsKind::Other);
    let dt = svc
        .get_property_val_str("dt")
        .map(DeviceType::parse)
        .unwrap_or(DeviceType::Other);
    let fpr_hex = svc.get_property_val_str("fpr")?;
    let fpr_bytes = hex::decode(fpr_hex).ok()?;
    if fpr_bytes.len() != 16 {
        return None;
    }
    let mut fp = [0u8; 16];
    fp.copy_from_slice(&fpr_bytes);
    let fingerprint = Fingerprint(fp);
    let port = svc.get_port();
    let mut addrs: Vec<SocketAddr> = svc
        .get_addresses()
        .iter()
        .map(|ip| SocketAddr::new(*ip, port))
        .collect();
    addrs.sort();
    addrs.dedup();
    Some(Peer {
        id,
        name,
        os,
        device_type: dt,
        addrs,
        fingerprint,
        trusted: is_trusted(&fingerprint),
        last_seen_ms: now_ms(),
    })
}

fn local_ipv4_addrs() -> Vec<Ipv4Addr> {
    match if_addrs::get_if_addrs() {
        Ok(addrs) => addrs
            .into_iter()
            .filter(|a| !a.is_loopback())
            .filter_map(|a| match a.ip() {
                IpAddr::V4(v4) => Some(v4),
                _ => None,
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "failed to enumerate interfaces");
            Vec::new()
        }
    }
}

fn sanitize_label(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = s.trim_matches('-').replace("--", "-");
    if trimmed.is_empty() {
        "quickdrop".into()
    } else {
        trimmed.to_lowercase()
    }
}

/// Bind a UDP socket suitable for both sending and receiving
/// broadcasts on `0.0.0.0:port` with `SO_REUSEADDR` so multiple
/// QuickDrop instances on the same machine (rare but possible) don't
/// fight for the port.
fn bind_udp_broadcast(port: u16) -> Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|e| Error::Discovery(format!("udp socket: {e}")))?;
    sock.set_reuse_address(true)
        .map_err(|e| Error::Discovery(format!("udp reuse_address: {e}")))?;
    sock.set_broadcast(true)
        .map_err(|e| Error::Discovery(format!("udp broadcast: {e}")))?;
    sock.set_nonblocking(true)
        .map_err(|e| Error::Discovery(format!("udp nonblocking: {e}")))?;
    let addr: SocketAddr = (Ipv4Addr::UNSPECIFIED, port).into();
    sock.bind(&addr.into())
        .map_err(|e| Error::Discovery(format!("udp bind {port}: {e}")))?;
    let std_sock: std::net::UdpSocket = sock.into();
    UdpSocket::from_std(std_sock).map_err(|e| Error::Discovery(format!("udp tokio: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_label_works() {
        assert_eq!(sanitize_label("Ayush PC!"), "ayush-pc");
        assert_eq!(sanitize_label(""), "quickdrop");
        assert_eq!(sanitize_label("---"), "quickdrop");
        assert_eq!(sanitize_label("MacBook-Pro"), "macbook-pro");
    }

    #[test]
    fn os_and_device_roundtrip() {
        for o in [OsKind::Windows, OsKind::Macos, OsKind::Linux, OsKind::Other] {
            assert_eq!(OsKind::parse(o.as_str()), o);
        }
        for d in [
            DeviceType::Desktop,
            DeviceType::Laptop,
            DeviceType::Phone,
            DeviceType::Other,
        ] {
            assert_eq!(DeviceType::parse(d.as_str()), d);
        }
    }

    #[tokio::test]
    async fn peer_table_upsert_emits() {
        let (table, mut rx) = PeerTable::new();
        let now = now_ms();
        let peer = Peer {
            id: Uuid::new_v4(),
            name: "Alice".into(),
            os: OsKind::Linux,
            device_type: DeviceType::Laptop,
            addrs: vec!["127.0.0.1:1234".parse().unwrap()],
            fingerprint: Fingerprint([1u8; 16]),
            trusted: false,
            last_seen_ms: now,
        };
        table.upsert(peer.clone()).await;
        rx.changed().await.unwrap();
        assert_eq!(rx.borrow().len(), 1);
    }
}
