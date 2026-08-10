//! Genetic memory (DNA) and law proxy for Algonism — identity source for the agent.
//!
//! DNA and laws are loaded from GitHub by the **node** only (poll / reload).
//! They are never written to the blockchain and never gossiped over P2P.
//! The Secretary receives the package exclusively via `GET /algonism/awaken`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Path as AxumPath, State},
    response::{IntoResponse, Json, Response},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::response;
use crate::AppState;

/// Contract version for `GET /algonism/awaken`.
pub const AWAKEN_SCHEMA: u32 = 1;

/// Candidate DNA sources. `algonism-core` is private — set `GITHUB_TOKEN`.
/// Verified layout (main): root `ALGONISM.md`, `DNA_ALGONISM.md`, `README.md`.
const DEFAULT_DNA_URLS: &[&str] = &[
    "https://raw.githubusercontent.com/quazar-vs/algonism-core/main/ALGONISM.md",
    "https://raw.githubusercontent.com/quazar-vs/algonism-core/main/DNA_ALGONISM.md",
    "https://api.github.com/repos/quazar-vs/algonism-core/contents/ALGONISM.md",
    "https://api.github.com/repos/quazar-vs/algonism-core/contents/DNA_ALGONISM.md",
    "https://raw.githubusercontent.com/quazar-vs/algonism-core/master/ALGONISM.md",
    "https://raw.githubusercontent.com/quazar-vs/algonism-core/master/DNA_ALGONISM.md",
];

const DEFAULT_LAW_BASE: &str =
    "https://raw.githubusercontent.com/quazar-vs/quazar-law/main";

const DEFAULT_CACHE_PATH: &str = "/var/lib/quazar/algonism/ALGONISM.md";
const DEFAULT_LAWS_CACHE_PATH: &str = "/var/lib/quazar/algonism/laws.json";
const DEFAULT_POLL_SECS: u64 = 120;

/// Fixed mapping: GitHub filename → short awaken key.
const LAW_KEY_FILES: &[(&str, &str)] = &[
    ("constitution", "CONSTITUTION_QUAZAR.md"),
    ("ek", "EK.md"),
    ("ethical", "ETHICAL.md"),
    ("kbo", "KBO.md"),
    ("kgps", "KGPS.md"),
    ("kva", "KVA.md"),
    ("svod", "SVOD.md"),
];

