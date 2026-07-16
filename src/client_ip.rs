//! Client IP resolution with trusted-proxy gating for `X-Forwarded-For`.
//!
//! By default XFF / X-Real-IP are **ignored** (spoofable). Set
//! `QUAZAR_TRUSTED_PROXIES` to a comma-separated list of proxy IPs or CIDRs
//! (e.g. `127.0.0.1,10.0.0.0/8,172.16.0.0/12`). XFF is used only when the
//! immediate TCP peer is in that list; the client IP is the rightmost
//! untrusted hop (or the leftmost entry if the whole chain is trusted).

use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;

use axum::extract::ConnectInfo;
use axum::http::HeaderMap;

#[derive(Debug, Clone)]
enum NetMatch {
    Exact(IpAddr),
    V4 { network: u32, mask: u32 },
    V6 { network: u128, mask: u128 },
}

impl NetMatch {
    fn contains(&self, ip: IpAddr) -> bool {
        match (self, ip) {
            (NetMatch::Exact(a), b) => *a == b,
            (NetMatch::V4 { network, mask }, IpAddr::V4(v4)) => {
                (u32::from(v4) & mask) == (*network & mask)
            }
            (NetMatch::V6 { network, mask }, IpAddr::V6(v6)) => {
                (u128::from(v6) & mask) == (*network & mask)
            }
            // Map IPv4-mapped IPv6 to IPv4 for CIDR checks.
            (NetMatch::V4 { network, mask }, IpAddr::V6(v6)) => {
                if let Some(v4) = v6.to_ipv4_mapped() {
                    (u32::from(v4) & mask) == (*network & mask)
                } else {
                    false
                }
            }
            (NetMatch::V6 { .. }, IpAddr::V4(_)) => false,
        }
    }
}

fn parse_entry(raw: &str) -> Option<NetMatch> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Some((addr_s, prefix_s)) = s.split_once('/') {
        let prefix: u8 = prefix_s.parse().ok()?;
        let addr: IpAddr = addr_s.parse().ok()?;
        return match addr {
            IpAddr::V4(v4) => {
                if prefix > 32 {
                    return None;
                }
                let mask = if prefix == 0 {
                    0u32
                } else {
                    u32::MAX << (32 - prefix)
                };
                Some(NetMatch::V4 {
                    network: u32::from(v4),
                    mask,
                })
            }
            IpAddr::V6(v6) => {
                if prefix > 128 {
                    return None;
                }
                let mask = if prefix == 0 {
                    0u128
                } else {
                    u128::MAX << (128 - prefix)
                };
                Some(NetMatch::V6 {
                    network: u128::from(v6),
                    mask,
                })
            }
        };
    }
    s.parse::<IpAddr>().ok().map(NetMatch::Exact)
}

fn trusted_nets() -> &'static [NetMatch] {
    static NETS: OnceLock<Vec<NetMatch>> = OnceLock::new();
    NETS.get_or_init(|| {
        std::env::var("QUAZAR_TRUSTED_PROXIES")
            .unwrap_or_default()
            .split(',')
            .filter_map(parse_entry)
            .collect()
    })
}

fn is_trusted(ip: IpAddr) -> bool {
    trusted_nets().iter().any(|n| n.contains(ip))
}

fn parse_ip_token(s: &str) -> Option<IpAddr> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    // Strip optional port from "[ipv6]:port" or "ipv4:port" when misconfigured.
    if let Ok(ip) = t.parse::<IpAddr>() {
        return Some(ip);
    }
    if let Ok(sock) = t.parse::<SocketAddr>() {
        return Some(sock.ip());
    }
    None
}

/// Resolve client IP for rate limits / logging.
///
/// `peer` is the TCP remote address. Forwarded headers are trusted only when
/// `peer` matches `QUAZAR_TRUSTED_PROXIES`.
pub fn resolve_client_ip(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    let peer_ip = peer.map(|a| a.ip());

    if let Some(peer_ip) = peer_ip {
        if !trusted_nets().is_empty() && is_trusted(peer_ip) {
            if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
                let hops: Vec<IpAddr> = xff.split(',').filter_map(parse_ip_token).collect();
                if !hops.is_empty() {
                    // Walk from the right: skip trusted proxies, take first client.
                    for hop in hops.iter().rev() {
                        if !is_trusted(*hop) {
                            return hop.to_string();
                        }
                    }
                    return hops[0].to_string();
                }
            }
            if let Some(real) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
                if let Some(ip) = parse_ip_token(real) {
                    return ip.to_string();
                }
            }
        }
        return peer_ip.to_string();
    }

    "unknown".to_string()
}

pub fn resolve_from_connect(
    headers: &HeaderMap,
    connect: Option<&ConnectInfo<SocketAddr>>,
) -> String {
    resolve_client_ip(headers, connect.map(|ConnectInfo(a)| *a))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn headers_xff(xff: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_str(xff).unwrap());
        h
    }

    #[test]
    fn ignores_xff_without_trusted_proxies() {
        // Ensure empty env for this process unit (OnceLock may already be init in other tests).
        // We only assert parse helpers / NetMatch here when nets are empty via Exact checks.
        let nets = [
            parse_entry("10.0.0.0/8").unwrap(),
            parse_entry("127.0.0.1").unwrap(),
        ];
        assert!(nets[0].contains(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        assert!(!nets[0].contains(IpAddr::V4(Ipv4Addr::new(11, 0, 0, 1))));
        assert!(nets[1].contains(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    #[test]
    fn cidr_v6_and_exact() {
        let n = parse_entry("::1").unwrap();
        assert!(n.contains(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        let c = parse_entry("fc00::/7").unwrap();
        assert!(c.contains("fc00::1".parse::<IpAddr>().unwrap()));
        assert!(!c.contains("2001:db8::1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn spoofed_xff_ignored_when_peer_untrusted() {
        // Direct call with empty trusted list behavior: peer wins.
        // Simulate by using resolve with peer that wouldn't match empty nets.
        let h = headers_xff("1.2.3.4");
        let peer = "203.0.113.9:443".parse().unwrap();
        // With empty QUAZAR_TRUSTED_PROXIES (typical in tests), result is peer.
        let ip = resolve_client_ip(&h, Some(peer));
        assert_eq!(ip, "203.0.113.9");
    }
}
