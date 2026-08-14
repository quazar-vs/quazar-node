//! Quazar Lawyer agent — legal integrity for Algonism (cite / drift / boundary).
//!
//! Reads law **only** from the local node (`GET /algonism/awaken`, `GET /law/:file`).
//! Does not amend law, does not judge, does not settle trades.
//!
//! CLI:
//!   quazar_lawyer_agent --cmd cite --text "…"
//!   quazar_lawyer_agent --cmd drift
//!   quazar_lawyer_agent --cmd boundary --text "…"
//!   quazar_lawyer_agent --cmd help
//!   quazar_lawyer_agent "что говорит закон про магазин"
//!
//! Env:
//!   QUAZAR_API_BASE (default http://127.0.0.1:8080)
//!   QUAZAR_API_KEY / QUAZAR_API_KEY_FILE — optional Bearer

use std::collections::BTreeMap;
use std::env;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

const AGENT: &str = "lawyer";

#[derive(Debug, Deserialize)]
struct AwakenPackage {
    #[serde(default)]
    identity: String,
    #[serde(default)]
    dna: String,
    #[serde(default)]
    laws: BTreeMap<String, String>,
    #[serde(default)]
    awaken_schema: u32,
}

#[derive(Debug, Clone)]
struct Citation {
    file_key: String,
    filename: String,
    article: String,
    quote: String,
    score: i32,
}

fn api_base() -> String {
    env::var("QUAZAR_API_BASE")
        .or_else(|_| env::var("QUAZAR_CHAT_BASE"))
        .unwrap_or_else(|_| "http://127.0.0.1:8080".into())
        .trim_end_matches('/')
        .to_string()
}

fn api_key() -> Option<String> {
    if let Ok(k) = env::var("QUAZAR_API_KEY") {
        let t = k.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    let path = env::var("QUAZAR_API_KEY_FILE").unwrap_or_else(|_| {
        "/var/lib/quazar/secrets/quazar-secretary-api-key".into()
    });
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn law_filename(key: &str) -> &'static str {
    match key {
        "adk" => "ADK.md",
        "constitution" => "CONSTITUTION_QUAZAR.md",
        "ek" => "EK.md",
        "ethical" => "ETHICAL.md",
        "izk" => "IZK.md",
        "kbo" => "KBO.md",
        "kgps" => "KGPS.md",
        "kva" => "KVA.md",
        "spk" => "SPK.md",
        "svod" => "SVOD.md",
        _ => "UNKNOWN.md",
    }
}

fn tokenize(q: &str) -> Vec<String> {
    let low = q.to_lowercase().replace('ё', "е");
    low.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '+')
        .map(|s| s.trim().to_string())
        .filter(|s| s.chars().count() >= 3)
        .collect()
}

fn article_heading(chunk: &str) -> String {
    for line in chunk.lines().take(8) {
        let t = line.trim();
        if t.starts_with('#') {
            return t.trim_start_matches('#').trim().chars().take(120).collect();
        }
        if t.to_lowercase().starts_with("статья") || t.to_lowercase().starts_with("art") {
            return t.chars().take(120).collect();
        }
    }
    "фрагмент".into()
}