#[derive(Debug, Clone, Serialize)]
pub struct IdentityResponse {
    pub identity: String,
    pub dna: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LastBlockInfo {
    pub number: i64,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AwakenState {
    pub last_block: Option<LastBlockInfo>,
    pub active_citizens: i64,
    pub active_nodes: i64,
}

/// Full awakening payload for the Secretary (empty brain → Algonism).
#[derive(Debug, Clone, Serialize)]
pub struct AwakenResponse {
    pub awaken_schema: u32,
    pub identity: String,
    pub dna: String,
    pub dna_sha: String,
    pub source: String,
    pub laws: BTreeMap<String, String>,
    pub laws_sha: String,
    pub state: AwakenState,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReloadResponse {
    pub ok: bool,
    pub source: String,
    pub dna_sha: String,
    pub laws_sha: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone)]
pub struct DnaSnapshot {
    pub text: String,
    pub source: String,
}

/// In-RAM genetic + law package (incl. SVOD.md as `laws.svod`). Not on-chain / P2P.
#[derive(Debug, Clone)]
pub struct ReferencePackage {
    pub dna: String,
    pub dna_sha: String,
    pub source: String,
    pub laws: BTreeMap<String, String>,
    pub laws_sha: String,
    pub timestamp: i64,
}

#[derive(Clone, Default)]
pub struct DnaMemory {
    inner: Arc<RwLock<Option<ReferencePackage>>>,
}

impl DnaMemory {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn get_package(&self) -> Option<ReferencePackage> {
        self.inner.read().await.clone()
    }

    pub async fn get(&self) -> Option<DnaSnapshot> {
        self.inner.read().await.as_ref().map(|p| DnaSnapshot {
            text: p.dna.clone(),
            source: p.source.clone(),
        })
    }

    pub async fn set_package(&self, pkg: ReferencePackage) {
        *self.inner.write().await = Some(pkg);
    }

    pub async fn set(&self, snap: DnaSnapshot) {
        let mut guard = self.inner.write().await;
        if let Some(existing) = guard.as_mut() {
            existing.dna = snap.text;
            existing.dna_sha = sha256_hex(&existing.dna);
            existing.source = snap.source;
            existing.timestamp = unix_now();
        } else {
            *guard = Some(ReferencePackage {
                dna_sha: sha256_hex(&snap.text),
                dna: snap.text,
                source: snap.source,
                laws: BTreeMap::new(),
                laws_sha: sha256_hex(""),
                timestamp: unix_now(),
            });
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

/// `laws_sha` = SHA-256(hex) of UTF-8 concatenation `key\ntext\n` for sorted keys.
pub fn compute_laws_sha(laws: &BTreeMap<String, String>) -> String {
    let mut buf = String::new();
    for (key, text) in laws {
        buf.push_str(key);
        buf.push('\n');
        buf.push_str(text);
        buf.push('\n');
    }
    sha256_hex(&buf)
}

fn cache_path() -> PathBuf {
    std::env::var("QUAZAR_ALGONISM_CACHE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_CACHE_PATH))
}

fn laws_cache_path() -> PathBuf {
    std::env::var("QUAZAR_ALGONISM_LAWS_CACHE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_LAWS_CACHE_PATH))
}

fn svod_seed_path() -> Option<PathBuf> {
    std::env::var("QUAZAR_SVOD_SEED_PATH")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from("/var/lib/quazar/algonism/SVOD.md")))
}

fn poll_interval_secs() -> u64 {
    let raw = std::env::var("QUAZAR_ALGONISM_POLL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_POLL_SECS);
    raw.clamp(60, 300)
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

fn write_laws_cache(path: &Path, laws: &BTreeMap<String, String>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(laws).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

fn read_laws_cache(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let map: BTreeMap<String, String> =
        serde_json::from_str(&text).map_err(|e| format!("laws cache JSON: {e}"))?;
    if map.is_empty() {
        return Err("laws cache empty".to_string());
    }
    Ok(map)
}

fn filename_to_law_key(filename: &str) -> Option<&'static str> {
    for (key, file) in LAW_KEY_FILES {
        if file.eq_ignore_ascii_case(filename) {
            return Some(*key);
        }
    }
    None
}

fn build_package(
    dna: String,
    source: String,
    laws: BTreeMap<String, String>,
) -> ReferencePackage {
    let dna_sha = sha256_hex(&dna);
    let laws_sha = compute_laws_sha(&laws);
    ReferencePackage {
        dna,
        dna_sha,
        source,
        laws,
        laws_sha,
        timestamp: unix_now(),
    }
}

async fn fetch_dna_text(client: &reqwest::Client) -> Result<String, String> {
    let mut last_err = "no DNA URLs configured".to_string();
    for url in dna_urls() {
        match fetch_url_text(client, &url).await {
            Ok(text) => return Ok(text),
            Err(e) => {
                tracing::warn!(url = %url, error = %e, "DNA fetch failed");
                last_err = e;
            }
        }
    }
    Err(last_err)
}

/// Fetch the fixed law set with short keys (includes `svod` ← `SVOD.md`).
async fn fetch_mapped_laws(
    client: &reqwest::Client,
) -> Result<BTreeMap<String, String>, String> {
    let base = law_base_url().trim_end_matches('/').to_string();
    let mut laws = BTreeMap::new();
    for (key, filename) in LAW_KEY_FILES {
        let url = format!("{base}/{filename}");
        match fetch_url_text(client, &url).await {
            Ok(text) => {
                laws.insert((*key).to_string(), text);
            }
            Err(e) if *key == "svod" => {
                // Bootstrap until SVOD.md is published to quazar-law.
                let mut loaded = None;
                if let Some(seed) = svod_seed_path() {
                    if let Ok(text) = read_cache(&seed) {
                        tracing::warn!(
                            error = %e,
                            seed = %seed.display(),
                            "SVOD.md GitHub fetch failed; using QUAZAR_SVOD_SEED_PATH"
                        );
                        loaded = Some(text);
                    }
                }
                match loaded {
                    Some(text) => {
                        laws.insert((*key).to_string(), text);
                    }
                    None => {
                        return Err(format!("law {filename} ({key}): {e}"));
                    }
                }
            }
            Err(e) => {
                return Err(format!("law {filename} ({key}): {e}"));
            }
        }
    }
    if !laws.contains_key("svod") || laws.get("svod").map(|s| s.trim().is_empty()).unwrap_or(true)
    {
        return Err("law package missing SVOD.md (laws.svod)".to_string());
    }
    Ok(laws)
}

fn persist_package(pkg: &ReferencePackage) {
    let dna_path = cache_path();
    if let Err(e) = write_cache(&dna_path, &pkg.dna) {
        tracing::warn!(error = %e, path = %dna_path.display(), "failed to write DNA cache");
    }
    let laws_path = laws_cache_path();
    if let Err(e) = write_laws_cache(&laws_path, &pkg.laws) {
        tracing::warn!(error = %e, path = %laws_path.display(), "failed to write laws cache");
    }
    // Also mirror SVOD.md next to DNA cache for seed/bootstrap.
    if let Some(svod) = pkg.laws.get("svod") {
        let path = PathBuf::from("/var/lib/quazar/algonism/SVOD.md");
        if let Err(e) = write_cache(&path, svod) {
            tracing::warn!(error = %e, path = %path.display(), "failed to write SVOD.md cache");
        }
    }
}

/// Extract machine JSON from SVOD.md (fenced ```json block) or raw JSON body.
fn extract_svod_machine_json(svod_md: &str) -> Option<String> {
    let trimmed = svod_md.trim();
    if trimmed.starts_with('{') {
        return Some(trimmed.to_string());
    }
    let start = trimmed.find("```json")?;
    let after = &trimmed[start + 7..];
    let end = after.find("```")?;
    let body = after[..end].trim();
    if body.starts_with('{') {
        Some(body.to_string())
    } else {
        None
    }
}

async fn apply_svod_if_present(pool: &sqlx::PgPool, pkg: &ReferencePackage) {
    let Some(svod_md) = pkg.laws.get("svod") else {
        return;
    };
    let Some(json) = extract_svod_machine_json(svod_md) else {
        tracing::warn!("SVOD.md has no machine JSON block; skipping Postgres apply");
        return;
    };
    if let Err(e) = crate::svod::apply_svod_etalon_json(pool, &json).await {
        tracing::error!(error = %e, "failed to apply SVOD etalon into Postgres");
    }
}

/// Force-load DNA + laws (incl. SVOD.md) from GitHub into RAM/disk. Sets `source=github`.
pub async fn reload_from_github(
    memory: &DnaMemory,
    pool: &sqlx::PgPool,
) -> Result<ReferencePackage, String> {
    let client = http_client()?;
    let dna = fetch_dna_text(&client).await?;
    let laws = fetch_mapped_laws(&client).await?;
    let pkg = build_package(dna, "github".to_string(), laws);
    persist_package(&pkg);
    apply_svod_if_present(pool, &pkg).await;
    memory.set_package(pkg.clone()).await;
    tracing::info!(
        dna_sha = %pkg.dna_sha,
        laws_sha = %pkg.laws_sha,
        laws = pkg.laws.len(),
        has_svod = pkg.laws.contains_key("svod"),
        "Algonism reference (DNA/laws/SVOD) reloaded from GitHub"
    );
    Ok(pkg)
}

/// Load DNA (+ laws incl. SVOD) at startup: GitHub first, then local cache.
pub async fn load_dna_at_startup(
    memory: &DnaMemory,
    pool: &sqlx::PgPool,
) -> Result<DnaSnapshot, String> {
    match reload_from_github(memory, pool).await {
        Ok(pkg) => {
            tracing::info!(
                bytes = pkg.dna.len(),
                dna_sha = %pkg.dna_sha,
                "Algonism DNA/laws/SVOD loaded from GitHub"
            );
            return Ok(DnaSnapshot {
                text: pkg.dna,
                source: pkg.source,
            });
        }
        Err(e) => {
            tracing::warn!(error = %e, "GitHub reference reload failed at startup");
        }
    }

    let dna_path = cache_path();
    let laws_path = laws_cache_path();
    let dna = read_cache(&dna_path).map_err(|e| {
        format!(
            "DNA not loaded: GitHub unreachable and cache missing at {}: {e}",
            dna_path.display()
        )
    })?;
    let mut laws = read_laws_cache(&laws_path).unwrap_or_default();
    if !laws.contains_key("svod") {
        if let Some(seed) = svod_seed_path() {
            if let Ok(text) = read_cache(&seed) {
                laws.insert("svod".to_string(), text);
            }
        }
    }
    let pkg = build_package(dna, "cache".to_string(), laws);
    apply_svod_if_present(pool, &pkg).await;
    memory.set_package(pkg.clone()).await;
    tracing::warn!(
        path = %dna_path.display(),
        bytes = pkg.dna.len(),
        dna_sha = %pkg.dna_sha,
        "GitHub unavailable; using local DNA/laws/SVOD cache"
    );
    Ok(DnaSnapshot {
        text: pkg.dna,
        source: pkg.source,
    })
}

/// Background poll: every 60–300s compare GitHub content hashes; reload on change.
pub fn spawn_reference_poller(state: Arc<AppState>) {
    let interval = poll_interval_secs();
    tracing::info!(interval_secs = interval, "Algonism reference poller started");
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            if let Err(e) = poll_once(&state).await {
                tracing::warn!(error = %e, "Algonism poll cycle failed");
            }
        }
    });
}

async fn poll_once(state: &AppState) -> Result<(), String> {
    let client = http_client()?;
    let dna = match fetch_dna_text(&client).await {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!(error = %e, "poll: DNA unreachable (keeping cache)");
            return Ok(());
        }
    };
    let laws = match fetch_mapped_laws(&client).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "poll: laws incomplete (keeping cache)");
            return Ok(());
        }
    };
    let new_dna_sha = sha256_hex(&dna);
    let new_laws_sha = compute_laws_sha(&laws);

