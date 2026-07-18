//! Client IP resolution with trusted-proxy gating for `X-Forwarded-For`.
//!
//! Trusted proxies come from two sources (merged):
//! 1. **Primary:** host/IP of `alive` peers in the `nodes` table (P2P registry),
//!    excluding local addresses (`localhost`, loopback, unspecified, link-local).
//! 2. **Additional:** `QUAZAR_TRUSTED_PROXIES` env (comma-separated IPs/CIDRs) —
//!    use this for a local nginx that is not in the registry (e.g. `127.0.0.1`).
//!
//! XFF / X-Real-IP are used only when the immediate TCP peer is in that combined
//! list; the client IP is the rightmost untrusted hop (or the leftmost entry if
//! the whole chain is trusted).
//!
//! Node-derived entries are cached (~45s) so request paths do not hit PostgreSQL
//! on every call.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ConnectInfo;
use axum::http::HeaderMap;
use sqlx::PgPool;
use tokio::sync::RwLock;

const NODES_CACHE_TTL: Duration = Duration::from_secs(45);

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

fn env_trusted_nets() -> Vec<NetMatch> {
    std::env::var("QUAZAR_TRUSTED_PROXIES")
        .unwrap_or_default()
        .split(',')
        .filter_map(parse_entry)
        .collect()
}

/// Snapshot of combined trusted proxy nets (env + nodes).
#[derive(Debug, Clone, Default)]
pub struct TrustedNets {
    nets: Vec<NetMatch>,
}

impl TrustedNets {
    pub fn empty() -> Self {
        Self { nets: Vec::new() }
    }

    pub fn from_env() -> Self {
        Self {
            nets: env_trusted_nets(),
        }
    }

    fn is_empty(&self) -> bool {
        self.nets.is_empty()
    }

    fn is_trusted(&self, ip: IpAddr) -> bool {
        self.nets.iter().any(|n| n.contains(ip))
    }
}

struct NodesCacheInner {
    nets: Vec<NetMatch>,
    fetched_at: Option<Instant>,
}

/// Cached trusted proxies from `nodes` + env allowlist.
#[derive(Clone)]
pub struct TrustedProxyCache {
    pool: PgPool,
    env_nets: Vec<NetMatch>,
    nodes: Arc<RwLock<NodesCacheInner>>,
}

impl TrustedProxyCache {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            env_nets: env_trusted_nets(),
            nodes: Arc::new(RwLock::new(NodesCacheInner {
                nets: Vec::new(),
                fetched_at: None,
            })),
        }
    }

    /// Combined allowlist: registry peers (non-local) + `QUAZAR_TRUSTED_PROXIES`.
    pub async fn snapshot(&self) -> TrustedNets {
        self.refresh_nodes_if_needed().await;
        let nodes = self.nodes.read().await;
        let mut nets = self.env_nets.clone();
        nets.extend(nodes.nets.iter().cloned());
        TrustedNets { nets }
    }

    /// Drop cached node IPs so the next `snapshot` reloads from DB.
    pub async fn invalidate(&self) {
        let mut guard = self.nodes.write().await;
        guard.fetched_at = None;
        guard.nets.clear();
    }

    async fn refresh_nodes_if_needed(&self) {
        {
            let guard = self.nodes.read().await;
            if let Some(at) = guard.fetched_at {
                if at.elapsed() < NODES_CACHE_TTL {
                    return;
                }
            }
        }

        let urls: Vec<String> = match sqlx::query_scalar(
            r#"
            SELECT url FROM nodes
            WHERE status = 'alive' AND TRIM(url) <> ''
            "#,
        )
        .fetch_all(&self.pool)
        .await
        {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "trusted proxies: failed to load nodes urls");
                return;
            }
        };

        let mut nets = Vec::new();
        for url in &urls {
            collect_trusted_from_node_url(url, &mut nets).await;
        }

        let mut guard = self.nodes.write().await;
        guard.nets = nets;
        guard.fetched_at = Some(Instant::now());
    }
}

/// Extract host from a node URL (`http://host:port` → `host`).
pub fn host_from_node_url(url: &str) -> Option<String> {
    let s = url.trim();
    if s.is_empty() {
        return None;
    }
    let after_scheme = match s.split_once("://") {
        Some((_, rest)) => rest,
        None => s,
    };
    let authority = after_scheme.split('/').next()?.trim();
    if authority.is_empty() {
        return None;
    }
    if authority.starts_with('[') {
        let end = authority.find(']')?;
        let host = &authority[1..end];
        if host.is_empty() {
            return None;
        }
        return Some(host.to_string());
    }
    // Strip :port for IPv4 / hostname (not bare IPv6 without brackets).
    let host = match authority.rfind(':') {
        Some(i) if authority[i + 1..].chars().all(|c| c.is_ascii_digit()) => &authority[..i],
        _ => authority,
    };
    if host.is_empty() {
        return None;
    }
    Some(host.to_string())
}

/// True for loopback, unspecified, link-local, and well-known local hostnames.
pub fn is_local_host(host: &str) -> bool {
    let h = host
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    if h.is_empty() {
        return true;
    }
    if h == "localhost" || h == "localhost." || h.ends_with(".localhost") {
        return true;
    }
    if let Ok(ip) = h.parse::<IpAddr>() {
        return is_local_ip(ip);
    }
    false
}

pub fn is_local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_local_ipv4(v4),
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_local_ipv4(v4);
            }
            // fe80::/10 link-local
            let seg0 = v6.segments()[0];
            (seg0 & 0xffc0) == 0xfe80
        }
    }
}

