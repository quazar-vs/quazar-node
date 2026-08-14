//! One-sided martingale strategy state and lot sizing.
//!
//! Spot start: restore botB_tp (open from client_id; TP untouched), else NEW at exchange_min × LOT_COEFF.
//! New cycle: min lot; TP/SL = ±TP_SL_PERCENT from first entry.
//! Soft SL: lot × LOT_STEP_COEFF; TP and SL each shift by another −/+ % of first entry (never from add fill).

use crate::config::{BotConfig, TradeSide};
use crate::error::BotError;
use tracing::info;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_order_side(&self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }

    pub fn as_position_side(&self) -> &'static str {
        match self {
            Side::Buy => "long",
            Side::Sell => "short",
        }
    }

    pub fn from_position_side(side: &str) -> Option<Self> {
        match side.to_ascii_lowercase().as_str() {
            "long" | "buy" => Some(Side::Buy),
            "short" | "sell" => Some(Side::Sell),
            _ => None,
        }
    }

    pub fn from_trade_side(side: TradeSide) -> Self {
        match side {
            TradeSide::Buy => Side::Buy,
            TradeSide::Sell => Side::Sell,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        }
    }

    pub fn opposite(&self) -> Self {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    TakeProfit,
    StopLoss,
    Unknown,
}

impl CloseReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            CloseReason::TakeProfit => "TakeProfit",
            CloseReason::StopLoss => "StopLoss",
            CloseReason::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Position {
    pub id: String,
    pub side: Side,
    pub open_price: f64,
    pub tp_price: f64,
    pub sl_price: f64,
    pub lot: f64,
    pub is_open: bool,
    pub tp_order_id: Option<String>,
    pub sl_order_id: Option<String>,
    pub tp_is_stop: bool,
    pub sl_is_stop: bool,
}

#[derive(Debug, Clone)]
pub struct BotState {
    pub config: BotConfig,
    pub position: Option<Position>,
    pub current_lot: f64,
    pub initial_lot: f64,
    pub max_lot: f64,
    pub trade_side: Side,
    pub is_running: bool,
    /// Soft SL frozen: not enough USDT to step the lot — keep TP as-is until it fills.
    pub sl_frozen: bool,
}

impl BotState {
    pub fn new(config: BotConfig) -> Self {
        let initial_lot = config.initial_lot;
        let max_lot = config.max_lot;
        let trade_side = Side::from_trade_side(config.trade_side);
        Self {
            config,
            position: None,
            current_lot: initial_lot,
            initial_lot,
            max_lot,
            trade_side,
            is_running: true,
            sl_frozen: false,
        }
    }

    pub fn set_position(&mut self, position: Position) {
        self.position = Some(position);
    }

    pub fn clear_position(&mut self) -> Option<Position> {
        self.position.take()
    }

    pub fn calc_tp_sl(&self, side: &Side, open_price: f64) -> (f64, f64) {
        self.config
            .calc_tp_sl(matches!(side, Side::Buy), open_price)
    }

    /// TP/SL for the current martingale depth, still from first entry.
    pub fn calc_tp_sl_at_step(&self, side: &Side, first_open: f64, soft_sl_steps: u32) -> (f64, f64) {
        self.config
            .calc_tp_sl_at_step(matches!(side, Side::Buy), first_open, soft_sl_steps)
    }

    /// How many soft-SL steps are reflected in `lot` vs working min lot.
    pub fn soft_sl_steps_from_lot(&self, lot: f64) -> u32 {
        self.config.soft_sl_steps_from_lot(lot)
    }

    pub fn detect_close_reason(position: &Position, close_price: f64) -> CloseReason {
        let dist_tp = (close_price - position.tp_price).abs();
        let dist_sl = (close_price - position.sl_price).abs();
        if dist_tp <= dist_sl {
            CloseReason::TakeProfit
        } else {
            CloseReason::StopLoss
        }
    }

    /// Soft SL against last/market price (limit TP is on the book separately).
    pub fn soft_sl_hit(side: &Side, last: f64, sl: f64) -> bool {
        match side {
            Side::Buy => last <= sl,
            Side::Sell => last >= sl,
        }
    }

    pub fn next_lot_after_close(
        &self,
        closed_lot: f64,
        reason: CloseReason,
    ) -> Result<f64, BotError> {
        let next = match reason {
            CloseReason::TakeProfit | CloseReason::Unknown => self.initial_lot,
            // Futures still close-and-reopen ×2 on SL. Spot uses `next_lot_after_sl_add`.
            CloseReason::StopLoss => self.next_step_lot(closed_lot)?,
        };
        info!(
            reason = reason.as_str(),
            closed_lot,
            next_lot = next,
            "martingale next lot"
        );
        Ok(next)
    }

    /// Target lot after a spot soft-SL add (× LOT_STEP_COEFF, hard-capped by max_lot).
    pub fn next_lot_after_sl_add(&self, current_lot: f64) -> Result<f64, BotError> {
        let next = self.next_step_lot(current_lot)?;
        info!(
            current_lot,
            next_lot = next,
            add_qty = next - current_lot,
            lot_step_coeff = self.config.lot_step_coeff,
            "spot SL add: next lot"
        );
        Ok(next)
    }

    fn next_step_lot(&self, lot: f64) -> Result<f64, BotError> {
        let next = lot * self.config.lot_step_coeff;
        if next > self.max_lot + f64::EPSILON {
            return Err(BotError::LotExceedsMax {
                lot: next,
                max_lot: self.max_lot,
            });
        }
        Ok(next.min(self.max_lot))
    }