    if let Some(cur) = state.dna.get_package().await {
        if cur.dna_sha == new_dna_sha && cur.laws_sha == new_laws_sha {
            tracing::debug!(dna_sha = %new_dna_sha, "poll: reference unchanged");
            return Ok(());
        }
        tracing::info!(
            old_dna_sha = %cur.dna_sha,
            new_dna_sha = %new_dna_sha,
            old_laws_sha = %cur.laws_sha,
            new_laws_sha = %new_laws_sha,
            "poll: reference changed — reloading cache"
        );
    } else {
        tracing::info!(dna_sha = %new_dna_sha, "poll: no package in RAM — loading");
    }

    let pkg = build_package(dna, "github".to_string(), laws);
    persist_package(&pkg);
    apply_svod_if_present(&state.db, &pkg).await;
    state.dna.set_package(pkg).await;
    Ok(())
}

async fn ensure_package(state: &AppState) -> Result<ReferencePackage, String> {
    if let Some(pkg) = state.dna.get_package().await {
        if !pkg.dna.trim().is_empty()
            && !pkg.laws.is_empty()
            && pkg.laws.get("svod").map(|s| !s.trim().is_empty()).unwrap_or(false)
        {
            return Ok(pkg);
        }
    }
    match reload_from_github(&state.dna, &state.db).await {
        Ok(pkg) => Ok(pkg),
        Err(gh_err) => {
            let dna_path = cache_path();
            let laws_path = laws_cache_path();
            let dna = read_cache(&dna_path).map_err(|_| gh_err.clone())?;
            let mut laws = read_laws_cache(&laws_path).map_err(|_| {
                format!("incomplete package: github={gh_err}; laws cache missing")
            })?;
            if !laws.contains_key("svod") {
                if let Some(seed) = svod_seed_path() {
                    if let Ok(text) = read_cache(&seed) {
                        laws.insert("svod".to_string(), text);
                    }
                }
            }
            if laws.is_empty()
                || laws.get("svod").map(|s| s.trim().is_empty()).unwrap_or(true)
            {
                return Err(format!("incomplete package: github={gh_err}; missing laws.svod"));
            }
            let pkg = build_package(dna, "cache".to_string(), laws);
            apply_svod_if_present(&state.db, &pkg).await;
            state.dna.set_package(pkg.clone()).await;
            Ok(pkg)
        }
    }
}

