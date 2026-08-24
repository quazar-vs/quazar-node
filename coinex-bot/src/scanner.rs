//! Pick a liquid USDT spot market for the next martingale cycle.
//!
//! 0) Turnover shortlist → depth for all → keep spread ≤ max → sort narrowest first.
//! 1) 12hour × N: pair has been alive (range spread across bars, not one spike).
//! 2) Prefer explicit 5m dump-entry; else calm bullish 1h without explicit reject.
//!    Reject: window-high 5m bar has upper_wick ≥ body **and** 1h is bullish.

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
    /// Signed 1h net move vs first open: (last_close − first_open) / first_open.
    pub hour_ratio: f64,
    pub reason: &'static str,
}

struct TickerCand {
    market: String,
    last: f64,
    vol: f64,
    turnover: f64,
}

struct SpreadCand {
    market: String,
    last: f64,
    vol: f64,
    turnover: f64,
    spread_pct: f64,
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

    fn upper_wick(&self) -> f64 {
        self.high - self.open.max(self.close)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandClass {
    /// 5m dump trigger: take first in spread order.
    DumpEntry,
    /// No reject, bullish 1h: fallback if no dump entry.
    CalmBull,
    /// Explicit reject or not tradable under current rules.
    Skip,
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
        "scanner: turnover shortlist ready (depth for all, then sort by spread)"
    );

    let mut with_spread: Vec<SpreadCand> = Vec::new();
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
        with_spread.push(SpreadCand {
            market: cand.market.clone(),
            last: cand.last,
            vol: cand.vol,
            turnover: cand.turnover,
            spread_pct,
        });
    }

