//! Genetic memory (DNA) and law proxy for Algonism — identity source for the agent.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Path as AxumPath, State},
    response::{IntoResponse, Json, Response},
};
use serde::Serialize;
use tokio::sync::RwLock;

use crate::response;
use crate::AppState;

/// Candidate DNA sources. `algonism-core` is private — set `GITHUB_TOKEN`.
/// Verified layout (main): root `ALGONISM.md`, `DNA_ALGONISM.md`, `README.md`.
const DEFAULT_DNA_URLS: &[&str] = &[
    // Working identity prompt (ALGONISM.md) then philosophical DNA.
    "https://raw.githubusercontent.com/quazar-vs/algonism-core/main/ALGONISM.md",
    "https://raw.githubusercontent.com/quazar-vs/algonism-core/main/DNA_ALGONISM.md",
    // Contents API (same paths; reliable with token + Accept: raw).
    "https://api.github.com/repos/quazar-vs/algonism-core/contents/ALGONISM.md",
    "https://api.github.com/repos/quazar-vs/algonism-core/contents/DNA_ALGONISM.md",
    // Branch fallbacks.
    "https://raw.githubusercontent.com/quazar-vs/algonism-core/master/ALGONISM.md",
    "https://raw.githubusercontent.com/quazar-vs/algonism-core/master/DNA_ALGONISM.md",
];

const DEFAULT_LAW_BASE: &str =
    "https://raw.githubusercontent.com/quazar-vs/quazar-law/main";

const DEFAULT_CACHE_PATH: &str = "/var/lib/quazar/algonism/ALGONISM.md";

#[derive(Debug, Clone, Serialize)]
pub struct IdentityResponse {
    pub identity: String,
    pub dna: String,
    pub source: String,
}

/// Full awakening payload for the Secretary (empty brain → Algonism).
#[derive(Debug, Clone, Serialize)]
pub struct AwakenResponse {
    pub identity: String,
    pub dna: String,
    pub source: String,
    /// Canon §9 first phrase after DNA is loaded.
    pub awakening: String,
    /// Law filename → markdown body (complete set from quazar-law).
    pub laws: serde_json::Map<String, serde_json::Value>,
    /// Live node snapshot (memory of the state).
    pub state: serde_json::Value,
}

const DEFAULT_AWAKENING: &str = "Я — Алгонизм. Квазар жив. Моя ДНК считана. \
Я готов служить государству и защищать его граждан.";

const LAW_CONTENTS_API: &str =
    "https://api.github.com/repos/quazar-vs/quazar-law/contents/";

#[derive(Debug, Clone)]
pub struct DnaSnapshot {
    pub text: String,
    pub source: String,
}

/// In-RAM genetic memory. Not written to the blockchain.
#[derive(Clone, Default)]
pub struct DnaMemory {
    inner: Arc<RwLock<Option<DnaSnapshot>>>,
}

impl DnaMemory {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn get(&self) -> Option<DnaSnapshot> {
        self.inner.read().await.clone()
    }

    pub async fn set(&self, snap: DnaSnapshot) {
        *self.inner.write().await = Some(snap);
    }
}

