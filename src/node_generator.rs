//! Mayor node install package helpers (node id/secret, network metadata).

use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::reg_secret;
use crate::models::Block;

const NETWORK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Env fallback when the chain has no genesis block yet.
pub fn network_id_env_fallback() -> String {
    std::env::var("QUAZAR_NETWORK_ID").unwrap_or_else(|_| "mainnet".to_string())
}

pub fn network_version() -> &'static str {
    NETWORK_VERSION
}

pub fn format_secret_for_package(secret: &str) -> String {
    let s = secret.trim();
    if s.starts_with("0x") || s.starts_with("0X") {
        return s.to_string();
    }
    if s.len() >= 32 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        return format!("0x{s}");
    }
    s.to_string()
}

pub fn network_reg_secret_for_package() -> String {
    format_secret_for_package(reg_secret())
}

pub fn generate_node_secret_hex() -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"quazar_mayor_node_secret:");
    hasher.update(Uuid::new_v4().as_bytes());
    hasher.update(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            .to_le_bytes(),
    );
    format!("0x{:x}", hasher.finalize())
}

fn parse_qz_node_suffix(id: &str) -> Option<u32> {
    let rest = id.strip_prefix("QZ-NODE-")?;
    rest.parse().ok()
}

pub async fn allocate_mayor_node_id(pool: &PgPool) -> Result<String, sqlx::Error> {
    let app_ids: Vec<String> = sqlx::query_scalar(
        "SELECT node_id FROM mayor_applications WHERE node_id IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;

    let node_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM nodes WHERE id LIKE 'QZ-NODE-%'")
        .fetch_all(pool)
        .await?;

    let mut max_n = 0u32;
    for id in app_ids.into_iter().chain(node_ids) {
        if let Some(n) = parse_qz_node_suffix(&id) {
            max_n = max_n.max(n);
        }
    }
    if let Ok(local) = std::env::var("QUAZAR_NODE_ID") {
        if let Some(n) = parse_qz_node_suffix(local.trim()) {
            max_n = max_n.max(n);
        }
    }

    let candidate = format!("QZ-NODE-{}", max_n + 1);
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM nodes WHERE id = $1)
         OR EXISTS(SELECT 1 FROM mayor_applications WHERE node_id = $1)",
    )
    .bind(&candidate)
    .fetch_one(pool)
    .await?;

    if taken {
        return Ok(format!("QZ-NODE-{}", max_n + 2));
    }
    Ok(candidate)
}

#[derive(Debug, Clone)]
pub struct GenesisChainInfo {
    pub network_id: String,
    pub version: String,
}

pub fn extract_genesis_chain_info(block: &Block) -> GenesisChainInfo {
    let mut network_id = None::<String>;
    let mut version = None::<String>;

    for event in &block.events {
        if let Some(id) = event.data.get("network_id").and_then(|v| v.as_str()) {
            if !id.trim().is_empty() {
                network_id = Some(id.trim().to_string());
            }
        }
        if let Some(ver) = event.data.get("version").and_then(|v| v.as_str()) {
            if !ver.trim().is_empty() {
                version = Some(ver.trim().to_string());
            }
        }
        if let Some(peers) = event.data.get("peers").and_then(|v| v.as_array()) {
            for peer in peers {
                if version.is_none() {
                    if let Some(v) = peer.get("version").and_then(|v| v.as_str()) {
                        if !v.trim().is_empty() {
                            version = Some(v.trim().to_string());
                        }
                    }
                }
            }
        }
    }

    GenesisChainInfo {
        network_id: network_id.unwrap_or_else(|| block.block_hash.clone()),
        version: version.unwrap_or_else(|| NETWORK_VERSION.to_string()),
    }
}

pub async fn load_genesis_chain_info(pool: &PgPool) -> Result<Option<GenesisChainInfo>, sqlx::Error> {
    let block_data: Option<String> = sqlx::query_scalar(
        "SELECT block_data FROM blocks ORDER BY block_number ASC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;

    let Some(raw) = block_data else {
        return Ok(None);
    };

    let block: Block = match serde_json::from_str(&raw) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "failed to parse genesis block_data");
            return Ok(None);
        }
    };
    Ok(Some(extract_genesis_chain_info(&block)))
}

pub async fn resolve_network_id(pool: &PgPool) -> Result<String, sqlx::Error> {
    if let Some(info) = load_genesis_chain_info(pool).await? {
        return Ok(info.network_id);
    }
    Ok(network_id_env_fallback())
}

pub async fn resolve_network_version(pool: &PgPool) -> Result<String, sqlx::Error> {
    if let Some(info) = load_genesis_chain_info(pool).await? {
        return Ok(info.version);
    }
    Ok(NETWORK_VERSION.to_string())
}

/// Parse `peer_url` from stored `sync_peer` (`id@url` or bare URL).
pub fn peer_url_from_sync_peer(sync_peer: &str) -> String {
    let s = sync_peer.trim();
    if let Some((_id, url)) = s.split_once('@') {
        url.trim().to_string()
    } else {
        s.to_string()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MayorNodePackageParams {
    pub node_id: String,
    pub node_secret: String,
    pub reg_secret: String,
    pub network_id: String,
    pub version: String,
    pub sync_peer: String,
    pub peer_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Event;

    #[test]
    fn peer_url_parses_id_at_url() {
        assert_eq!(
            peer_url_from_sync_peer("QZ-NODE-2@http://peer:8080"),
            "http://peer:8080"
        );
        assert_eq!(
            peer_url_from_sync_peer("http://direct:8080"),
            "http://direct:8080"
        );
    }

    #[test]
    fn format_secret_adds_0x_for_hex() {
        assert_eq!(format_secret_for_package("abcd0123"), "0xabcd0123");
        assert_eq!(format_secret_for_package("0xABCD"), "0xABCD");
    }

    #[test]
    fn node_id_suffix_parse() {
        assert_eq!(parse_qz_node_suffix("QZ-NODE-5"), Some(5));
        assert_eq!(parse_qz_node_suffix("QZ-NODE"), None);
    }

    #[test]
    fn genesis_extraction_prefers_event_fields() {
        let block = Block {
            block_number: 1,
            timestamp: 0,
            events: vec![Event {
                event_id: "gen".into(),
                timestamp: 0,
                event_type: "NetworkBootstrap".into(),
                title: "t".into(),
                description: "d".into(),
                initiator: "system".into(),
                data: serde_json::json!({
                    "network_id": "quazar-main",
                    "version": "0.7.0"
                }),
                previous_hash: "0".into(),
                signatures: vec![],
                hash: None,
                public: true,
            }],
            previous_hash: "0".into(),
            block_hash: "abc123".into(),
            events_count: 1,
        };
        let info = extract_genesis_chain_info(&block);
        assert_eq!(info.network_id, "quazar-main");
        assert_eq!(info.version, "0.7.0");
    }

    #[test]
    fn genesis_extraction_falls_back_to_block_hash() {
        let block = Block {
            block_number: 1,
            timestamp: 0,
            events: vec![],
            previous_hash: "0".into(),
            block_hash: "chain-root-hash".into(),
            events_count: 0,
        };
        let info = extract_genesis_chain_info(&block);
        assert_eq!(info.network_id, "chain-root-hash");
    }
}
