//! Bot configuration loaded from environment variables.

use anyhow::{bail, Context, Result};
use std::env;

/// Default soft-SL adds (entry + 6 adds = 7 levels). Override via `MAX_SOFT_SL_ADDS`.
pub const DEFAULT_MAX_SOFT_SL_ADDS: u32 = 6;

/// Approx USDT needed for a full ladder at constant `price` (fee buffer 5%).
/// Final inventory ≈ `initial_lot × LOT_STEP_COEFF^max_adds`.
pub fn full_ladder_usdt(initial_lot: f64, price: f64, lot_step_coeff: f64, max_adds: u32) -> f64 {
    if initial_lot <= 0.0 || price <= 0.0 || lot_step_coeff <= 1.0 || max_adds == 0 {
        return f64::INFINITY;
    }
    let final_lot = initial_lot * lot_step_coeff.powi(max_adds as i32);
    final_lot * price * 1.05
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketKind {
    Futures,
    Spot,
}

impl MarketKind {
    pub fn as_api_str(&self) -> &'static str {
        match self {
            MarketKind::Futures => "FUTURES",
            MarketKind::Spot => "SPOT",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            MarketKind::Futures => "futures",
            MarketKind::Spot => "spot",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeSide {
    Buy,
    Sell,
}

impl TradeSide {
    pub fn as_str(&self) -> &'static str {
        match self {
            TradeSide::Buy => "buy",
            TradeSide::Sell => "sell",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BotConfig {
    pub api_key: String,
    pub api_secret: String,
    pub market: String,
    pub market_kind: MarketKind,
    pub trade_side: TradeSide,
    /// Multiplier applied to exchange `min_amount` → working min lot.
    /// Env: `LOT_COEFF` or `LOT_MULTIPLIER` (default 1).
    pub lot_coeff: f64,
    /// Soft-SL lot multiplier. Env: `LOT_STEP_COEFF` (default 2 → 100→200→400).
    pub lot_step_coeff: f64,
    /// Max full soft-SL adds after entry (levels = adds + 1). Env: `MAX_SOFT_SL_ADDS` (default 6 → 7 levels).
    pub max_soft_sl_adds: u32,
    /// Spot fee charged in quote (USDT): buy exact base shortage, no `/(1−fee)` pad.
    /// Env: `SPOT_FEE_IN_QUOTE` (default true). Set false only if the pair deducts fee from base.
    pub spot_fee_in_quote: bool,
    /// Working min lot = round(exchange_min × lot_coeff). Filled after market meta load.
    pub initial_lot: f64,
    /// Shared TP and SL distance as percent of first-entry price.
    /// Active value: auto-derived from pair fee after market meta load, with env fallback.
    pub tp_sl_percent: f64,
    /// Manual env fallback kept for cases where pair fee metadata is missing/invalid.
    /// Env: `TP_SL_PERCENT` (e.g. 5 = +5% TP and −5% SL for BUY).
    pub tp_sl_percent_fallback: f64,
    pub leverage: u32,
    pub margin_mode: String,
    pub tp_sl_trigger: String,
    pub rest_base_url: String,
    pub ws_url: String,
    pub quote_ccy: String,
    pub base_ccy: String,
    /// If true, pick a volatile tight-spread USDT pair before each new cycle.
    pub auto_market: bool,
    /// 24h turnover floor in quote (USDT) for scanner candidates.
    pub min_turnover_usdt: f64,
    /// Max bid/ask spread percent for scanner (e.g. 0.5 = 0.5%).
    pub max_spread_pct: f64,
    /// Markets the scanner must skip.
    pub market_blacklist: Vec<String>,
    /// Quote suffix the scanner keeps (usually USDT).
    pub scanner_quote: String,
    /// Candles per timeframe (12h / 1h / 5m). Env: `SCANNER_BARS` (default 12).
    pub scanner_bars: u32,
    /// After turnover filter, how many pairs get depth + kline. Env: `SCANNER_SHORTLIST` (default 20).
    pub scanner_shortlist: u32,
    /// 12h liveness: at least this many of `scanner_bars` must have a non-zero range.
    pub scanner_alive_min_bars: u32,
    /// 12h liveness: one bar must not dominate the window range (e.g. 0.60).
    pub scanner_alive_max_bar_share: f64,
    /// 1h anti dump/pump: |net move| / window range must be below this (e.g. 0.70).
    pub scanner_hour_directionality_max: f64,
    /// 1h: last close position in window range must be ≥ this (e.g. 0.25).
    pub scanner_close_pos_min: f64,
    /// 1h: last close position in window range must be ≤ this (e.g. 0.75).
    pub scanner_close_pos_max: f64,
    /// 5m: share of bearish candles with lower_wick > body (e.g. 0.65).
    pub scanner_dump_wick_pass_pct: f64,
}

impl BotConfig {
    pub fn from_env() -> Result<Self> {
        dotenv::dotenv().ok();

        let api_key = env::var("COINEX_API_KEY").context("COINEX_API_KEY is required")?;
        let api_secret = env::var("COINEX_API_SECRET").context("COINEX_API_SECRET is required")?;
        let market = env::var("MARKET").unwrap_or_else(|_| "BTCUSDT".to_string());

        let market_kind = match env::var("MARKET_TYPE")
            .unwrap_or_else(|_| "futures".to_string())
            .to_ascii_lowercase()
            .as_str()
        {
            "spot" => MarketKind::Spot,
            "futures" | "future" | "swap" => MarketKind::Futures,
            other => bail!("MARKET_TYPE must be 'spot' or 'futures', got '{other}'"),
        };

        let trade_side = match env::var("TRADE_SIDE")
            .unwrap_or_else(|_| "buy".to_string())
            .to_ascii_lowercase()
            .as_str()
        {
            "buy" | "long" => TradeSide::Buy,
            "sell" | "short" => TradeSide::Sell,
            other => bail!("TRADE_SIDE must be 'buy' or 'sell', got '{other}'"),
        };

        // Coefficient on exchange min_amount. Prefer LOT_COEFF / LOT_MULTIPLIER.
        // Legacy: INITIAL_LOT_SPOT / INITIAL_LOT kept only as optional hint (not the min source).
        let lot_coeff: f64 = env::var("LOT_COEFF")
            .or_else(|_| env::var("LOT_MULTIPLIER"))
            .unwrap_or_else(|_| "1".to_string())
            .parse()
            .context("LOT_COEFF (or LOT_MULTIPLIER) must be a number")?;

        let lot_step_coeff: f64 = env::var("LOT_STEP_COEFF")
            .unwrap_or_else(|_| "2".to_string())
            .parse()
            .context("LOT_STEP_COEFF must be a number")?;

        let max_soft_sl_adds: u32 = env::var("MAX_SOFT_SL_ADDS")
            .unwrap_or_else(|_| DEFAULT_MAX_SOFT_SL_ADDS.to_string())
            .parse()
            .context("MAX_SOFT_SL_ADDS must be an integer")?;

        let spot_fee_in_quote = matches!(
            env::var("SPOT_FEE_IN_QUOTE")
                .unwrap_or_else(|_| "true".to_string())
                .to_ascii_lowercase()
                .as_str(),
            "1" | "true" | "yes" | "on"
        );

        // One percent for both TP and SL. Legacy: TP_PERCENT / SL_PERCENT / TP_POINTS / SL_POINTS.
        let tp_sl_percent_fallback: f64 = env::var("TP_SL_PERCENT")
            .or_else(|_| env::var("TP_PERCENT"))
            .or_else(|_| env::var("SL_PERCENT"))
            .or_else(|_| env::var("TP_POINTS"))
            .or_else(|_| env::var("SL_POINTS"))
            .unwrap_or_else(|_| "10".to_string())
            .parse()
            .context("TP_SL_PERCENT must be a number")?;
        let tp_sl_percent = tp_sl_percent_fallback;
        let leverage: u32 = env::var("LEVERAGE")
            .unwrap_or_else(|_| "10".to_string())
            .parse()
            .context("LEVERAGE must be an integer")?;

        let margin_mode = env::var("MARGIN_MODE").unwrap_or_else(|_| "cross".to_string());
        let tp_sl_trigger = env::var("TP_SL_TRIGGER").unwrap_or_else(|_| {
            if market_kind == MarketKind::Spot {
                "latest_price".to_string()
            } else {
                "mark_price".to_string()
            }
        });
        let rest_base_url =
            env::var("COINEX_REST_URL").unwrap_or_else(|_| "https://api.coinex.com/v2".to_string());

        let default_ws = match market_kind {
            MarketKind::Futures => "wss://socket.coinex.com/v2/futures",
            MarketKind::Spot => "wss://socket.coinex.com/v2/spot",
        };
        let ws_url = env::var("COINEX_WS_URL").unwrap_or_else(|_| default_ws.to_string());

        let auto_market = matches!(
            env::var("AUTO_MARKET")
                .unwrap_or_else(|_| "false".to_string())
                .to_ascii_lowercase()
                .as_str(),
            "1" | "true" | "yes" | "on"
        );
        let min_turnover_usdt: f64 = env::var("MIN_TURNOVER_USDT")
            .unwrap_or_else(|_| "50000".to_string())
            .parse()
            .context("MIN_TURNOVER_USDT must be a number")?;
        let max_spread_pct: f64 = env::var("MAX_SPREAD_PCT")
            .unwrap_or_else(|_| "0.5".to_string())
            .parse()
            .context("MAX_SPREAD_PCT must be a number")?;
        let market_blacklist = env::var("MARKET_BLACKLIST")
            .unwrap_or_else(|_| "USDCUSDT,DAIUSDT,TUSDUSDT,FDUSDUSDT,BUSDUSDT".to_string())
            .split(',')
            .map(|s| s.trim().to_ascii_uppercase())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        let scanner_quote = env::var("SCANNER_QUOTE")
            .unwrap_or_else(|_| "USDT".to_string())
            .to_ascii_uppercase();
        let scanner_bars: u32 = env::var("SCANNER_BARS")
            .unwrap_or_else(|_| "12".to_string())
            .parse()
            .context("SCANNER_BARS must be an integer")?;
        let scanner_shortlist: u32 = env::var("SCANNER_SHORTLIST")
            .unwrap_or_else(|_| "20".to_string())
            .parse()
            .context("SCANNER_SHORTLIST must be an integer")?;
        let scanner_alive_min_bars: u32 = env::var("SCANNER_ALIVE_MIN_BARS")
            .unwrap_or_else(|_| "8".to_string())
            .parse()
            .context("SCANNER_ALIVE_MIN_BARS must be an integer")?;
        let scanner_alive_max_bar_share: f64 = env::var("SCANNER_ALIVE_MAX_BAR_SHARE")
            .unwrap_or_else(|_| "0.60".to_string())
            .parse()
            .context("SCANNER_ALIVE_MAX_BAR_SHARE must be a number")?;
        let scanner_hour_directionality_max: f64 = env::var("SCANNER_HOUR_DIRECTIONALITY_MAX")
            .unwrap_or_else(|_| "0.70".to_string())
            .parse()
            .context("SCANNER_HOUR_DIRECTIONALITY_MAX must be a number")?;
        let scanner_close_pos_min: f64 = env::var("SCANNER_CLOSE_POS_MIN")
            .unwrap_or_else(|_| "0.25".to_string())
            .parse()
            .context("SCANNER_CLOSE_POS_MIN must be a number")?;
        let scanner_close_pos_max: f64 = env::var("SCANNER_CLOSE_POS_MAX")
            .unwrap_or_else(|_| "0.75".to_string())
            .parse()
            .context("SCANNER_CLOSE_POS_MAX must be a number")?;
        let scanner_dump_wick_pass_pct: f64 = env::var("SCANNER_DUMP_WICK_PASS_PCT")
            .unwrap_or_else(|_| "0.65".to_string())
            .parse()
            .context("SCANNER_DUMP_WICK_PASS_PCT must be a number")?;

        // Infer base/quote from market name when possible (e.g. BTCUSDT, SOLUSDT).
        let (base_ccy, quote_ccy) = split_market(&market);

        if lot_coeff <= 0.0 {
            bail!("LOT_COEFF must be > 0");
        }
        if lot_step_coeff <= 1.0 {
            bail!("LOT_STEP_COEFF must be > 1 (e.g. 2 = double on soft SL)");
        }
        if max_soft_sl_adds == 0 {
            bail!("MAX_SOFT_SL_ADDS must be >= 1 (number of full soft-SL adds after entry)");
        }
        if tp_sl_percent_fallback <= 0.0 {
            bail!("TP_SL_PERCENT must be > 0");
        }
        if market_kind == MarketKind::Futures && !(1..=100).contains(&leverage) {
            bail!("LEVERAGE must be between 1 and 100");
        }
        if scanner_bars == 0 {
            bail!("SCANNER_BARS must be >= 1");
        }
        if scanner_shortlist == 0 {
            bail!("SCANNER_SHORTLIST must be >= 1");
        }
        if scanner_alive_min_bars == 0 || scanner_alive_min_bars > scanner_bars {
            bail!("SCANNER_ALIVE_MIN_BARS must be in 1..=SCANNER_BARS");
        }
        if !(0.0..=1.0).contains(&scanner_alive_max_bar_share) {
            bail!("SCANNER_ALIVE_MAX_BAR_SHARE must be in 0..=1");
        }
        if !(0.0..=1.0).contains(&scanner_hour_directionality_max) {
            bail!("SCANNER_HOUR_DIRECTIONALITY_MAX must be in 0..=1");
        }
        if !(0.0..=1.0).contains(&scanner_close_pos_min)
            || !(0.0..=1.0).contains(&scanner_close_pos_max)
            || scanner_close_pos_min > scanner_close_pos_max
        {
            bail!("SCANNER_CLOSE_POS_MIN/MAX must be in 0..=1 with min <= max");
        }
        if !(0.0..=1.0).contains(&scanner_dump_wick_pass_pct) {
            bail!("SCANNER_DUMP_WICK_PASS_PCT must be in 0..=1");
        }

        Ok(Self {
            api_key,
            api_secret,
            market,
            market_kind,
            trade_side,
            lot_coeff,
            lot_step_coeff,
            max_soft_sl_adds,
            spot_fee_in_quote,
            // Resolved from exchange min_amount × lot_coeff after meta load.
            initial_lot: 0.0,
            tp_sl_percent,
            tp_sl_percent_fallback,
            leverage,
            margin_mode,
            tp_sl_trigger,
            rest_base_url,
            ws_url,
            quote_ccy,
            base_ccy,
            auto_market,
            min_turnover_usdt,
            max_spread_pct,
            market_blacklist,
            scanner_quote,
            scanner_bars,
            scanner_shortlist,
            scanner_alive_min_bars,
            scanner_alive_max_bar_share,
            scanner_hour_directionality_max,
            scanner_close_pos_min,
            scanner_close_pos_max,
            scanner_dump_wick_pass_pct,
        })
    }

    pub fn is_spot(&self) -> bool {
        self.market_kind == MarketKind::Spot
    }

    /// Set working min lot from exchange `min_amount × LOT_COEFF` (market-rounded).
    /// Call after fetching `MarketMeta`.
    pub fn apply_exchange_min_lot(
        &mut self,
        exchange_min: f64,
        round_amount: impl Fn(f64) -> f64,
    ) -> Result<()> {
        if exchange_min <= 0.0 {
            bail!("exchange min_amount must be > 0 (got {exchange_min})");
        }
        let raw = exchange_min * self.lot_coeff;
        let mut lot = round_amount(raw);
        // Never go below exchange minimum.
        if lot + f64::EPSILON < exchange_min {
            lot = round_amount(exchange_min);
        }
        if lot + f64::EPSILON < exchange_min {
            bail!(
                "rounded min lot {lot} is below exchange min_amount {exchange_min}"
            );
        }
        self.initial_lot = lot;
        Ok(())
    }

    /// Recompute TP/SL percent from the pair fee returned by market metadata.
    ///
    /// CoinEx fee rates come as fractions (e.g. `0.003 = 0.3%`), while this config
    /// stores TP/SL as percents (e.g. `1.8 = 1.8%`). Formula:
    /// `tp_sl_percent = max_fee * 2 * 3 * 100 = max_fee * 600`.
    ///
    /// Returns the active percent after applying the fee-based value or env fallback.
    pub fn apply_fee_based_tp_sl_percent(&mut self, fee_rate_for_calc: f64) -> f64 {
        let computed = Self::tp_sl_percent_from_fee_rate(fee_rate_for_calc);
        self.tp_sl_percent = if computed.is_finite() && computed > 0.0 {
            computed
        } else {
            self.tp_sl_percent_fallback
        };
        self.tp_sl_percent
    }

    /// Convert fee fraction (e.g. `0.003 = 0.3%`) into TP/SL percent points
    /// used by the rest of the strategy (e.g. `1.8 = 1.8%`).
    pub fn tp_sl_percent_from_fee_rate(fee_rate_for_calc: f64) -> f64 {
        fee_rate_for_calc * 600.0
    }

    /// How many soft-SL steps are in `lot` vs working min lot (`LOT_STEP_COEFF`).
    pub fn soft_sl_steps_from_lot(&self, lot: f64) -> u32 {
        self.soft_sl_steps_for(lot, self.initial_lot)
    }

    /// Same as `soft_sl_steps_from_lot`, but uses an explicit working min lot.
    /// Needed at restore: `client.config().initial_lot` stays 0 until meta is applied
    /// onto BotState / LiveMarket, not onto the frozen client config.
    pub fn soft_sl_steps_for(&self, lot: f64, initial_lot: f64) -> u32 {
        let m = self.lot_step_coeff;
        if initial_lot <= 0.0 || lot <= 0.0 || m <= 1.0 {
            return 0;
        }
        let ratio = lot / initial_lot;
        if ratio + f64::EPSILON < (1.0 + m) / 2.0 {
            return 0;
        }
        (ratio.ln() / m.ln()).round().max(0.0) as u32
    }

    /// TP/SL from first entry: same percent both ways.
    /// BUY: TP = entry×(1+p%), SL = entry×(1−p%); SELL: mirrored.
    pub fn calc_tp_sl(&self, buy: bool, first_open: f64) -> (f64, f64) {
        let m = self.tp_sl_percent / 100.0;
        if buy {
            (first_open * (1.0 + m), first_open * (1.0 - m))
        } else {
            (first_open * (1.0 - m), first_open * (1.0 + m))
        }
    }

    /// TP/SL after `soft_sl_steps` lot steps, still anchored to the **first** entry.
    /// Each soft-SL shifts both levels by `first_open × TP_SL_PERCENT/100` (adverse direction).
    /// BUY step1 with 10%: TP→entry, SL→entry−20%; never uses the add-fill price.
    pub fn calc_tp_sl_at_step(&self, buy: bool, first_open: f64, soft_sl_steps: u32) -> (f64, f64) {
        let (base_tp, base_sl) = self.calc_tp_sl(buy, first_open);
        if soft_sl_steps == 0 || first_open <= 0.0 {
            return (base_tp, base_sl);
        }
        let shift = first_open * (self.tp_sl_percent / 100.0) * f64::from(soft_sl_steps);
        if buy {
            (base_tp - shift, base_sl - shift)
        } else {
            (base_tp + shift, base_sl + shift)
        }
    }

    /// Infer approximate open from an existing TP limit price (step 0 only).
    pub fn infer_open_from_tp(&self, buy: bool, tp_price: f64) -> f64 {
        let tp_m = self.tp_sl_percent / 100.0;
        if buy {
            tp_price / (1.0 + tp_m)
        } else {
            tp_price / (1.0 - tp_m).max(1e-12)
        }
    }
}

pub fn split_market(market: &str) -> (String, String) {
    for quote in ["USDT", "USDC", "BTC", "ETH", "CET"] {
        if let Some(base) = market.strip_suffix(quote) {
            if !base.is_empty() {
                return (base.to_string(), quote.to_string());
            }
        }
    }
    (market.to_string(), "USDT".to_string())
}

#[cfg(test)]
mod tests {
    use super::BotConfig;

    #[test]
    fn fee_based_tp_sl_percent_stays_in_percent_units() {
        let got = BotConfig::tp_sl_percent_from_fee_rate(0.003);
        assert!((got - 1.8).abs() < 1e-12);
    }

    #[test]
    fn invalid_fee_uses_env_fallback() {
        let mut cfg = BotConfig {
            api_key: "k".into(),
            api_secret: "s".into(),
            market: "BTCUSDT".into(),
            market_kind: super::MarketKind::Spot,
            trade_side: super::TradeSide::Buy,
            lot_coeff: 1.0,
            lot_step_coeff: 2.0,
            max_soft_sl_adds: super::DEFAULT_MAX_SOFT_SL_ADDS,
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
        };
        let got = cfg.apply_fee_based_tp_sl_percent(0.0);
        assert!((got - 5.0).abs() < 1e-12);
        assert!((cfg.tp_sl_percent - 5.0).abs() < 1e-12);
    }
}