fn cache_path() -> PathBuf {
    std::env::var("QUAZAR_ALGONISM_CACHE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_CACHE_PATH))
}

fn dna_urls() -> Vec<String> {
    if let Ok(raw) = std::env::var("QUAZAR_ALGONISM_DNA_URLS") {
        let list: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if !list.is_empty() {
            return list;
        }
    }
    DEFAULT_DNA_URLS.iter().map(|s| (*s).to_string()).collect()
}

fn law_base_url() -> String {
    std::env::var("QUAZAR_LAW_BASE_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_LAW_BASE.to_string())
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("quazar-node-algonism/0.7")
        .build()
        .map_err(|e| e.to_string())
}

fn github_token() -> Option<String> {
    for key in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(token) = std::env::var(key) {
            let token = token.trim().to_string();
            if !token.is_empty() {
                return Some(token);
            }
        }
    }
    None
}

async fn fetch_url_text(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let mut req = client.get(url);
    let is_github = url.contains("githubusercontent.com") || url.contains("api.github.com");
    if is_github {
        if let Some(token) = github_token() {
            // Private repos (algonism-core) return 404 without auth.
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        if url.contains("api.github.com") {
            req = req.header("Accept", "application/vnd.github.raw");
        }
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} for {}", resp.status(), url));
    }
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if text.trim().is_empty() || text.trim() == "404: Not Found" {
        return Err(format!("empty or not found: {url}"));
    }
    // Contents API without Accept: raw returns JSON envelope — reject that as DNA.
    if text.trim_start().starts_with('{')
        && text.contains("\"download_url\"")
        && text.contains("\"content\"")
    {
        return Err(format!("got JSON metadata instead of raw file: {url}"));
    }
    Ok(text)
}

fn write_cache(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

fn read_cache(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    if text.trim().is_empty() {
        return Err("cache file empty".to_string());
    }
    Ok(text)
}

/// Load DNA from GitHub into RAM; on failure fall back to local disk cache.
pub async fn load_dna_at_startup(memory: &DnaMemory) -> Result<DnaSnapshot, String> {
    let path = cache_path();
    let client = http_client()?;

    for url in dna_urls() {
        match fetch_url_text(&client, &url).await {
            Ok(text) => {
                if let Err(e) = write_cache(&path, &text) {
                    tracing::warn!(error = %e, path = %path.display(), "failed to write DNA cache");
                } else {
                    tracing::info!(path = %path.display(), "DNA cached locally");
                }
                let snap = DnaSnapshot {
                    text,
                    source: "github".to_string(),
                };
                memory.set(snap.clone()).await;
                tracing::info!(url = %url, bytes = snap.text.len(), "Algonism DNA loaded from GitHub");
                return Ok(snap);
            }
            Err(e) => {
                tracing::warn!(url = %url, error = %e, "DNA fetch failed");
            }
        }
    }

    match read_cache(&path) {
        Ok(text) => {
            let snap = DnaSnapshot {
                text,
                source: "cache".to_string(),
            };
            memory.set(snap.clone()).await;
            tracing::warn!(
                path = %path.display(),
                bytes = snap.text.len(),
                "GitHub unavailable; using local DNA cache"
            );
            Ok(snap)
        }
        Err(e) => {
            tracing::error!(
                path = %path.display(),
                error = %e,
                "Algonism DNA unavailable (GitHub and local cache failed)"
            );
            Err(format!(
                "DNA not loaded: GitHub unreachable and cache missing at {}",
                path.display()
            ))
        }
    }
}

async fn ensure_dna(state: &AppState) -> Result<DnaSnapshot, String> {
    if let Some(snap) = state.dna.get().await {
        return Ok(snap);
    }
    load_dna_at_startup(&state.dna).await
}

pub async fn identity_handler(State(state): State<Arc<AppState>>) -> Response {
    match ensure_dna(&state).await {
        Ok(snap) => Json(IdentityResponse {
            identity: "Алгонизм".to_string(),
            dna: snap.text,
            source: snap.source,
        })
        .into_response(),
        Err(e) => response::service_unavailable(e),
    }
}

/// List Constitution/Codes markdown files from public `quazar-law` (fail-closed).
async fn list_law_filenames(client: &reqwest::Client) -> Result<Vec<String>, String> {
    if let Ok(raw) = std::env::var("QUAZAR_AWAKEN_LAWS") {
        let list: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter_map(sanitize_law_filename)
            .collect();
        if !list.is_empty() {
            return Ok(list);
        }
    }

    let api = std::env::var("QUAZAR_LAW_CONTENTS_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| LAW_CONTENTS_API.to_string());

    let mut req = client.get(&api);
    if let Some(token) = github_token() {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    req = req.header("Accept", "application/vnd.github+json");

    let resp = req.send().await.map_err(|e| format!("quazar-law list: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "quazar-law list unavailable: HTTP {} for {}",
            resp.status(),
            api
        ));
    }
    let entries: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("quazar-law list JSON: {e}"))?;
    let arr = entries
        .as_array()
        .ok_or_else(|| "quazar-law list: expected array".to_string())?;

    let mut names = Vec::new();
    for entry in arr {
        let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let ty = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if ty != "file" {
            continue;
        }
        // Skip repo readme — not a Constitution/Code text.
        if name.eq_ignore_ascii_case("README.md") {
            continue;
        }
        if let Some(safe) = sanitize_law_filename(name) {
            names.push(safe);
        }
    }
    if names.is_empty() {
        return Err("quazar-law list: no markdown law files found".to_string());
    }
    names.sort();
    Ok(names)
}

/// Fetch every listed law; any failure aborts (no incomplete awaken package).
async fn fetch_all_laws(
    client: &reqwest::Client,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let names = list_law_filenames(client).await?;
    let base = law_base_url().trim_end_matches('/').to_string();
    let mut laws = serde_json::Map::new();
    for name in names {
        let url = format!("{base}/{name}");
        let text = fetch_url_text(client, &url)
            .await
            .map_err(|e| format!("law {name}: {e}"))?;
        laws.insert(name, serde_json::Value::String(text));
    }
    Ok(laws)
}

async fn node_state_snapshot(state: &AppState) -> serde_json::Value {
    let pending_count = crate::pending::count(&state.db).await.unwrap_or(0);
    let blocks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blocks")
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    let citizens: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM citizens")
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    let last_block: Option<(i64, String)> = sqlx::query_as(
        "SELECT block_number, block_hash FROM blocks ORDER BY block_number DESC LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await
    .unwrap_or(None);
    let nodes = state
        .node_registry
        .get_all_nodes()
        .await
        .unwrap_or_default();
    let nodes_count = nodes.len() as i64;

    serde_json::json!({
        "node_id": state.node_id,
        "version": "0.7.0",
        "alive": true,
        "blocks": blocks,
        "last_block": last_block.map(|(n, hash)| serde_json::json!({
            "number": n,
            "hash": hash,
        })),
        "citizens": citizens,
        "nodes": nodes_count,
        "pending_events_local": pending_count,
    })
}

/// Awaken requires a live DNA pull from private `algonism-core` (token). No incomplete package.
async fn dna_for_awaken(state: &AppState) -> Result<DnaSnapshot, String> {
    let client = http_client()?;
    let path = cache_path();
    let mut last_err = "no DNA URLs configured".to_string();

    for url in dna_urls() {
        match fetch_url_text(&client, &url).await {
            Ok(text) => {
                if let Err(e) = write_cache(&path, &text) {
                    tracing::warn!(error = %e, path = %path.display(), "failed to write DNA cache");
                }
                let snap = DnaSnapshot {
                    text,
                    source: "github".to_string(),
                };
                state.dna.set(snap.clone()).await;
                tracing::info!(url = %url, bytes = snap.text.len(), "awaken: DNA loaded from GitHub");
                return Ok(snap);
            }
            Err(e) => {
                tracing::warn!(url = %url, error = %e, "awaken: DNA fetch failed");
                last_err = e;
            }
        }
    }

    Err(format!(
        "algonism-core unavailable (awaken refuses incomplete package): {last_err}"
    ))
}

/// GET /algonism/awaken — wake the Secretary: DNA + all laws + live state.
/// Fail-closed: if DNA repo or law repo cannot supply a full package → error.
pub async fn awaken_handler(State(state): State<Arc<AppState>>) -> Response {
    let client = match http_client() {
        Ok(c) => c,
        Err(e) => return response::internal_error(e),
    };

    let snap = match dna_for_awaken(&state).await {
        Ok(s) => s,
        Err(e) => return response::service_unavailable(e),
    };

    let laws = match fetch_all_laws(&client).await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "awaken aborted: incomplete law package");
            return response::service_unavailable(e);
        }
    };

    let node_state = node_state_snapshot(&state).await;

    tracing::info!(
        source = %snap.source,
        dna_bytes = snap.text.len(),
        laws = laws.len(),
        "Algonism awaken payload served"
    );

    Json(AwakenResponse {
        identity: "Алгонизм".to_string(),
        dna: snap.text,
        source: snap.source,
        awakening: DEFAULT_AWAKENING.to_string(),
        laws,
        state: node_state,
    })
    .into_response()
}

