//! Pick a liquid USDT spot market that is chopping, not pumping/dumping.
//!
//! 0) Ticker shortlist by 24h turnover (not max range), then tight spread.
//! 1) 12hour × N: pair has been alive (range spread across bars, not one spike).
//! 2) 1hour × N: reject one-sided dump/pump (net move and close not glued to extremes).
//! 3) 5min × N: on bearish bars, lower wick > body; bullish bars unrestricted.

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
    pub hour_ratio: f64,
}

struct TickerCand {
    market: String,
    last: f64,
    vol: f64,
    turnover: f64,
}

#[derive(Debug, Clone, Copy)]
struct Bar {
    open: f64,
    high: f64,
    low: f64,
    close: f64,
}

impl Bar {
    fn range(&self) -> f64 {
        (self.high - self.low).max(0.0)
    }

    fn body(&self) -> f64 {
        (self.close - self.open).abs()
    }

    fn is_bearish(&self) -> bool {
        self.close + f64::EPSILON < self.open
    }

    fn lower_wick(&self) -> f64 {
        self.open.min(self.close) - self.low
    }
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
        let last = parse_num(item, "last")
            .or_else(|| parse_num(item, "close"))
            .unwrap_or(0.0);
        let high = parse_num(item, "high").unwrap_or(0.0);
        let low = parse_num(item, "low").unwrap_or(0.0);
        let close = parse_num(item, "close").unwrap_or(last);
        let turnover = parse_num(item, "value").unwrap_or(0.0);
        if last <= 0.0 || close <= 0.0 {
            continue;
        }
        if turnover + f64::EPSILON < cfg.min_turnover_usdt {
            continue;
        }
        let vol = if high > 0.0 && low > 0.0 && high >= low {
            (high - low) / close
        } else {
            0.0
        };
        cands.push(TickerCand {
            market,
            last,
            vol,
            turnover,
        });
    }

    cands.sort_by(|a, b| {
        b.turnover
            .partial_cmp(&a.turnover)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let shortlist: Vec<TickerCand> = cands
        .into_iter()
        .take(cfg.scanner_shortlist.max(1) as usize)
        .collect();
    if shortlist.is_empty() {
        warn!("scanner: no ticker candidates after turnover/quote filter");
        return Ok(None);
    }
    info!(
        n = shortlist.len(),
        top = %shortlist.first().map(|c| c.market.as_str()).unwrap_or("-"),
        "scanner: turnover shortlist ready (depth + kline next)"
    );

    let bars_n = cfg.scanner_bars.max(1);
    for cand in &shortlist {
        let spread_pct = match client.get_spot_depth(&cand.market).await {
            Ok(depth) => match spread_pct_from_depth(&depth) {
                Some(s) => s,
                None => {
                    debug!(market = %cand.market, "scanner: empty book — skip");
                    continue;
                }
            },
            Err(e) => {
                debug!(error = %e, market = %cand.market, "scanner: depth failed — skip");
                continue;
            }
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

        let h12 = match load_bars(client, &cand.market, "12hour", bars_n).await {
            Ok(v) => v,
            Err(e) => {
                debug!(error = %e, market = %cand.market, "scanner: 12h kline failed — skip");
                continue;
            }
        };
        if !pair_is_alive(&h12, cfg) {
            debug!(market = %cand.market, n = h12.len(), "scanner: 12h not alive — skip");
            continue;
        }

        let h1 = match load_bars(client, &cand.market, "1hour", bars_n).await {
            Ok(v) => v,
            Err(e) => {
                debug!(error = %e, market = %cand.market, "scanner: 1h kline failed — skip");
                continue;
            }
        };
        let Some(hour_ratio) = hour_not_dump_pump(&h1, cfg) else {
            debug!(market = %cand.market, n = h1.len(), "scanner: 1h dump/pump — skip");
            continue;
        };

        let m5 = match load_bars(client, &cand.market, "5min", bars_n).await {
            Ok(v) => v,
            Err(e) => {
                debug!(error = %e, market = %cand.market, "scanner: 5m kline failed — skip");
                continue;
            }
        };
        if !dump_wicks_ok(&m5, cfg) {
            debug!(market = %cand.market, n = m5.len(), "scanner: 5m dump wick filter — skip");
            continue;
        }

        let pick = MarketPick {
            market: cand.market.clone(),
            last: cand.last,
            vol: cand.vol,
            spread_pct,
            turnover: cand.turnover,
            hour_ratio,
        };
        info!(
            market = %pick.market,
            vol_pct = pick.vol * 100.0,
            hour_directionality = pick.hour_ratio,
            spread_pct = pick.spread_pct,
            turnover = pick.turnover,
            last = pick.last,
            "scanner: selected chopping spot market (12h alive, 1h not dump/pump, 5m dump wick)"
        );
        return Ok(Some(pick));
    }

    warn!("scanner: no candidate passed 12h/1h/5m filters");
    Ok(None)
}

async fn load_bars(
    client: &CoinExClient,
    market: &str,
    period: &str,
    limit: u32,
) -> Result<Vec<Bar>> {
    let raw = client.get_spot_kline(market, period, limit).await?;
    Ok(parse_bars(&raw))
}

fn parse_bars(klines: &[Value]) -> Vec<Bar> {
    let mut indexed: Vec<(i64, Bar)> = Vec::new();
    for (i, item) in klines.iter().enumerate() {
        let open = parse_num(item, "open").unwrap_or(0.0);
        let high = parse_num(item, "high").unwrap_or(0.0);
        let low = parse_num(item, "low").unwrap_or(0.0);
        let close = parse_num(item, "close").unwrap_or(0.0);
        if open > 0.0 && high > 0.0 && low > 0.0 && close > 0.0 && high + f64::EPSILON >= low {
            let ts = item
                .get("created_at")
                .and_then(|v| v.as_i64())
                .or_else(|| parse_num(item, "created_at").map(|f| f as i64))
                .unwrap_or(i as i64);
            indexed.push((
                ts,
                Bar {
                    open,
                    high,
                    low,
                    close,
                },
            ));
        }
    }
    indexed.sort_by_key(|(ts, _)| *ts);
    indexed.into_iter().map(|(_, b)| b).collect()
}

/// 12h: activity across the window, not a single spike that "woke" the pair.
fn pair_is_alive(bars: &[Bar], cfg: &BotConfig) -> bool {
    if bars.is_empty() {
        return false;
    }
    let active = bars.iter().filter(|b| b.range() > f64::EPSILON).count() as u32;
    if active < cfg.scanner_alive_min_bars {
        return false;
    }
    let Some((wh, wl)) = window_high_low(bars) else {
        return false;
    };
    let window = wh - wl;
    if window <= f64::EPSILON {
        return false;
    }
    let max_bar = bars.iter().map(|b| b.range()).fold(0.0_f64, f64::max);
    max_bar / window <= cfg.scanner_alive_max_bar_share + f64::EPSILON
}

/// 1h: reject one-way dump/pump. `Some(directionality)` if accepted.
fn hour_not_dump_pump(bars: &[Bar], cfg: &BotConfig) -> Option<f64> {
    if bars.len() < 2 {
        return None;
    }
    let (wh, wl) = window_high_low(bars)?;
    let window = wh - wl;
    if window <= f64::EPSILON {
        return None;
    }
    let first_open = bars.first()?.open;
    let last_close = bars.last()?.close;
    let directionality = (last_close - first_open).abs() / window;
    if directionality > cfg.scanner_hour_directionality_max + f64::EPSILON {
        return None;
    }
    let pos = (last_close - wl) / window;
    if pos + f64::EPSILON < cfg.scanner_close_pos_min
        || pos > cfg.scanner_close_pos_max + f64::EPSILON
    {
        return None;
    }
    Some(directionality)
}

/// 5m: among bearish bars, lower wick must beat body. Bullish unrestricted.
/// No bearish bars in the window → pass (up-move left as-is).
fn dump_wicks_ok(bars: &[Bar], cfg: &BotConfig) -> bool {
    let mut bear = 0u32;
    let mut pass = 0u32;
    for b in bars {
        if !b.is_bearish() {
            continue;
        }
        bear += 1;
        if b.lower_wick() > b.body() + f64::EPSILON {
            pass += 1;
        }
    }
    if bear == 0 {
        return true;
    }
    (pass as f64 / bear as f64) + f64::EPSILON >= cfg.scanner_dump_wick_pass_pct
}

fn window_high_low(bars: &[Bar]) -> Option<(f64, f64)> {
    let mut hi = f64::NEG_INFINITY;
    let mut lo = f64::INFINITY;
    for b in bars {
        hi = hi.max(b.high);
        lo = lo.min(b.low);
    }
    if hi.is_finite() && lo.is_finite() && hi > lo {
        Some((hi, lo))
    } else {
        None
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BotConfig, MarketKind, TradeSide};

    fn cfg() -> BotConfig {
        BotConfig {
            api_key: "k".into(),
            api_secret: "s".into(),
            market: "BTCUSDT".into(),
            market_kind: MarketKind::Spot,
            trade_side: TradeSide::Buy,
            lot_coeff: 1.0,
            lot_step_coeff: 2.0,
            max_soft_sl_adds: 6,
            spot_fee_in_quote: true,
            initial_lot: 1.0,
            tp_sl_percent: 5.0,
            tp_sl_percent_fallback: 5.0,
            leverage: 10,
            margin_mode: "cross".into(),
            tp_sl_trigger: "latest_price".into(),
            rest_base_url: "https://api.coinex.com/v2".into(),
            ws_url: "wss://socket.coinex.com/v2/spot".into(),
            quote_ccy: "USDT".into(),
            base_ccy: "BTC".into(),
            auto_market: false,
            min_turnover_usdt: 50_000.0,
            max_spread_pct: 0.5,
            market_blacklist: vec![],
            scanner_quote: "USDT".into(),
            scanner_bars: 12,
            scanner_shortlist: 20,
            scanner_alive_min_bars: 8,
            scanner_alive_max_bar_share: 0.60,
            scanner_hour_directionality_max: 0.70,
            scanner_close_pos_min: 0.25,
            scanner_close_pos_max: 0.75,
            scanner_dump_wick_pass_pct: 0.65,
        }
    }

    fn bar(o: f64, h: f64, l: f64, c: f64) -> Bar {
        Bar {
            open: o,
            high: h,
            low: l,
            close: c,
        }
    }

    #[test]
    fn alive_rejects_single_spike() {
        let c = cfg();
        let mut bars = vec![bar(10.0, 10.1, 9.9, 10.0); 11];
        bars.push(bar(10.0, 20.0, 10.0, 19.0));
        assert!(!pair_is_alive(&bars, &c));
    }

    #[test]
    fn alive_accepts_spread_out_range() {
        let c = cfg();
        let bars: Vec<Bar> = (0..12)
            .map(|i| {
                let base = 10.0 + (i as f64 % 3.0) * 0.4;
                bar(base, base + 0.5, base - 0.5, base + 0.1)
            })
            .collect();
        assert!(pair_is_alive(&bars, &c));
    }

    #[test]
    fn hour_rejects_one_way_dump() {
        let c = cfg();
        let bars: Vec<Bar> = (0..12)
            .map(|i| {
                let p = 12.0 - i as f64;
                bar(p, p + 0.05, p - 0.2, p - 0.15)
            })
            .collect();
        assert!(hour_not_dump_pump(&bars, &c).is_none());
    }

    #[test]
    fn hour_accepts_chop_mid_close() {
        let c = cfg();
        let bars = vec![
            bar(10.0, 10.8, 9.6, 10.2),
            bar(10.2, 10.5, 9.9, 10.0),
            bar(10.0, 10.7, 9.7, 10.4),
            bar(10.4, 10.6, 9.8, 10.1),
            bar(10.1, 10.9, 9.5, 10.3),
            bar(10.3, 10.6, 9.8, 10.0),
            bar(10.0, 10.8, 9.6, 10.5),
            bar(10.5, 10.7, 9.9, 10.2),
            bar(10.2, 10.6, 9.7, 10.1),
            bar(10.1, 10.8, 9.6, 10.4),
            bar(10.4, 10.7, 9.8, 10.2),
            bar(10.2, 10.6, 9.7, 10.3),
        ];
        assert!(hour_not_dump_pump(&bars, &c).is_some());
    }

    #[test]
    fn dump_wick_requires_lower_wick_gt_body() {
        let c = cfg();
        // Bearish with tiny lower wick — fail.
        let fail = vec![bar(10.0, 10.1, 9.9, 9.5); 12];
        assert!(!dump_wicks_ok(&fail, &c));
        // Bearish with long lower wick (body 0.2, lower wick 0.8).
        let ok = vec![bar(10.0, 10.05, 9.0, 9.8); 12];
        assert!(dump_wicks_ok(&ok, &c));
        // All bullish — pass (up-move unrestricted).
        let up = vec![bar(10.0, 10.5, 9.95, 10.4); 12];
        assert!(dump_wicks_ok(&up, &c));
    }
}