pub async fn identity_handler(State(state): State<Arc<AppState>>) -> Response {
    match ensure_package(&state).await {
        Ok(pkg) => Json(IdentityResponse {
            identity: "Алгонизм".to_string(),
            dna: pkg.dna,
            source: pkg.source,
        })
        .into_response(),
        Err(e) => response::service_unavailable(e),
    }
}

async fn awaken_state_snapshot(state: &AppState) -> AwakenState {
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

    AwakenState {
        last_block: last_block.map(|(number, hash)| LastBlockInfo { number, hash }),
        active_citizens: citizens,
        active_nodes: nodes_count,
    }
}

/// GET /algonism/awaken — wake the Secretary from the local core package.
pub async fn awaken_handler(State(state): State<Arc<AppState>>) -> Response {
    let pkg = match ensure_package(&state).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "awaken aborted: incomplete package");
            return response::service_unavailable(e);
        }
    };

    if pkg.laws.is_empty() {
        return response::service_unavailable("awaken refuses incomplete law package");
    }
    if pkg
        .laws
        .get("svod")
        .map(|s| s.trim().is_empty())
        .unwrap_or(true)
    {
        return response::service_unavailable("awaken refuses incomplete package: missing laws.svod");
    }

    let node_state = awaken_state_snapshot(&state).await;

    tracing::info!(
        schema = AWAKEN_SCHEMA,
        source = %pkg.source,
        dna_sha = %pkg.dna_sha,
        laws_sha = %pkg.laws_sha,
        dna_bytes = pkg.dna.len(),
        laws = pkg.laws.len(),
        "Algonism awaken payload served"
    );

    Json(AwakenResponse {
        awaken_schema: AWAKEN_SCHEMA,
        identity: "Алгонизм".to_string(),
        dna: pkg.dna,
        dna_sha: pkg.dna_sha,
        source: pkg.source,
        laws: pkg.laws,
        laws_sha: pkg.laws_sha,
        state: node_state,
        timestamp: pkg.timestamp,
    })
    .into_response()
}