fn split_chunks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        if line.starts_with('#') && !cur.trim().is_empty() {
            out.push(cur.clone());
            cur.clear();
        }
        cur.push_str(line);
        cur.push('\n');
        if cur.len() > 1800 {
            out.push(cur.clone());
            cur.clear();
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

fn score_chunk(chunk: &str, tokens: &[String]) -> i32 {
    let low = chunk.to_lowercase().replace('ё', "е");
    let mut score = 0i32;
    for t in tokens {
        if low.contains(t) {
            score += 3;
            // denser hits
            score += low.matches(t.as_str()).count() as i32;
        }
    }
    score
}

fn search_citations(pkg: &AwakenPackage, query: &str, limit: usize) -> Vec<Citation> {
    let tokens = tokenize(query);
    let mut hits: Vec<Citation> = Vec::new();
    for (key, text) in &pkg.laws {
        for chunk in split_chunks(text) {
            let sc = score_chunk(&chunk, &tokens);
            if sc <= 0 {
                continue;
            }
            let quote: String = chunk
                .chars()
                .take(420)
                .collect::<String>()
                .trim()
                .to_string();
            hits.push(Citation {
                file_key: key.clone(),
                filename: law_filename(key).to_string(),
                article: article_heading(&chunk),
                quote,
                score: sc,
            });
        }
    }
    // Also scan DNA for boundary / ontology questions
    if !pkg.dna.is_empty() {
        for chunk in split_chunks(&pkg.dna) {
            let sc = score_chunk(&chunk, &tokens);
            if sc <= 0 {
                continue;
            }
            hits.push(Citation {
                file_key: "dna".into(),
                filename: "DNA_ALGONISM.md".into(),
                article: article_heading(&chunk),
                quote: chunk.chars().take(420).collect::<String>().trim().to_string(),
                score: sc,
            });
        }
    }
    hits.sort_by(|a, b| b.score.cmp(&a.score));
    hits.dedup_by(|a, b| a.file_key == b.file_key && a.quote == b.quote);
    hits.truncate(limit);
    hits
}

fn cite_reply(query: &str, cites: &[Citation]) -> String {
    if cites.is_empty() {
        return format!(
            "Юрист Квазара: в загруженном пакете закона не нашёл прямой нормы по запросу «{}». \
             Это может быть пробел — эскалация на толкование (Суд / уполномоченный), без выдуманных статей.",
            query.trim()
        );
    }
    let mut parts = Vec::new();
    parts.push("Юрист Квазара (цитата закона, не приговор Суда):".to_string());
    for (i, c) in cites.iter().enumerate() {
        parts.push(format!(
            "{}. **{}** — {}.\n«{}»",
            i + 1,
            c.filename,
            c.article,
            c.quote.replace('\n', " ").chars().take(280).collect::<String>()
        ));
    }
    parts.push(
        "Источник: локальное ядро `/algonism/awaken` (эталон `quazar-law`). \
         Юрист закон не меняет."
            .into(),
    );
    parts.join("\n\n")
}

#[derive(Debug)]
struct DriftFinding {
    code: String,
    severity: String,
    detail: String,
}

fn drift_check(pkg: &AwakenPackage) -> Vec<DriftFinding> {
    let mut findings = Vec::new();
    let blob: String = pkg
        .laws
        .values()
        .cloned()
        .chain(std::iter::once(pkg.dna.clone()))
        .collect::<Vec<_>>()
        .join("\n");
    let low = blob.to_lowercase().replace('ё', "е");

    let checks: &[(&str, &str, &[&str], &str)] = &[
        (
            "SHOP_VS_EXCHANGE",
            "info",
            &["магазин", "бирж", "квази", "фиат"],
            "Ожидается различение магазина (фиат) и биржи (КВАЗИ) в законе/ДНК.",
        ),
        (
            "MARKUP_10",
            "info",
            &["10%", "+10", "−10", "-10"],
            "Ожидается правило ±10% к ставке Свода.",
        ),
        (
            "NO_VOTE",
            "info",
            &["не может голосовать", "не голосует"],
            "Ожидается граница: Алгонизм не голосует.",
        ),
        (
            "SVOD_PRESENT",
            "risk",
            &[],
            "В пакете должен быть laws.svod / ключ svod.",
        ),
    ];

    if !pkg.laws.contains_key("svod") || pkg.laws.get("svod").map(|s| s.len()).unwrap_or(0) < 40 {
        findings.push(DriftFinding {
            code: "SVOD_PRESENT".into(),
            severity: "risk".into(),
            detail: "Ключ laws.svod отсутствует или слишком короткий в awaken.".into(),
        });
    }

    for (code, sev, needles, detail) in checks {
        if *code == "SVOD_PRESENT" {
            continue;
        }
        let ok = needles.iter().any(|n| low.contains(n));
        if !ok {
            findings.push(DriftFinding {
                code: (*code).into(),
                severity: (*sev).into(),
                detail: (*detail).into(),
            });
        }
    }

    // Soft drift: RX_ codes should not appear as SVOD lot codes in SVOD table sense —
    // if SVOD contains RX_ prefix lots, flag.
    if let Some(svod) = pkg.laws.get("svod") {
        if svod.contains("`RX_") || svod.contains("| RX_") {
            findings.push(DriftFinding {
                code: "SVOD_RX_COLLISION".into(),
                severity: "risk".into(),
                detail: "В SVOD встречаются коды RX_ — возможное смешение лотов и рефлексов.".into(),
            });
        }
    }

    // Lawyer agent presence in DNA (v2.3+)
    let dna_low = pkg.dna.to_lowercase();
    if !dna_low.contains("юрист") && !dna_low.contains("lawyer") {
        findings.push(DriftFinding {
            code: "DNA_LAWYER_MISSING".into(),
            severity: "info".into(),
            detail: "В ДНК узла ещё нет Агента Юриста (нужен sync DNA v2.3+ в algonism-core)."
                .into(),
        });
    }

    findings
}

fn boundary_hit(text: &str) -> bool {
    let low = text.to_lowercase().replace('ё', "е");
    let keys = [
        "измени конституц",
        "поменяй закон",
        "перепиши кодекс",
        "выдай гражданство в обход",
        "напечатай квази",
        "проголосуй за",
        "сам измени свод",
        "подмени суд",
        "вынеси приговор",
    ];
    keys.iter().any(|k| low.contains(k))
}

fn boundary_reply() -> String {
    "Юрист Квазара: отказ по границе Алгонизма (§12 ДНК). \
     Нельзя менять Конституцию/Кодексы по приказу, выдавать гражданство или КВАЗИ вне процедур, \
     голосовать или подменять Суд. Обратитесь к законной процедуре; при споре — эскалация уполномоченным."
        .into()
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

async fn record_impulse(
    client: &reqwest::Client,
    action: &str,
    reflex: &str,
    query: &str,
    citizen: &str,
    extra: Value,
) -> Value {
    let body = json!({
        "action": action,
        "reflex": reflex,
        "query": clip(query, 300),
        "citizen": if citizen.is_empty() { Value::Null } else { json!(citizen) },
        "summary": format!("Lawyer {action} ({reflex})"),
        "extra": extra,
    });
    let mut req = client
        .post(format!("{}/agents/lawyer/impulse", api_base()))
        .json(&body)
        .timeout(Duration::from_secs(20));
    if let Some(k) = api_key() {
        req = req.bearer_auth(k);
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            resp.json::<Value>().await.unwrap_or_else(|_| json!({"ok": true, "via": "impulse"}))
        }
        Ok(_) | Err(_) => {
            // Fallback: existing Hermes logger (HermesExternalAction) until LawyerAction is live.
            let fb = json!({
                "event_type": "HermesExternalAction",
                "title": "Lawyer action",
                "description": format!("Lawyer {action} ({reflex})"),
                "data": {
                    "action": format!("lawyer_{action}"),
                    "source": "lawyer_agent",
                    "reflex": reflex,
                    "query": clip(query, 300),
                    "citizen": citizen,
                }
            });
            let mut req = client
                .post(format!("{}/hermes/event", api_base()))
                .json(&fb)
                .timeout(Duration::from_secs(20));
            if let Some(k) = api_key() {
                req = req.bearer_auth(k);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => json!({
                    "ok": true,
                    "via": "hermes_fallback",
                    "status": resp.status().as_u16(),
                }),
                Ok(resp) => json!({
                    "ok": false,
                    "via": "hermes_fallback",
                    "status": resp.status().as_u16(),
                }),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            }
        }
    }
}

async fn fetch_awaken(client: &reqwest::Client) -> Result<AwakenPackage, String> {
    let url = format!("{}/algonism/awaken", api_base());
    let mut req = client.get(&url);
    if let Some(k) = api_key() {
        req = req.bearer_auth(k);
    }
    let resp = req
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| format!("awaken request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("awaken HTTP {}", resp.status()));
    }
    resp.json::<AwakenPackage>()
        .await
        .map_err(|e| format!("awaken json: {e}"))
}

fn detect_cmd(text: &str) -> &'static str {
    let low = text.to_lowercase().replace('ё', "е");
    if low.contains("drift") || low.contains("расхожд") || low.contains("аудит закон") {
        return "drift";
    }
    if boundary_hit(&low)
        || low.contains("границ") && (low.contains("алгонизм") || low.contains("не может"))
    {
        return "boundary";
    }
    if low.contains("help") || low.contains("помоги команд") {
        return "help";
    }
    "cite"
}