    with_spread.sort_by(|a, b| {
        a.spread_pct
            .partial_cmp(&b.spread_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if with_spread.is_empty() {
        warn!("scanner: no candidates left after spread filter");
        return Ok(None);
    }
    info!(
        n = with_spread.len(),
        tightest = %with_spread.first().map(|c| c.market.as_str()).unwrap_or("-"),
        tightest_spread = with_spread.first().map(|c| c.spread_pct).unwrap_or(0.0),
        "scanner: spread-sorted candidates ready (kline next)"
    );

    let bars_n = cfg.scanner_bars.max(1);
    let mut dump_pick: Option<MarketPick> = None;
    let mut calm_pick: Option<MarketPick> = None;

    for cand in &with_spread {
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
        let m5 = match load_bars(client, &cand.market, "5min", bars_n).await {
            Ok(v) => v,
            Err(e) => {
                debug!(error = %e, market = %cand.market, "scanner: 5m kline failed — skip");
                continue;
            }
        };

        let hour_bull = hour_is_bullish(&h1);
        let hour_ratio = hour_net_ratio(&h1).unwrap_or(0.0);
        let class = classify_5m(&m5, hour_bull);
        match class {
            CandClass::Skip => {
                debug!(
                    market = %cand.market,
                    hour_bull,
                    "scanner: explicit reject or no entry signal — skip"
                );
                continue;
            }
            CandClass::DumpEntry => {
                if dump_pick.is_none() {
                    dump_pick = Some(MarketPick {
                        market: cand.market.clone(),
                        last: cand.last,
                        vol: cand.vol,
                        spread_pct: cand.spread_pct,
                        turnover: cand.turnover,
                        hour_ratio,
                        reason: "dump_entry",
                    });
                }
            }
            CandClass::CalmBull => {
                if calm_pick.is_none() {
                    calm_pick = Some(MarketPick {
                        market: cand.market.clone(),
                        last: cand.last,
                        vol: cand.vol,
                        spread_pct: cand.spread_pct,
                        turnover: cand.turnover,
                        hour_ratio,
                        reason: "calm_bull",
                    });
                }
            }
        }

        // Early exit once both slots filled (list is already narrowest-first).
        if dump_pick.is_some() && calm_pick.is_some() {
            break;
        }
    }

    if let Some(pick) = dump_pick.or(calm_pick) {
        info!(
            market = %pick.market,
            reason = pick.reason,
            vol_pct = pick.vol * 100.0,
            hour_net = pick.hour_ratio,
            spread_pct = pick.spread_pct,
            turnover = pick.turnover,
            last = pick.last,
            "scanner: selected spot market"
        );
        return Ok(Some(pick));
    }

    warn!("scanner: no dump-entry or calm-bull candidate");
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

fn hour_is_bullish(bars: &[Bar]) -> bool {
    match (bars.first(), bars.last()) {
        (Some(first), Some(last)) => last.close > first.open + f64::EPSILON,
        _ => false,
    }
}

fn hour_net_ratio(bars: &[Bar]) -> Option<f64> {
    let first = bars.first()?;
    let last = bars.last()?;
    if first.open <= f64::EPSILON {
        return None;
    }
    Some((last.close - first.open) / first.open)
}

/// Window-high bar has upper wick ≥ body (top rejection).
fn top_reject_wick(bars: &[Bar]) -> bool {
    let Some(hi_bar) = bars
        .iter()
        .max_by(|a, b| a.high.partial_cmp(&b.high).unwrap_or(std::cmp::Ordering::Equal))
    else {
        return false;
    };
    hi_bar.upper_wick() + f64::EPSILON >= hi_bar.body()
}

/// Explicit dump entry: prev bearish and last.open − last.low ≥ prev.body.
fn dump_entry_signal(bars: &[Bar]) -> bool {
    if bars.len() < 2 {
        return false;
    }
    let prev = bars[bars.len() - 2];
    let last = bars[bars.len() - 1];
    if !prev.is_bearish() {
        return false;
    }
    let body = prev.body();
    if body <= f64::EPSILON {
        return false;
    }
    (last.open - last.low) + f64::EPSILON >= body
}

fn classify_5m(m5: &[Bar], hour_bull: bool) -> CandClass {
    // Explicit reject only on bullish hourly context.
    if hour_bull && top_reject_wick(m5) {
        return CandClass::Skip;
    }
    if dump_entry_signal(m5) {
        return CandClass::DumpEntry;
    }
    if hour_bull {
        return CandClass::CalmBull;
    }
    CandClass::Skip
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
            scanner_shortlist: 40,
            scanner_alive_min_bars: 8,
            scanner_alive_max_bar_share: 0.60,
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
    fn hour_bullish_when_close_above_first_open() {
        let up = vec![bar(10.0, 10.5, 9.9, 10.2), bar(10.2, 11.0, 10.1, 10.8)];
        assert!(hour_is_bullish(&up));
        let down = vec![bar(10.0, 10.2, 9.5, 9.6), bar(9.6, 9.7, 9.0, 9.1)];
        assert!(!hour_is_bullish(&down));
    }

    #[test]
    fn top_reject_only_with_bull_hour() {
        // High of window on last bar with huge upper wick.
        let m5 = vec![
            bar(10.0, 10.2, 9.9, 10.1),
            bar(10.1, 10.3, 10.0, 10.2),
            bar(10.2, 12.0, 10.2, 10.4), // high=12, body=0.2, upper=1.6
        ];
        assert!(top_reject_wick(&m5));
        assert_eq!(classify_5m(&m5, true), CandClass::Skip);
        // Same 5m on bear hour: not this reject; no dump signal → skip as non-calm.
        assert_eq!(classify_5m(&m5, false), CandClass::Skip);
    }

    #[test]
    fn dump_entry_prev_bear_and_plunge() {
        let mut m5 = vec![bar(10.0, 10.1, 9.95, 10.05); 10];
        // prev bearish body=0.4; last open-low=0.5 ≥ 0.4
        m5.push(bar(10.5, 10.55, 10.0, 10.1));
        m5.push(bar(10.1, 10.15, 9.6, 9.9));
        assert!(dump_entry_signal(&m5));
        assert_eq!(classify_5m(&m5, false), CandClass::DumpEntry);
        assert_eq!(classify_5m(&m5, true), CandClass::DumpEntry);
    }

    #[test]
    fn calm_bull_when_no_reject_no_dump() {
        // Upper wick tiny vs body so window-high is not a top-reject; no dump plunge.
        let m5: Vec<Bar> = (0..12)
            .map(|i| {
                let o = 10.0 + i as f64 * 0.05;
                bar(o, o + 0.05, o - 0.01, o + 0.04)
            })
            .collect();
        assert!(!top_reject_wick(&m5));
        assert!(!dump_entry_signal(&m5));
        assert_eq!(classify_5m(&m5, true), CandClass::CalmBull);
        assert_eq!(classify_5m(&m5, false), CandClass::Skip);
    }
}