    /// Volume-weighted average entry after adding `add_qty` at `add_price`.
    pub fn average_after_add(
        open_price: f64,
        open_lot: f64,
        add_price: f64,
        add_qty: f64,
    ) -> f64 {
        let total = open_lot + add_qty;
        if total <= f64::EPSILON {
            return open_price;
        }
        (open_price * open_lot + add_price * add_qty) / total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MarketKind;

    fn sample_config() -> BotConfig {
        BotConfig {
            api_key: "k".into(),
            api_secret: "s".into(),
            market: "KOMAUSDT".into(),
            market_kind: MarketKind::Spot,
            trade_side: TradeSide::Buy,
            lot_coeff: 2.0,
            lot_step_coeff: 2.0,
            initial_lot: 100.0,
            max_lot: 800.0,
            tp_sl_percent: 10.0,
            leverage: 10,
            margin_mode: "cross".into(),
            tp_sl_trigger: "latest_price".into(),
            rest_base_url: "https://api.coinex.com/v2".into(),
            ws_url: "wss://socket.coinex.com/v2/spot".into(),
            quote_ccy: "USDT".into(),
            base_ccy: "KOMA".into(),
            auto_market: false,
            min_turnover_usdt: 50_000.0,
            max_spread_pct: 0.5,
            market_blacklist: vec![],
            scanner_quote: "USDT".into(),
        }
    }

    #[test]
    fn min_lot_from_exchange_times_coeff() {
        let mut cfg = sample_config();
        cfg.lot_coeff = 2.0;
        cfg.initial_lot = 0.0;
        cfg.apply_exchange_min_lot(50.0, |a| (a * 1e8).round() / 1e8)
            .unwrap();
        assert!((cfg.initial_lot - 100.0).abs() < 1e-9);
    }

    #[test]
    fn tp_sl_buy() {
        let state = BotState::new(sample_config());
        let (tp, sl) = state.calc_tp_sl(&Side::Buy, 0.025);
        assert!((tp - 0.0275).abs() < 1e-12);
        assert!((sl - 0.0225).abs() < 1e-12);
    }

    #[test]
    fn tp_sl_shifts_from_first_entry_on_soft_sl_steps() {
        let state = BotState::new(sample_config());
        let p = 0.020;
        // step0: +10% / -10%
        let (tp0, sl0) = state.calc_tp_sl_at_step(&Side::Buy, p, 0);
        assert!((tp0 - 0.022).abs() < 1e-12);
        assert!((sl0 - 0.018).abs() < 1e-12);
        // step1: both down by 10% of first entry → TP=entry, SL=entry-20%
        let (tp1, sl1) = state.calc_tp_sl_at_step(&Side::Buy, p, 1);
        assert!((tp1 - 0.020).abs() < 1e-12);
        assert!((sl1 - 0.016).abs() < 1e-12);
        // step2
        let (tp2, sl2) = state.calc_tp_sl_at_step(&Side::Buy, p, 2);
        assert!((tp2 - 0.018).abs() < 1e-12);
        assert!((sl2 - 0.014).abs() < 1e-12);
        assert_eq!(state.soft_sl_steps_from_lot(100.0), 0);
        assert_eq!(state.soft_sl_steps_from_lot(200.0), 1);
        assert_eq!(state.soft_sl_steps_from_lot(400.0), 2);
        let mut cfg = sample_config();
        cfg.lot_step_coeff = 1.5;
        let stepped = BotState::new(cfg);
        assert_eq!(stepped.soft_sl_steps_from_lot(100.0), 0);
        assert_eq!(stepped.soft_sl_steps_from_lot(150.0), 1);
        assert_eq!(stepped.soft_sl_steps_from_lot(225.0), 2);
        assert!((stepped.next_lot_after_sl_add(100.0).unwrap() - 150.0).abs() < 1e-9);
    }

    #[test]
    fn tp_sl_sell() {
        let state = BotState::new(sample_config());
        let (tp, sl) = state.calc_tp_sl(&Side::Sell, 0.025);
        assert!((tp - 0.0225).abs() < 1e-12);
        assert!((sl - 0.0275).abs() < 1e-12);
    }

    #[test]
    fn martingale_cycle() {
        let state = BotState::new(sample_config());
        assert!(
            (state.next_lot_after_sl_add(100.0).unwrap() - 200.0).abs() < 1e-9
        );
        assert!(
            (state
                .next_lot_after_close(200.0, CloseReason::TakeProfit)
                .unwrap()
                - 100.0)
                .abs()
                < 1e-9
        );
        assert!(matches!(
            state.next_lot_after_sl_add(800.0),
            Err(BotError::LotExceedsMax { .. })
        ));
    }

    #[test]
    fn average_after_add_buy() {
        let avg = BotState::average_after_add(0.026, 100.0, 0.024, 100.0);
        assert!((avg - 0.025).abs() < 1e-12);
    }

    #[test]
    fn tp_sl_from_market_last_after_sl_add() {
        // After SL add, levels come from market last — not from average entry.
        let state = BotState::new(sample_config());
        let last = 0.024;
        let (tp, sl) = state.calc_tp_sl(&Side::Buy, last);
        assert!((tp - 0.0264).abs() < 1e-12);
        assert!((sl - 0.0216).abs() < 1e-12);
    }
}