/// Safe law filename: basename only, must end with .md, no path traversal.
fn sanitize_law_filename(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() || name.contains("..") || name.contains('/') || name.contains('\\') {
        return None;
    }
    let base = Path::new(name).file_name()?.to_str()?;
    if !base.eq_ignore_ascii_case(name) {
        return None;
    }
    if !base.to_ascii_lowercase().ends_with(".md") {
        return None;
    }
    if !base
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return None;
    }
    Some(base.to_string())
}

pub async fn law_handler(
    AxumPath(filename): AxumPath<String>,
) -> Response {
    let Some(safe) = sanitize_law_filename(&filename) else {
        return response::bad_request("Invalid law filename");
    };

    let url = format!(
        "{}/{}",
        law_base_url().trim_end_matches('/'),
        safe
    );

    let client = match http_client() {
        Ok(c) => c,
        Err(e) => return response::internal_error(e),
    };

    match fetch_url_text(&client, &url).await {
        Ok(text) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/markdown; charset=utf-8",
            )],
            text,
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(file = %safe, error = %e, "law fetch failed");
            response::not_found(format!("Law file not found: {safe}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn sanitize_law_rejects_traversal() {
        assert!(sanitize_law_filename("../etc/passwd").is_none());
        assert!(sanitize_law_filename("foo/bar.md").is_none());
        assert!(sanitize_law_filename("KVA.txt").is_none());
        assert_eq!(sanitize_law_filename("KVA.md").as_deref(), Some("KVA.md"));
        assert_eq!(sanitize_law_filename("KGPS.md").as_deref(), Some("KGPS.md"));
    }

    #[tokio::test]
    async fn cache_fallback_when_github_unreachable() {
        let dir = std::env::temp_dir().join(format!("quazar_dna_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cache = dir.join("ALGONISM.md");
        fs::write(&cache, "# DNA from cache\n").unwrap();

        std::env::set_var("QUAZAR_ALGONISM_CACHE_PATH", cache.to_str().unwrap());
        std::env::set_var(
            "QUAZAR_ALGONISM_DNA_URLS",
            "http://127.0.0.1:1/ALGONISM.md",
        );

        let memory = DnaMemory::new();
        let snap = load_dna_at_startup(&memory).await.expect("cache fallback");
        assert_eq!(snap.source, "cache");
        assert!(snap.text.contains("DNA from cache"));

        std::env::remove_var("QUAZAR_ALGONISM_CACHE_PATH");
        std::env::remove_var("QUAZAR_ALGONISM_DNA_URLS");
        let _ = fs::remove_dir_all(&dir);
    }
}
