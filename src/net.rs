//! Camera address resolution + validation.
//!
//! The browser *claims* the camera's address at login (a hostname, e.g.
//! `camera.lan`). Before any dial (CGI, snapshot, RTSP) we resolve
//! the claim and refuse to connect to addresses that are never a LAN
//! camera: loopback, link-local (incl. the 169.254.169.254 metadata
//! range), unspecified, and multicast.
use anyhow::{bail, Context, Result};
use std::net::{IpAddr, SocketAddr};

/// True if this address must never be dialed on behalf of a client claim.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_unspecified() || v4.is_loopback() || v4.is_link_local() || v4.is_multicast()
        }
        IpAddr::V6(v6) => {
            v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                // fe80::/10 link-local
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4-mapped (e.g. ::ffff:127.0.0.1)
                || v6.to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_unspecified() || v4.is_loopback() || v4.is_link_local())
        }
    }
}

/// Resolve a claimed camera `host:port`, validating **every** resolved
/// address.
///
/// If any resolved address is blocked, the whole claim is rejected — a
/// hostname with mixed A records (one LAN IP, one `127.0.0.1`) must not
/// steer one of the dials at a blocked range.
pub async fn resolve_camera(host: &str, port: u16) -> Result<SocketAddr> {
    let host = host.trim();
    if host.is_empty() {
        bail!("empty camera host");
    }

    // IP literal — validate directly.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_blocked_ip(ip) {
            bail!("blocked camera address {ip}");
        }
        return Ok(SocketAddr::new(ip, port));
    }

    // Hostname — resolve and validate all answers.
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("failed to resolve camera host {host}"))?
        .collect();
    validate_resolved(host, &addrs)?;
    Ok(addrs
        .into_iter()
        .next()
        .expect("checked non-empty in validate_resolved"))
}

/// Validate the answers for a resolved camera hostname.
///
/// Every answer must be dialable: if any is blocked, the whole claim is
/// rejected (a hostname with mixed A records must not steer one dial at
/// a blocked range).
fn validate_resolved(host: &str, addrs: &[SocketAddr]) -> Result<()> {
    if addrs.is_empty() {
        bail!("camera host {host} resolved to no addresses");
    }
    for a in addrs {
        if is_blocked_ip(a.ip()) {
            bail!("camera host {host} resolves to blocked address {}", a.ip());
        }
    }
    Ok(())
}

/// Parsed `Origin` header of a browser request.
///
/// The browser sets `Origin` from the URL of the page that opened the
/// connection; a page cannot forge another site's origin. For the stock
/// camera UI the origin is the camera itself (`http://<cam>:<port>`) because
/// the UI page is served by the camera — which is exactly the invariant
/// enforced at login (core.rs) and on the HTTP-FLV endpoints (http_flv.rs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginInfo {
    /// Lowercased host (no brackets, no port). May be a hostname or IP.
    pub host: String,
    /// Port, with scheme defaults applied (80 for http, 443 for https).
    pub port: u16,
}

impl OriginInfo {
    /// Parse an `Origin` header value (`scheme://host[:port]`).
    ///
    /// Returns `None` for missing/empty/`null` origins, bad schemes, or
    /// unparseable ports — callers treat `None` as "reject".
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.is_empty() || value == "null" {
            return None;
        }
        let (scheme, authority) = value.split_once("//")?;
        let authority = authority.split('/').next()?.trim();
        let default_port: u16 = match scheme.strip_suffix(':') {
            Some("http") => 80,
            Some("https") => 443,
            _ => return None,
        };
        // IPv6 literals are bracketed: [::1]:88
        let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
            let (host, rest) = inner.split_once(']')?;
            let port = rest.strip_prefix(':')?.parse().ok()?;
            (host.to_ascii_lowercase(), port)
        } else if let Some((h, p)) = authority.rsplit_once(':') {
            (h.to_ascii_lowercase(), p.parse().ok()?)
        } else {
            (authority.to_ascii_lowercase(), default_port)
        };
        if host.is_empty() {
            return None;
        }
        Some(Self { host, port })
    }

    /// The origin matches a claimed camera iff host (case-insensitive) and
    /// port are equal.
    pub fn matches(&self, claimed_host: &str, claimed_port: u16) -> bool {
        self.host == claimed_host.trim().to_ascii_lowercase() && self.port == claimed_port
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_ranges() {
        assert!(is_blocked_ip("127.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip("127.8.9.10".parse().unwrap()));
        assert!(is_blocked_ip("169.254.169.254".parse().unwrap()));
        assert!(is_blocked_ip("0.0.0.0".parse().unwrap()));
        assert!(is_blocked_ip("224.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip("::1".parse().unwrap()));
        assert!(is_blocked_ip("fe80::1".parse().unwrap()));
        assert!(is_blocked_ip("::ffff:127.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip("::ffff:169.254.169.254".parse().unwrap()));
        assert!(!is_blocked_ip("2001:db8::1".parse().unwrap()));
        // LAN and global addresses are the whole point — must be allowed.
        assert!(!is_blocked_ip("192.168.1.50".parse().unwrap()));
        assert!(!is_blocked_ip("10.0.0.5".parse().unwrap()));
        assert!(!is_blocked_ip("172.16.0.1".parse().unwrap()));
        assert!(!is_blocked_ip("8.8.8.8".parse().unwrap()));
    }

    #[tokio::test]
    async fn resolve_ip_literal_ok() {
        let a = resolve_camera("192.168.1.50", 88).await.unwrap();
        assert_eq!(a, "192.168.1.50:88".parse().unwrap());
    }

    #[tokio::test]
    async fn resolve_ip_literal_blocked() {
        assert!(resolve_camera("127.0.0.1", 88).await.is_err());
        assert!(resolve_camera("169.254.169.254", 80).await.is_err());
        assert!(resolve_camera("::1", 88).await.is_err());
        assert!(resolve_camera("0.0.0.0", 88).await.is_err());
    }

    #[tokio::test]
    async fn resolve_empty_host() {
        assert!(resolve_camera("", 88).await.is_err());
        assert!(resolve_camera("   ", 88).await.is_err());
    }

    // NB: no "unresolvable host" test — DNS behavior is environment
    // dependent (some LAN resolvers are wildcard sinkholes).

    #[test]
    fn validate_resolved_allows_lan_ip() {
        let addrs: Vec<SocketAddr> = vec!["192.168.1.50:88".parse().unwrap()];
        assert!(validate_resolved("cam.lan", &addrs).is_ok());
    }

    #[test]
    fn validate_resolved_rejects_mixed_records() {
        // One good LAN answer + one loopback answer → whole claim rejected.
        let addrs: Vec<SocketAddr> = vec![
            "192.168.1.50:88".parse().unwrap(),
            "127.0.0.1:88".parse().unwrap(),
        ];
        assert!(validate_resolved("cam.lan", &addrs).is_err());
    }

    #[test]
    fn validate_resolved_rejects_empty() {
        assert!(validate_resolved("cam.lan", &[]).is_err());
    }
}
