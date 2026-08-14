//! Pick a liquid, volatile USDT spot market before opening a new cycle.

use crate::config::BotConfig;
use crate::exchange::CoinExClient;
use anyhow::Result;
use serde_json::Value;
use tracing::{debug, info, warn};

const STABLE_BASES: &[&str] = &[
    "USDT", "USDC", "DAI", "TUSD", "FDUSD", "BUSD", "USDD", "USD1", "PYUSD",
];

#[derive(Debug, Clone)]
pub struct MarketPick {
    pub market: String,
    pub last: f64,
    pub vol: f64,
    pub spread_pct: f64,
    pub turnover: f64,
}

struct TickerCand {
    market: String,
    last: f64,
    vol: f64,
    turnover: f64,
}

pub async fn select_spot_market(
    client: &CoinExClient,
    cfg: &BotConfig,
) -> Result<Option<MarketPick>> {
    let tickers: Vec<Value> = client.get_spot_tickers().await?;
    let quote = cfg.scanner_quote.as_str();
    let mut cands: Vec<TickerCand> = Vec::new();

    for item in &tickers {
        let market = item
            .get("market")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_uppercase();
        if market.is_empty() || !market.ends_with(quote) {
            continue;
        }
        if cfg.market_blacklist.iter().any(|b| b == &market) {
            continue;
        }
        let base = market.strip_suffix(quote).unwrap_or("");
        if STABLE_BASES.iter().any(|s| *s == base) {
            continue;
        }
        let last = parse_num(item, "last").or_else(|| parse_num(item, "close")).unwrap_or(0.0);
        let high = parse_num(item, "high").unwrap_or(0.0);
        let low = parse_num(item, "low").unwrap_or(0.0);
        let turnover = parse_num(item, "value").unwrap_or(0.0);
        if last <= 0.0 || low <= 0.0 || high <= 0.0 {
            continue;
        }
        if turnover + f64::EPSILON < cfg.min_turnover_usdt {
            continue;
        }
        let vol = (high - low) / low;
        if vol <= 0.0 {
            continue;
        }
        cands.push(TickerCand {
            market,
            last,
            vol,
            turnover,
        });
    }

    cands.sort_by(|a, b| b.vol.partial_cmp(&a.vol).unwrap_or(std::cmp::Ordering::Equal));
    let top: Vec<TickerCand> = cands.into_iter().take(15).collect();
    if top.is_empty() {
        warn!("scanner: no ticker candidates after volume/quote filter");
        return Ok(None);
    }

    let mut best: Option<MarketPick> = None;
    for cand in &top {
        match client.get_spot_depth(&cand.market).await {
            Ok(depth) => {
                let Some(spread_pct) = spread_pct_from_depth(&depth) else {
                    debug!(market = %cand.market, "scanner: empty book — skip");
                    continue;
                };
                if spread_pct > cfg.max_spread_pct + f64::EPSILON {
                    debug!(
                        market = %cand.market,
                        spread_pct,
                        max = cfg.max_spread_pct,
                        "scanner: spread too wide — skip"
                    );
                    continue;
                }
                let pick = MarketPick {
                    market: cand.market.clone(),
                    last: cand.last,
                    vol: cand.vol,
                    spread_pct,
                    turnover: cand.turnover,
                };
                let better = match &best {
                    None => true,
                    Some(cur) => {
                        pick.vol > cur.vol + 1e-12
                            || ((pick.vol - cur.vol).abs() <= 1e-12 && pick.spread_pct < cur.spread_pct)
                    }
                };
                if better {
                    best = Some(pick);
                }
            }
            Err(e) => {
                debug!(error = %e, market = %cand.market, "scanner: depth failed — skip");
            }
        }
    }

    if let Some(ref pick) = best {
        info!(
            market = %pick.market,
            vol_pct = pick.vol * 100.0,
            spread_pct = pick.spread_pct,
            turnover = pick.turnover,
            last = pick.last,
            "scanner: selected spot market"
        );
    } else {
        warn!("scanner: no candidate passed spread filter");
    }
    Ok(best)
}

fn spread_pct_from_depth(data: &Value) -> Option<f64> {
    let depth = data.get("depth").unwrap_or(data);
    let ask = first_px(depth.get("asks")?)?;
    let bid = first_px(depth.get("bids")?)?;
    if ask <= 0.0 || bid <= 0.0 || ask < bid {
        return None;
    }
    let mid = (ask + bid) / 2.0;
    if mid <= 0.0 {
        return None;
    }
    Some((ask - bid) / mid * 100.0)
}

fn first_px(levels: &Value) -> Option<f64> {
    let row = levels.as_array()?.first()?;
    match row {
        Value::Array(pair) => pair.first()?.as_str()?.parse().ok(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn parse_num(v: &Value, key: &str) -> Option<f64> {
    let n = v.get(key)?;
    if let Some(s) = n.as_str() {
        return s.parse().ok();
    }
    n.as_f64()
}