fn is_local_ipv4(v4: Ipv4Addr) -> bool {
    v4.is_loopback() || v4.is_unspecified() || v4.is_link_local()
}

async fn collect_trusted_from_node_url(url: &str, out: &mut Vec<NetMatch>) {
    let Some(host) = host_from_node_url(url) else {
        return;
    };
    if is_local_host(&host) {
        return;
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_local_ip(ip) {
            out.push(NetMatch::Exact(ip));
        }
        return;
    }

    // Hostname: resolve and keep non-local addresses.
    let lookup = format!("{host}:0");
    let resolved: Result<Vec<SocketAddr>, _> = tokio::net::lookup_host(lookup.as_str())
        .await
        .map(|iter| iter.collect());
    match resolved {
        Ok(addrs) => {
            for addr in addrs {
                let ip = addr.ip();
                if !is_local_ip(ip) {
                    out.push(NetMatch::Exact(ip));
                }
            }
        }
        Err(e) => {
            tracing::debug!(host = %host, error = %e, "trusted proxies: DNS lookup failed");
        }
    }
}

fn parse_ip_token(s: &str) -> Option<IpAddr> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
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
/// `peer` matches the combined allowlist (`nodes` + `QUAZAR_TRUSTED_PROXIES`).
pub fn resolve_client_ip(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trusted: &TrustedNets,
) -> String {
    let peer_ip = peer.map(|a| a.ip());

    if let Some(peer_ip) = peer_ip {
        if !trusted.is_empty() && trusted.is_trusted(peer_ip) {
            if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
                let hops: Vec<IpAddr> = xff.split(',').filter_map(parse_ip_token).collect();
                if !hops.is_empty() {
                    // Walk from the right: skip trusted proxies, take first client.
                    for hop in hops.iter().rev() {
                        if !trusted.is_trusted(*hop) {
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
    trusted: &TrustedNets,
) -> String {
    resolve_client_ip(headers, connect.map(|ConnectInfo(a)| *a), trusted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::net::Ipv6Addr;

    fn headers_xff(xff: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_str(xff).unwrap());
        h
    }

    #[test]
    fn host_from_http_url() {
        assert_eq!(
            host_from_node_url("http://node.example:8080").as_deref(),
            Some("node.example")
        );
        assert_eq!(
            host_from_node_url("https://203.0.113.10:443/path").as_deref(),
            Some("203.0.113.10")
        );
        assert_eq!(
            host_from_node_url("http://[2001:db8::1]:8080").as_deref(),
            Some("2001:db8::1")
        );
        assert_eq!(host_from_node_url("http://localhost:8080").as_deref(), Some("localhost"));
    }

    #[test]
    fn excludes_local_hosts_and_ips() {
        assert!(is_local_host("localhost"));
        assert!(is_local_host("LOCALHOST"));
        assert!(is_local_host("127.0.0.1"));
        assert!(is_local_host("::1"));
        assert!(is_local_host("0.0.0.0"));
        assert!(is_local_host("169.254.1.1"));
        assert!(is_local_host("fe80::1"));
        assert!(is_local_host("app.localhost"));
        assert!(!is_local_host("203.0.113.9"));
        assert!(!is_local_host("node.example"));
        assert!(!is_local_host("10.0.0.5"));
        assert!(is_local_ip(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(is_local_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!is_local_ip("203.0.113.9".parse().unwrap()));
    }

    #[test]
    fn local_node_url_skipped_from_trusted() {
        assert!(is_local_host(&host_from_node_url("http://127.0.0.1:8080").unwrap()));
        assert!(is_local_host(&host_from_node_url("http://localhost:8080").unwrap()));
        assert!(is_local_host(&host_from_node_url("http://[::1]:8080").unwrap()));
        assert!(!is_local_host(
            &host_from_node_url("http://203.0.113.50:8080").unwrap()
        ));
    }

    #[test]
    fn cidr_and_exact_match() {
        let nets = [
            parse_entry("10.0.0.0/8").unwrap(),
            parse_entry("127.0.0.1").unwrap(),
        ];
        assert!(nets[0].contains(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        assert!(!nets[0].contains(IpAddr::V4(Ipv4Addr::new(11, 0, 0, 1))));
        assert!(nets[1].contains(IpAddr::V4(Ipv4Addr::LOCALHOST)));

        let n = parse_entry("::1").unwrap();
        assert!(n.contains(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        let c = parse_entry("fc00::/7").unwrap();
        assert!(c.contains("fc00::1".parse::<IpAddr>().unwrap()));
        assert!(!c.contains("2001:db8::1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn spoofed_xff_ignored_when_peer_untrusted() {
        let h = headers_xff("1.2.3.4");
        let peer = "203.0.113.9:443".parse().unwrap();
        let ip = resolve_client_ip(&h, Some(peer), &TrustedNets::empty());
        assert_eq!(ip, "203.0.113.9");
    }

    #[test]
    fn xff_honored_when_peer_in_trusted() {
        let h = headers_xff("198.51.100.7, 10.0.0.1");
        let peer = "10.0.0.1:443".parse().unwrap();
        let trusted = TrustedNets {
            nets: vec![parse_entry("10.0.0.1").unwrap()],
        };
        let ip = resolve_client_ip(&h, Some(peer), &trusted);
        assert_eq!(ip, "198.51.100.7");
    }
}
