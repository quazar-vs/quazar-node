//! Bot configuration loaded from environment variables.

use anyhow::{bail, Context, Result};
use std::env;

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
    /// Working min lot = round(exchange_min × lot_coeff). Filled after market meta load.
    pub initial_lot: f64,
    /// Absolute martingale cap from `MAX_LOT` / `MAX_LOT_SPOT`.
    pub max_lot: f64,
    /// Take-profit distance as percent of first-entry price (e.g. 10 = +10% for BUY).
    pub tp_percent: f64,
    /// Soft stop-loss distance as percent of first-entry price (e.g. 10 = -10% for BUY).
    pub sl_percent: f64,
    pub leverage: u32,
    pub margin_mode: String,
    pub tp_sl_trigger: String,
    pub rest_base_url: String,
    pub ws_url: String,
    pub quote_ccy: String,
    pub base_ccy: String,
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

        let futures_max: f64 = env::var("MAX_LOT")
            .unwrap_or_else(|_| "0.064".to_string())
            .parse()
            .context("MAX_LOT must be a number")?;
        let spot_max: f64 = env::var("MAX_LOT_SPOT")
            .unwrap_or_else(|_| "6400".to_string())
            .parse()
            .context("MAX_LOT_SPOT must be a number")?;

        let max_lot = match market_kind {
            MarketKind::Spot => spot_max,
            MarketKind::Futures => futures_max,
        };

        // Prefer TP_PERCENT / SL_PERCENT; fall back to legacy TP_POINTS / SL_POINTS names.
        let tp_percent: f64 = env::var("TP_PERCENT")
            .or_else(|_| env::var("TP_POINTS"))
            .unwrap_or_else(|_| "10".to_string())
            .parse()
            .context("TP_PERCENT (or TP_POINTS) must be a number")?;
        let sl_percent: f64 = env::var("SL_PERCENT")
            .or_else(|_| env::var("SL_POINTS"))
            .unwrap_or_else(|_| "10".to_string())
            .parse()
            .context("SL_PERCENT (or SL_POINTS) must be a number")?;
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

        // Infer base/quote from market name when possible (e.g. BTCUSDT, SOLUSDT).
        let (base_ccy, quote_ccy) = split_market(&market);

        if lot_coeff <= 0.0 {
            bail!("LOT_COEFF must be > 0");
        }
        if max_lot <= 0.0 {
            bail!("MAX_LOT / MAX_LOT_SPOT must be > 0");
        }
        if tp_percent <= 0.0 || sl_percent <= 0.0 {
            bail!("TP_PERCENT and SL_PERCENT must be > 0");
        }
        if market_kind == MarketKind::Futures && !(1..=100).contains(&leverage) {
            bail!("LEVERAGE must be between 1 and 100");
        }

        Ok(Self {
            api_key,
            api_secret,
            market,
            market_kind,
            trade_side,
            lot_coeff,
            // Resolved from exchange min_amount × lot_coeff after meta load.
            initial_lot: 0.0,
            max_lot,
            tp_percent,
            sl_percent,
            leverage,
            margin_mode,
            tp_sl_trigger,
            rest_base_url,
            ws_url,
            quote_ccy,
            base_ccy,
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
        if self.max_lot + f64::EPSILON < lot {
            bail!(
                "MAX_LOT {} is below working min lot {} (exchange_min {} × LOT_COEFF {})",
                self.max_lot,
                lot,
                exchange_min,
                self.lot_coeff
            );
        }
        self.initial_lot = lot;
        Ok(())
    }

    /// TP/SL from first entry: percent of that price.
    /// BUY: TP = entry×(1+tp%), SL = entry×(1−sl%); SELL: mirrored.
    pub fn calc_tp_sl(&self, buy: bool, first_open: f64) -> (f64, f64) {
        let tp_m = self.tp_percent / 100.0;
        let sl_m = self.sl_percent / 100.0;
        if buy {
            (first_open * (1.0 + tp_m), first_open * (1.0 - sl_m))
        } else {
            (first_open * (1.0 - tp_m), first_open * (1.0 + sl_m))
        }
    }

    /// TP/SL after `soft_sl_steps` doubles, still anchored to the **first** entry.
    /// Each soft-SL shifts both levels by `first_open × sl_percent/100` (adverse direction).
    /// BUY step1: TP→entry, SL→entry−2×sl%; never uses the add-fill price.
    pub fn calc_tp_sl_at_step(&self, buy: bool, first_open: f64, soft_sl_steps: u32) -> (f64, f64) {
        let (base_tp, base_sl) = self.calc_tp_sl(buy, first_open);
        if soft_sl_steps == 0 || first_open <= 0.0 {
            return (base_tp, base_sl);
        }
        let shift = first_open * (self.sl_percent / 100.0) * f64::from(soft_sl_steps);
        if buy {
            (base_tp - shift, base_sl - shift)
        } else {
            (base_tp + shift, base_sl + shift)
        }
    }

    /// Infer approximate open from an existing TP limit price (step 0 only).
    pub fn infer_open_from_tp(&self, buy: bool, tp_price: f64) -> f64 {
        let tp_m = self.tp_percent / 100.0;
        if buy {
            tp_price / (1.0 + tp_m)
        } else {
            tp_price / (1.0 - tp_m).max(1e-12)
        }
    }
}

fn split_market(market: &str) -> (String, String) {
    for quote in ["USDT", "USDC", "BTC", "ETH", "CET"] {
        if let Some(base) = market.strip_suffix(quote) {
            if !base.is_empty() {
                return (base.to_string(), quote.to_string());
            }
        }
    }
    (market.to_string(), "USDT".to_string())
}
