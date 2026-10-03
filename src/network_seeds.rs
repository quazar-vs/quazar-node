//! Network bootstrap seeds (GitHub / local file) — not the trust-of-record.
//!
//! Trust identities live in `nodes` + PeerListUpdate gossip.
//! Seeds only provide first reachable tips so a new node can dial outbound.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use crate::nodes::{Node, NodeRegistry, NodeStatus};

const DEFAULT_SEEDS_URLS: &[&str] = &[
    "https://raw.githubusercontent.com/quazar-vs/quazar-node/main/network/seeds.json",
    "https://raw.githubusercontent.com/quazar-vs/quazar-node/master/network/seeds.json",
];

#[derive(Debug, Clone, Deserialize)]
pub struct SeedsFile {
    pub version: Option<u32>,
    pub seeds: Vec<SeedEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SeedEntry {
    pub node_id: String,
    #[serde(default)]
    pub endpoints: Vec<String>,
    #[serde(default)]
    pub pubkey: Option<String>,
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(format!("quazar-node/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())
}

fn github_token() -> Option<String> {
    for key in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(v) = std::env::var(key) {
            let t = v.trim().to_string();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

async fn fetch_url_text(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let mut req = client.get(url);
    if let Some(token) = github_token() {
        if url.contains("githubusercontent.com") || url.contains("github.com") {
            req = req.header("Authorization", format!("Bearer {token}"));
            req = req.header("Accept", "application/vnd.github.raw");
        }
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} for {url}", resp.status()));
    }
    resp.text().await.map_err(|e| e.to_string())
}

fn local_seeds_path() -> PathBuf {
    if let Ok(p) = std::env::var("QUAZAR_SEEDS_PATH") {
        return PathBuf::from(p);
    }
    PathBuf::from("network/seeds.json")
}

/// Load seeds: env URL → GitHub defaults → local file.
pub async fn load_seeds() -> Result<SeedsFile, String> {
    let client = http_client()?;

    if let Ok(url) = std::env::var("QUAZAR_SEEDS_URL") {
        let url = url.trim().to_string();
        if !url.is_empty() {
            match fetch_url_text(&client, &url).await {
                Ok(text) => {
                    return serde_json::from_str(&text)
                        .map_err(|e| format!("parse seeds from {url}: {e}"));
                }
                Err(e) => tracing::warn!(%url, error = %e, "QUAZAR_SEEDS_URL fetch failed"),
            }
        }
    }

    for url in DEFAULT_SEEDS_URLS {
        match fetch_url_text(&client, url).await {
            Ok(text) => match serde_json::from_str::<SeedsFile>(&text) {
                Ok(file) => {
                    tracing::info!(%url, seeds = file.seeds.len(), "network seeds loaded from GitHub");
                    return Ok(file);
                }
                Err(e) => tracing::warn!(%url, error = %e, "seeds JSON parse failed"),
            },
            Err(e) => tracing::debug!(%url, error = %e, "seeds fetch miss"),
        }
    }

    let path = local_seeds_path();
    if let Ok(text) = std::fs::read_to_string(&path) {
        match serde_json::from_str::<SeedsFile>(&text) {
            Ok(file) => {
                tracing::info!(
                    path = %path.display(),
                    seeds = file.seeds.len(),
                    "network seeds loaded from local file"
                );
                return Ok(file);
            }
            Err(e) => {
                return Err(format!("parse local seeds {}: {e}", path.display()));
            }
        }
    }

    // Embedded fallback shipped in the binary (same as repo network/seeds.json).
    const EMBEDDED: &str = include_str!("../network/seeds.json");
    let file: SeedsFile = serde_json::from_str(EMBEDDED)
        .map_err(|e| format!("parse embedded seeds: {e}"))?;
    tracing::info!(
        seeds = file.seeds.len(),
        "network seeds loaded from embedded fallback"
    );
    Ok(file)
}

/// Register seed tips into trust table + ephemeral endpoints (outbound bootstrap).
pub async fn apply_seeds_to_registry(
    registry: &NodeRegistry,
    pool: &sqlx::PgPool,
    my_node_id: &str,
    file: &SeedsFile,
) -> Result<usize, sqlx::Error> {
    let mut n = 0usize;
    for seed in &file.seeds {
        let id = seed.node_id.trim();
        if id.is_empty() {
            continue;
        }
        // Always record published endpoints (including our own id from seeds).
        for ep in &seed.endpoints {
            let ep = ep.trim();
            if ep.is_empty() {
                continue;
            }
            crate::p2p::upsert_endpoint(pool, id, ep, "seed").await?;
        }
        if id == my_node_id {
            continue;
        }
        let primary = seed
            .endpoints
            .iter()
            .map(|s| s.trim())
            .find(|s| !s.is_empty())
            .unwrap_or("");
        let node = Node {
            id: id.to_string(),
            url: primary.to_string(),
            public_key: seed.pubkey.clone(),
            status: NodeStatus::Alive,
            last_seen: chrono::Utc::now(),
            version: "seed".into(),
        };
        registry.upsert_node(&node).await?;
        n += 1;
        tracing::info!(peer_id = %id, endpoints = ?seed.endpoints, "seed peer registered");
    }
    Ok(n)
}

pub async fn load_and_apply_seeds(
    registry: &NodeRegistry,
    pool: &sqlx::PgPool,
    my_node_id: &str,
) {
    match load_seeds().await {
        Ok(file) => match apply_seeds_to_registry(registry, pool, my_node_id, &file).await {
            Ok(n) => tracing::info!(count = n, "bootstrap seeds applied"),
            Err(e) => tracing::warn!(error = %e, "failed to apply seeds to registry"),
        },
        Err(e) => tracing::warn!(error = %e, "network seeds not loaded (set QUAZAR_BOOTSTRAP_PEERS or network/seeds.json)"),
    }
}
