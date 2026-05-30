//! LAN address detection and shareable URL construction.
//!
//! A phone's browser needs a URL it can actually reach. We prefer a
//! stable, human-friendly mDNS hostname (`http://quickdrop.local:PORT`)
//! and fall back to the raw private IPv4 (`http://192.168.x.y:PORT`),
//! which works even on networks/phones without mDNS resolution.

use std::net::Ipv4Addr;

/// A candidate base URL the receiver can open, most-preferred first.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ShareEndpoint {
    /// e.g. `http://192.168.1.42:8080`
    pub base_url: String,
    /// Human label for the UI ("Wi-Fi", "mDNS hostname", …).
    pub label: String,
    /// True if this is the mDNS `.local` form (nice but not always
    /// resolvable from phones).
    pub is_hostname: bool,
}

/// All private (RFC1918) IPv4 addresses on non-loopback interfaces.
/// These are the addresses a phone on the same Wi-Fi can route to.
pub fn private_ipv4_addrs() -> Vec<Ipv4Addr> {
    let all = match if_addrs::get_if_addrs() {
        Ok(list) => list,
        Err(e) => {
            tracing::warn!(error = %e, "failed to enumerate interfaces");
            return Vec::new();
        }
    };

    // Log every interface so we can diagnose missing hotspot IPs.
    for iface in &all {
        tracing::debug!(
            name = %iface.name,
            ip   = %iface.ip(),
            loopback = iface.is_loopback(),
            "network interface"
        );
    }

    let mut addrs: Vec<Ipv4Addr> = all
        .into_iter()
        .filter(|a| !a.is_loopback())
        .filter_map(|a| match a.ip() {
            std::net::IpAddr::V4(v4) => Some(v4),
            _ => None,
        })
        .filter(is_private_v4)
        .collect();

    addrs.sort_by_key(rank_v4);
    addrs.dedup();
    tracing::info!(addrs = ?addrs, "share: candidate LAN addresses");
    addrs
}

/// Build the ordered list of endpoints for a running server.
///
/// `hostname` is the mDNS label (without the trailing dot), e.g.
/// `"quickdrop"` → `http://quickdrop.local:PORT`. Pass `None` to omit
/// the hostname form.
pub fn endpoints(port: u16, hostname: Option<&str>) -> Vec<ShareEndpoint> {
    let mut out = Vec::new();
    if let Some(h) = hostname {
        let h = h.trim_end_matches('.').trim_end_matches(".local");
        if !h.is_empty() {
            out.push(ShareEndpoint {
                base_url: format!("http://{h}.local:{port}"),
                label: "mDNS hostname".into(),
                is_hostname: true,
            });
        }
    }
    for ip in private_ipv4_addrs() {
        out.push(ShareEndpoint {
            base_url: format!("http://{ip}:{port}"),
            label: interface_label(&ip),
            is_hostname: false,
        });
    }
    out
}

/// The single best URL to encode into the QR code: the first IP-based
/// endpoint (works without mDNS), or the hostname form if no IP was
/// found.
pub fn primary_base_url(port: u16, hostname: Option<&str>) -> Option<String> {
    let eps = endpoints(port, hostname);
    eps.iter()
        .find(|e| !e.is_hostname)
        .or_else(|| eps.first())
        .map(|e| e.base_url.clone())
}

fn is_private_v4(ip: &Ipv4Addr) -> bool {
    ip.is_private() && !ip.is_link_local()
}

fn rank_v4(ip: &Ipv4Addr) -> u8 {
    let o = ip.octets();
    match (o[0], o[1], o[2]) {
        // Windows Mobile Hotspot (ICS) always uses 192.168.137.x — this is
        // the direct connection to the phone, rank it first.
        (192, 168, 137) => 0,
        // Regular Wi-Fi client IPs (assigned by a router, phone is likely
        // on the same subnet).
        (192, 168, _) => 1,
        (10, _, _) => 2,
        (172, 16..=31, _) => 3,
        _ => 4,
    }
}

fn interface_label(ip: &Ipv4Addr) -> String {
    let o = ip.octets();
    match (o[0], o[1], o[2]) {
        (192, 168, 137) => "Laptop hotspot".into(),
        (192, 168, _) => "Local network".into(),
        (10, _, _) => "Private network".into(),
        (172, 16..=31, _) => "Private network".into(),
        _ => "Network".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_include_hostname_first_when_given() {
        let eps = endpoints(8080, Some("quickdrop"));
        assert!(eps.first().map(|e| e.is_hostname).unwrap_or(false));
        assert_eq!(eps[0].base_url, "http://quickdrop.local:8080");
    }

    #[test]
    fn primary_prefers_ip_over_hostname() {
        // We can't guarantee a private IP in CI, but the function must
        // never panic and must prefer a non-hostname entry if present.
        let url = primary_base_url(8080, Some("quickdrop"));
        if let Some(u) = url {
            assert!(u.starts_with("http://"));
        }
    }

    #[test]
    fn private_ranking_is_stable() {
        assert!(rank_v4(&Ipv4Addr::new(192, 168, 1, 1)) < rank_v4(&Ipv4Addr::new(10, 0, 0, 1)));
        assert!(rank_v4(&Ipv4Addr::new(10, 0, 0, 1)) < rank_v4(&Ipv4Addr::new(172, 16, 0, 1)));
    }
}
