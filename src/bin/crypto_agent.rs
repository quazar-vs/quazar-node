//! Crypto market watcher agent (public market data only — no trading).
//!
//! Polls Binance spot tickers and logs prices; optionally alerts on % move.
//!
//! Env:
//! - `CRYPTO_AGENT_SYMBOLS` — comma-separated pairs (default `BTCUSDT,ETHUSDT`)
//! - `CRYPTO_AGENT_INTERVAL_SECS` — poll interval (default `30`)
//! - `CRYPTO_AGENT_ALERT_PCT` — alert if |Δ| ≥ this % since last sample (default `2.0`; `0` disables)
//! - `CRYPTO_AGENT_EXCHANGE` — currently only `binance` (default)

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Deserialize)]
struct BinanceTicker {
    symbol: String,
    #[serde(rename = "lastPrice")]
    last_price: String,
    #[serde(rename = "priceChangePercent")]
    price_change_percent: String,
    #[serde(rename = "highPrice")]
    high_price: String,
    #[serde(rename = "lowPrice")]
    low_price: String,
    #[serde(rename = "volume")]
    volume: String,
}

fn env_symbols() -> Vec<String> {
    std::env::var("CRYPTO_AGENT_SYMBOLS")
        .unwrap_or_else(|_| "BTCUSDT,ETHUSDT".to_string())
        .split(',')
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn env_interval() -> Duration {
    let secs: u64 = std::env::var("CRYPTO_AGENT_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
        .max(5);
    Duration::from_secs(secs)
}

fn env_alert_pct() -> f64 {
    std::env::var("CRYPTO_AGENT_ALERT_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2.0)
}

async fn fetch_ticker(client: &reqwest::Client, symbol: &str) -> Result<BinanceTicker, String> {
    let url = format!(
        "https://api.binance.com/api/v3/ticker/24hr?symbol={}",
        symbol
    );
    let resp = client
        .get(&url)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {body}"));
    }
    resp.json::<BinanceTicker>()
        .await
        .map_err(|e| format!("json: {e}"))
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let exchange = std::env::var("CRYPTO_AGENT_EXCHANGE").unwrap_or_else(|_| "binance".into());
    if exchange.to_lowercase() != "binance" {
        error!(%exchange, "only binance is supported in this MVP");
        std::process::exit(1);
    }

    let symbols = env_symbols();
    if symbols.is_empty() {
        error!("CRYPTO_AGENT_SYMBOLS is empty");
        std::process::exit(1);
    }
    let interval = env_interval();
    let alert_pct = env_alert_pct();

    info!(
        exchange = %exchange,
        ?symbols,
        interval_secs = interval.as_secs(),
        alert_pct,
        "crypto market agent starting (public data only, no trading)"
    );

    let client = reqwest::Client::builder()
        .user_agent("quazar-crypto-agent/0.1")
        .build()
        .expect("http client");

    let mut last_prices: HashMap<String, f64> = HashMap::new();

    loop {
        for symbol in &symbols {
            match fetch_ticker(&client, symbol).await {
                Ok(t) => {
                    let price: f64 = t.last_price.parse().unwrap_or(f64::NAN);
                    let day_pct: f64 = t.price_change_percent.parse().unwrap_or(f64::NAN);

                    info!(
                        symbol = %t.symbol,
                        last = %t.last_price,
                        change_24h_pct = %t.price_change_percent,
                        high = %t.high_price,
                        low = %t.low_price,
                        volume = %t.volume,
                        "ticker"
                    );

                    if alert_pct > 0.0 && price.is_finite() {
                        if let Some(prev) = last_prices.get(symbol).copied() {
                            if prev > 0.0 {
                                let move_pct = ((price - prev) / prev) * 100.0;
                                if move_pct.abs() >= alert_pct {
                                    warn!(
                                        symbol = %symbol,
                                        prev,
                                        price,
                                        move_pct,
                                        threshold_pct = alert_pct,
                                        day_pct,
                                        "ALERT: price move since last sample"
                                    );
                                }
                            }
                        }
                        last_prices.insert(symbol.clone(), price);
                    }
                }
                Err(e) => {
                    warn!(symbol = %symbol, error = %e, "ticker fetch failed");
                }
            }
        }
        tokio::time::sleep(interval).await;
    }
}