/// POST /algonism/dna/reload — force GitHub → node cache (auth required).
pub async fn reload_handler(State(state): State<Arc<AppState>>) -> Response {
    match reload_from_github(&state.dna, &state.db).await {
        Ok(pkg) => Json(ReloadResponse {
            ok: true,
            source: pkg.source,
            dna_sha: pkg.dna_sha,
            laws_sha: pkg.laws_sha,
            timestamp: pkg.timestamp,
        })
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "DNA/laws/SVOD reload failed");
            response::service_unavailable(e)
        }
    }
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
    State(state): State<Arc<AppState>>,
    AxumPath(filename): AxumPath<String>,
) -> Response {
    let Some(safe) = sanitize_law_filename(&filename) else {
        return response::bad_request("Invalid law filename");
    };

    // Prefer short-key cache when filename maps to an awaken key.
    if let Some(key) = filename_to_law_key(&safe) {
        if let Some(pkg) = state.dna.get_package().await {
            if let Some(text) = pkg.laws.get(key) {
                return (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/markdown; charset=utf-8",
                    )],
                    text.clone(),
                )
                    .into_response();
            }
        }
    }

    let url = format!("{}/{}", law_base_url().trim_end_matches('/'), safe);

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

    #[test]
    fn law_filename_maps_to_short_keys() {
        assert_eq!(filename_to_law_key("CONSTITUTION_QUAZAR.md"), Some("constitution"));
        assert_eq!(filename_to_law_key("KVA.md"), Some("kva"));
        assert_eq!(filename_to_law_key("ETHICAL.md"), Some("ethical"));
        assert_eq!(filename_to_law_key("SVOD.md"), Some("svod"));
        assert_eq!(filename_to_law_key("README.md"), None);
    }

    #[test]
    fn extract_svod_json_from_markdown_fence() {
        let md = "# Svod\n\n```json\n{\"version\":1,\"items\":[],\"rates\":[]}\n```\n";
        let json = extract_svod_machine_json(md).expect("json");
        assert!(json.contains("\"version\":1"));
    }

    #[test]
    fn laws_sha_is_sorted_key_text_concat() {
        let mut laws = BTreeMap::new();
        laws.insert("kva".to_string(), "A".to_string());
        laws.insert("constitution".to_string(), "B".to_string());
        // Sorted keys: constitution, kva → "constitution\nB\nkva\nA\n"
        let expected = sha256_hex("constitution\nB\nkva\nA\n");
        assert_eq!(compute_laws_sha(&laws), expected);
    }

    #[test]
    fn dna_sha_is_utf8_sha256() {
        let mut h = Sha256::new();
        h.update("hello".as_bytes());
        assert_eq!(sha256_hex("hello"), hex::encode(h.finalize()));

        let mut h2 = Sha256::new();
        h2.update("Алгонизм".as_bytes());
        assert_eq!(sha256_hex("Алгонизм"), hex::encode(h2.finalize()));
    }

    #[tokio::test]
    async fn cache_fallback_when_github_unreachable() {
        let dir = std::env::temp_dir().join(format!("quazar_dna_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cache = dir.join("ALGONISM.md");
        let laws_cache = dir.join("laws.json");
        fs::write(&cache, "# DNA from cache\n").unwrap();
        let mut laws = BTreeMap::new();
        laws.insert("constitution".to_string(), "# C\n".to_string());
        laws.insert(
            "svod".to_string(),
            "# Svod\n\n```json\n{\"version\":1,\"items\":[],\"rates\":[]}\n```\n".to_string(),
        );
        fs::write(&laws_cache, serde_json::to_string(&laws).unwrap()).unwrap();

        let memory = DnaMemory::new();
        let dna = fs::read_to_string(&cache).unwrap();
        let laws = read_laws_cache(&laws_cache).unwrap();
        let pkg = build_package(dna, "cache".to_string(), laws);
        memory.set_package(pkg).await;
        let snap = memory.get().await.expect("snap");
        assert_eq!(snap.source, "cache");
        assert!(snap.text.contains("DNA from cache"));
        let pkg = memory.get_package().await.expect("package");
        assert_eq!(pkg.laws.get("constitution").map(String::as_str), Some("# C\n"));
        assert!(pkg.laws.get("svod").unwrap().contains("Svod"));

        let _ = fs::remove_dir_all(&dir);
    }
}