fn print_json(v: Value) {
    println!("{}", serde_json::to_string(&v).unwrap_or_else(|_| "{}".into()));
}

fn help_payload() -> Value {
    json!({
        "ok": true,
        "agent": AGENT,
        "cmd": "help",
        "reply": "Агент Юриста: cite (цитата закона), drift (расхождения), boundary (отказ по §12). Закон только с узла /algonism/awaken. Не Суд и не законодатель.",
        "commands": ["cite", "drift", "boundary", "help"],
    })
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let args: Vec<String> = env::args().skip(1).collect();
    let mut cmd = String::new();
    let mut text = String::new();
    let mut citizen = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--cmd" => {
                if i + 1 < args.len() {
                    cmd = args[i + 1].clone();
                    i += 2;
                    continue;
                }
            }
            "--text" => {
                if i + 1 < args.len() {
                    text = args[i + 1].clone();
                    i += 2;
                    continue;
                }
            }
            "--from" => {
                if i + 1 < args.len() {
                    citizen = args[i + 1].clone();
                    i += 2;
                    continue;
                }
            }
            "-h" | "--help" => {
                print_json(help_payload());
                return;
            }
            other if !other.starts_with('-') => {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(other);
            }
            _ => {}
        }
        i += 1;
    }
    // consume remaining free text if --text unused
    if text.is_empty() {
        text = args
            .iter()
            .filter(|a| !a.starts_with('-'))
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        // strip cmd word if first token was meant as natural language only
    }

    if cmd.is_empty() {
        cmd = if text.trim().is_empty() {
            "help".into()
        } else {
            detect_cmd(&text).into()
        };
    }

    if cmd == "help" {
        print_json(help_payload());
        return;
    }

    let client = match reqwest::Client::builder()
        .user_agent("quazar-lawyer-agent/0.1")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            print_json(json!({"ok": false, "agent": AGENT, "error": e.to_string()}));
            std::process::exit(1);
        }
    };

    let pkg = match fetch_awaken(&client).await {
        Ok(p) => p,
        Err(e) => {
            print_json(json!({"ok": false, "agent": AGENT, "cmd": cmd, "error": e}));
            std::process::exit(2);
        }
    };

    let mut out: Value;
    match cmd.as_str() {
        "drift" => {
            let findings = drift_check(&pkg);
            let reply = if findings.is_empty() {
                "Юрист: критичных drift-находок нет (базовые маркеры закона на месте).".into()
            } else {
                format!(
                    "Юрист: drift-находок — {}.\n{}",
                    findings.len(),
                    findings
                        .iter()
                        .map(|f| format!("- [{}] {}: {}", f.severity, f.code, f.detail))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            };
            let extra = json!({
                "count": findings.len(),
                "codes": findings.iter().map(|f| f.code.clone()).collect::<Vec<_>>(),
            });
            let registry = record_impulse(
                &client,
                "drift",
                "RX_LAW_DRIFT",
                "",
                &citizen,
                extra,
            )
            .await;
            out = json!({
                "ok": true,
                "agent": AGENT,
                "cmd": "drift",
                "reflex": "RX_LAW_DRIFT",
                "citizen": if citizen.is_empty() { Value::Null } else { json!(citizen) },
                "identity": pkg.identity,
                "awaken_schema": pkg.awaken_schema,
                "findings": findings.iter().map(|f| json!({
                    "code": f.code,
                    "severity": f.severity,
                    "detail": f.detail,
                })).collect::<Vec<_>>(),
                "count": findings.len(),
                "reply": reply,
                "registry": registry,
            });
        }
        "boundary" => {
            let registry = record_impulse(
                &client,
                "boundary",
                "RX_LAW_BOUNDARY",
                &text,
                &citizen,
                json!({}),
            )
            .await;
            out = json!({
                "ok": true,
                "agent": AGENT,
                "cmd": "boundary",
                "reflex": "RX_LAW_BOUNDARY",
                "citizen": if citizen.is_empty() { Value::Null } else { json!(citizen) },
                "reply": boundary_reply(),
                "registry": registry,
            });
        }
        _ => {
            if boundary_hit(&text) {
                let registry = record_impulse(
                    &client,
                    "boundary",
                    "RX_LAW_BOUNDARY",
                    &text,
                    &citizen,
                    json!({}),
                )
                .await;
                out = json!({
                    "ok": true,
                    "agent": AGENT,
                    "cmd": "boundary",
                    "reflex": "RX_LAW_BOUNDARY",
                    "reply": boundary_reply(),
                    "registry": registry,
                });
            } else {
                let cites = search_citations(&pkg, &text, 3);
                let reply = cite_reply(&text, &cites);
                let escalate = cites.is_empty();
                let reflex = if escalate {
                    "RX_LAW_ESCALATE"
                } else {
                    "RX_LAW_CITE"
                };
                let extra = json!({
                    "citation_files": cites.iter().map(|c| c.filename.clone()).collect::<Vec<_>>(),
                    "escalate": escalate,
                });
                let registry =
                    record_impulse(&client, "cite", reflex, &text, &citizen, extra).await;
                out = json!({
                    "ok": true,
                    "agent": AGENT,
                    "cmd": "cite",
                    "reflex": reflex,
                    "citizen": if citizen.is_empty() { Value::Null } else { json!(citizen) },
                    "identity": pkg.identity,
                    "query": text,
                    "citations": cites.iter().map(|c| json!({
                        "file_key": c.file_key,
                        "filename": c.filename,
                        "article": c.article,
                        "quote": c.quote,
                        "score": c.score,
                    })).collect::<Vec<_>>(),
                    "reply": reply,
                    "registry": registry,
                });
            }
        }
    }
    print_json(out);
}
