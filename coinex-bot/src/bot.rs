//! Spot BUY martingale: always exactly one limit TP.
//! Start: botB_tp → restore (first open from client_id; TP untouched); else NEW at exchange_min × LOT_COEFF.
//! New cycle: min lot; TP/SL = ±% of first fill.
//! Soft SL: × LOT_STEP_COEFF via modify; TP and SL each shift by another TP_SL_PERCENT of **first entry** (not add fill).
//! After TP / idle with no order: optional AUTO_MARKET scanner picks a volatile tight-spread USDT pair.

use crate::config::{split_market, BotConfig};
use crate::error::BotError;
use crate::exchange::{
    is_bot_tp_client_id, make_bot_tp_client_id, parse_open_from_bot_tp_client_id, round_amount,
    round_price, side_prefix, snap_amount_to_min, spot_buy_gross_for_shortage, CoinExClient,
    ExchangePosition, MarketMeta, WsEvent,
};
use crate::scanner;
use crate::strategy::{BotState, CloseReason, Position, Side};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

pub struct Bot {
    client: Arc<CoinExClient>,
    state: Arc<Mutex<BotState>>,
    market_meta: StdMutex<MarketMeta>,
    handled_closes: Arc<Mutex<HashSet<String>>>,
    closing: Arc<Mutex<bool>>,
    /// Blocks idle-ensure from opening a second cycle while open_position is in flight.
    opening: Arc<Mutex<bool>>,
    /// TP fill seen while soft-SL/reopen holds `closing` (otherwise WS fill is ignored).
    tp_filled_during_busy: Arc<Mutex<Option<String>>>,
    /// Exact TP order id we currently own (survives soft-SL clear of `position`).
    watched_tp_order_id: Arc<Mutex<Option<String>>>,
    /// Ignore soft-SL / reconcile for a short window after place/amend (API lag + stale ticks).
    tp_guard_until: Arc<Mutex<Instant>>,
    /// After a failed/empty scan, skip idle opens until this instant.
    scanner_cooldown_until: StdMutex<Instant>,
}

impl Bot {
    pub async fn new(config: BotConfig) -> Result<Self> {
        let client = Arc::new(CoinExClient::new(config.clone())?);
        let meta = client.get_market_meta().await.context("load market meta")?;
        let mut config = config;
        let meta_for_round = meta.clone();
        config
            .apply_exchange_min_lot(meta.min_amount, |a| round_amount(a, &meta_for_round))
            .context("resolve working min lot from exchange min_amount × LOT_COEFF")?;
        info!(
            market = %config.market,
            mode = config.market_kind.label(),
            side = config.trade_side.as_str(),
            tick_size = meta.tick_size,
            exchange_min_amount = meta.min_amount,
            lot_coeff = config.lot_coeff,
            working_min_lot = config.initial_lot,
            max_lot = config.max_lot,
            "loaded market metadata — min lot = exchange_min × LOT_COEFF"
        );
        client.set_working_min_lot(config.initial_lot);
        Ok(Self {
            client,
            state: Arc::new(Mutex::new(BotState::new(config))),
            market_meta: StdMutex::new(meta),
            handled_closes: Arc::new(Mutex::new(HashSet::new())),
            closing: Arc::new(Mutex::new(false)),
            opening: Arc::new(Mutex::new(false)),
            tp_filled_during_busy: Arc::new(Mutex::new(None)),
            watched_tp_order_id: Arc::new(Mutex::new(None)),
            tp_guard_until: Arc::new(Mutex::new(Instant::now())),
            scanner_cooldown_until: StdMutex::new(Instant::now()),
        })
    }

    fn meta(&self) -> MarketMeta {
        *self.market_meta.lock().expect("meta lock")
    }

    fn set_meta(&self, meta: MarketMeta) {
        *self.market_meta.lock().expect("meta lock") = meta;
    }

    async fn watch_tp(&self, order_id: Option<&str>) {
        *self.watched_tp_order_id.lock().await = order_id.map(str::to_string);
    }

    async fn arm_tp_guard(&self) {
        *self.tp_guard_until.lock().await = Instant::now() + Duration::from_secs(8);
    }

    async fn tp_guard_active(&self) -> bool {
        Instant::now() < *self.tp_guard_until.lock().await
    }

    async fn clear_busy_tp_fill(&self) {
        *self.tp_filled_during_busy.lock().await = None;
    }

    async fn take_busy_tp_fill(&self) -> Option<String> {
        self.tp_filled_during_busy.lock().await.take()
    }

    async fn tp_still_pending(&self, tp_order_id: Option<&str>) -> bool {
        let Some(oid) = tp_order_id else {
            return false;
        };
        let limits = self.client.get_pending_orders().await.unwrap_or_default();
        limits.iter().any(|item| id_from(item, "order_id") == oid)
    }

    pub async fn run(self, mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
        self.restore_or_bootstrap().await?;

        let (tx, mut rx) = mpsc::channel::<WsEvent>(256);
        let client = self.client.clone();
        let ws_shutdown = shutdown.clone();
        let ws_task = tokio::spawn(async move {
            if let Err(e) = client.run_private_ws(ws_shutdown, tx).await {
                error!(error = %e, "websocket task terminated with error");
            }
        });

        let mut price_poll = tokio::time::interval(std::time::Duration::from_secs(1));
        price_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("shutdown signal received");
                        self.state.lock().await.is_running = false;
                        break;
                    }
                }
                _ = price_poll.tick() => {
                    if self.client.config().is_spot() {
                        let has_pos = self.state.lock().await.position.is_some();
                        if has_pos {
                            match self.client.get_last_price().await {
                                Ok(price) => {
                                    if let Err(e) = self.on_price_tick(price).await {
                                        if Self::is_critical(&e) {
                                            error!(error = %e, "critical martingale stop");
                                            self.state.lock().await.is_running = false;
                                            break;
                                        }
                                        error!(error = %e, "price poll handler error (continuing)");
                                    }
                                }
                                Err(e) => warn!(error = %e, "REST last price poll failed"),
                            }
                        } else if let Err(e) = self.ensure_idle_spot_entry().await {
                            if Self::is_critical(&e) {
                                error!(error = %e, "critical martingale stop");
                                self.state.lock().await.is_running = false;
                                break;
                            }
                            // Balance / API blips: keep retrying every poll — never stay idle forever.
                            warn!(error = %e, "idle ensure entry failed (will retry)");
                        }
                    }
                }
                event = rx.recv() => {
                    match event {
                        Some(ev) => {
                            if let Err(e) = self.handle_event(ev).await {
                                if Self::is_critical(&e) {
                                    error!(error = %e, "critical martingale stop");
                                    self.state.lock().await.is_running = false;
                                    break;
                                }
                                error!(error = %e, "event handler error (continuing)");
                            }
                        }
                        None => {
                            warn!("event channel closed");
                            break;
                        }
                    }
                }
            }
        }

        ws_task.abort();
        Ok(())
    }

    fn is_critical(e: &anyhow::Error) -> bool {
        // Soft-SL InsufficientBalance / ×2>max freeze the cycle (Ok) — not a stop.
        // LotExceedsMax remains fatal only outside that path (e.g. futures SL reopen).
        e.chain().any(|c| {
            c.downcast_ref::<BotError>()
                .is_some_and(|be| matches!(be, BotError::LotExceedsMax { .. }))
        })
    }

    fn is_insufficient_balance(e: &anyhow::Error) -> bool {
        e.chain().any(|c| {
            c.downcast_ref::<BotError>()
                .is_some_and(|be| matches!(be, BotError::InsufficientBalance { .. }))
        })
    }

    async fn restore_or_bootstrap(&self) -> Result<()> {
        let side = self.state.lock().await.trade_side.clone();
        if self.client.config().is_spot() {
            self.restore_or_bootstrap_spot(side).await
        } else {
            self.restore_or_bootstrap_futures(side).await
        }
    }

    async fn restore_or_bootstrap_futures(&self, side: Side) -> Result<()> {
        let open = self.client.get_open_positions().await?;
        info!(count = open.len(), "fetched futures positions");

        let mut ours = None;
        for p in &open {
            if Side::from_position_side(&p.side) == Some(side.clone()) {
                ours = Some(p);
                break;
            }
        }

        for p in &open {
            if Side::from_position_side(&p.side) != Some(side.clone()) {
                warn!(
                    side = %p.side,
                    size = %p.open_interest,
                    "opposite futures position exists; one-sided bot will ignore it"
                );
            }
        }

        if let Some(p) = ours {
            let pos = self.exchange_to_position(side.clone(), p).await?;
            info!(lot = pos.lot, id = %pos.id, "restored futures position");
            let mut st = self.state.lock().await;
            st.current_lot = pos.lot;
            st.set_position(pos);
        } else {
            let pending = self.client.get_pending_orders().await.unwrap_or_default();
            if !pending.is_empty() {
                warn!(
                    count = pending.len(),
                    "open futures orders exist on market — refusing new entry"
                );
            } else {
                let lot = self.state.lock().await.initial_lot;
                info!(side = side.label(), lot, "no position — opening martingale entry");
                self.open_position(side, lot).await?;
            }
        }
        self.log_state_ready().await;
        Ok(())
    }

    async fn restore_or_bootstrap_spot(&self, side: Side) -> Result<()> {
        // Soft SL only — drop legacy exchange stops (not part of strategy open order).
        let n_stops = self.client.cancel_all_spot_stops().await.unwrap_or(0);
        if n_stops > 0 {
            info!(cancelled = n_stops, "cancelled legacy spot stop orders");
        }

        let limits = self.client.get_pending_orders().await.unwrap_or_default();
        info!(limits = limits.len(), "fetched spot pending limits");

        self.dedupe_bot_tp_limits(&limits, &side).await;

        let mut limits = self.client.get_pending_orders().await.unwrap_or_default();
        if self.try_restore_from_tp_limit(side.clone(), &limits).is_none()
            && self.client.config().auto_market
        {
            if let Ok(all) = self.client.get_all_pending_orders().await {
                if let Some((item, _)) = find_by_client_id(&all, &bot_tp_client_id_base_for(&side)) {
                    if let Some(mkt) = item.get("market").and_then(|v| v.as_str()) {
                        if !mkt.eq_ignore_ascii_case(&self.client.market()) {
                            info!(market = %mkt, "start: found bot TP on another market — switching");
                            if let Err(e) = self.apply_live_market(mkt).await {
                                warn!(error = %e, market = %mkt, "start: switch to TP market failed");
                            } else {
                                limits = self.client.get_pending_orders().await.unwrap_or_default();
                            }
                        }
                    }
                }
            }
        }
        if let Some(mut pos) = self.try_restore_from_tp_limit(side.clone(), &limits) {
            // Restore from pending botB_tp: open/SL inferred from TP order price.
            // If free+frozen base is clearly larger than TP amount, enlarge the order
            // (soft SL may have bought shortage but failed to apply amount).
            if let Err(e) = self.align_tp_amount_to_inventory(&mut pos).await {
                warn!(error = %e, "start: TP amount align skipped");
            }
            let lot = pos.lot;
            info!(
                lot,
                open = pos.open_price,
                tp = pos.tp_price,
                sl = pos.sl_price,
                tp_order = ?pos.tp_order_id,
                "start: restored existing bot TP (open/SL from order)"
            );
            self.watch_tp(pos.tp_order_id.as_deref()).await;
            let mut st = self.state.lock().await;
            st.current_lot = lot;
            st.sl_frozen = false;
            st.set_position(pos);
            drop(st);
            self.arm_tp_guard().await;
        } else {
            // Last-chance: a bot TP on any market must never be followed by a second entry.
            let tp_cid = bot_tp_client_id_base_for(&side);
            if let Ok(all) = self.client.get_all_pending_orders().await {
                if find_by_client_id(&all, &tp_cid).is_some() {
                    warn!(
                        "start: bot TP still on book but restore failed — refusing new entry"
                    );
                    self.log_state_ready().await;
                    return Ok(());
                }
            }
            if !self.select_market_if_idle(true).await {
                warn!("start: scanner found no live-nervous market — idle until next scan");
                self.log_state_ready().await;
                return Ok(());
            }
            let lot = self.state.lock().await.initial_lot;
            info!(
                side = side.label(),
                lot,
                market = %self.client.market(),
                "start: no bot TP — open_position at working min lot"
            );
            self.open_position(side, lot).await?;
        }
        self.log_state_ready().await;
        Ok(())
    }

    async fn apply_live_market(&self, market: &str) -> Result<()> {
        let current = self.client.market();
        if current.eq_ignore_ascii_case(market) {
            return Ok(());
        }
        let (base, quote) = split_market(market);
        self.client.set_live_market(market, &base, &quote);
        let meta = match self.client.get_market_meta().await {
            Ok(m) => m,
            Err(e) => {
                let (b, q) = split_market(&current);
                self.client.set_live_market(&current, &b, &q);
                return Err(e).context("load meta for scanned market");
            }
        };
        {
            let mut st = self.state.lock().await;
            if meta.min_amount > st.max_lot + f64::EPSILON {
                let (b, q) = split_market(&current);
                self.client.set_live_market(&current, &b, &q);
                anyhow::bail!(
                    "scanner skip {}: min_amount {} > max_lot {}",
                    market,
                    meta.min_amount,
                    st.max_lot
                );
            }
            st.config.market = market.to_string();
            st.config.base_ccy = base.clone();
            st.config.quote_ccy = quote.clone();
            if let Err(e) = st
                .config
                .apply_exchange_min_lot(meta.min_amount, |a| round_amount(a, &meta))
            {
                let (b, q) = split_market(&current);
                self.client.set_live_market(&current, &b, &q);
                return Err(e);
            }
            st.initial_lot = st.config.initial_lot;
            st.current_lot = st.config.initial_lot;
            self.client.set_working_min_lot(st.config.initial_lot);
        }
        self.set_meta(meta);
        info!(
            from = %current,
            to = %market,
            base = %base,
            min_amount = meta.min_amount,
            working_min_lot = self.state.lock().await.initial_lot,
            "switched live spot market"
        );
        Ok(())
    }

    /// Returns true if a market is ready for a new cycle.
    /// `force` skips the idle cooldown (use after TP / startup).
    async fn select_market_if_idle(&self, force: bool) -> bool {
        if !self.client.config().auto_market || !self.client.config().is_spot() {
            return true;
        }
        if !force {
            let until = *self.scanner_cooldown_until.lock().expect("scan cooldown");
            if Instant::now() < until {
                return false;
            }
        }
        let cfg = self.state.lock().await.config.clone();
        match scanner::select_spot_market(&self.client, &cfg).await {
            Ok(Some(pick)) => {
                if let Err(e) = self.apply_live_market(&pick.market).await {
                    warn!(
                        error = %e,
                        market = %pick.market,
                        "scanner apply failed — skip new entry"
                    );
                    *self.scanner_cooldown_until.lock().expect("scan cooldown") =
                        Instant::now() + Duration::from_secs(30);
                    return false;
                }
                true
            }
            Ok(None) => {
                warn!("scanner: no still-nervous market — wait, do not open on a calm pair");
                *self.scanner_cooldown_until.lock().expect("scan cooldown") =
                    Instant::now() + Duration::from_secs(30);
                false
            }
            Err(e) => {
                warn!(error = %e, "scanner failed — wait, do not open");
                *self.scanner_cooldown_until.lock().expect("scan cooldown") =
                    Instant::now() + Duration::from_secs(30);
                false
            }
        }
    }

    /// Enlarge TP only when leftover base is at least one exchange min lot.
    /// Tiny fee dust stays free so the working lot stays a round step.
    async fn align_tp_amount_to_inventory(&self, pos: &mut Position) -> Result<()> {
        let base = self.client.base_ccy().clone();
        let (available, frozen) = self.client.get_balance_parts(&base).await?;
        let inventory = round_amount(available + frozen, &self.meta());
        let gap = inventory - pos.lot;
        if gap + f64::EPSILON < self.meta().min_amount {
            return Ok(());
        }
        if inventory + f64::EPSILON < self.meta().min_amount {
            return Ok(());
        }
        let new_lot = inventory;
        let tp_price = pos.tp_price;
        let oid = pos.tp_order_id.clone();
        let exit_side = pos.side.opposite();
        let cid = make_bot_tp_client_id(side_prefix(&pos.side), pos.open_price);
        info!(
            tp_lot = pos.lot,
            inventory,
            available,
            frozen,
            gap,
            "TP amount < inventory — aligning order amount via modify"
        );
        let new_id = self
            .client
            .ensure_spot_tp_order(oid.as_deref(), &exit_side, new_lot, tp_price, &cid, &self.meta())
            .await
            .context("align TP amount to inventory failed")?;
        pos.lot = new_lot;
        pos.tp_order_id = Some(new_id);
        Ok(())
    }

    /// Keep the first `{botB|botS}_tp` (exact or with open-price suffix); cancel only duplicates.
    /// Foreign limits are left alone.
    async fn dedupe_bot_tp_limits(&self, limits: &[Value], side: &Side) {
        let tp_cid = bot_tp_client_id_base_for(side);
        let mut kept = false;
        for item in limits {
            let cid = item.get("client_id").and_then(|v| v.as_str()).unwrap_or("");
            if !is_bot_tp_client_id(cid, &tp_cid) {
                continue;
            }
            let id = id_from(item, "order_id");
            if id.is_empty() {
                continue;
            }
            if !kept {
                kept = true;
                continue;
            }
            match self.client.cancel_order(&id).await {
                Ok(()) => info!(%id, %cid, "cancelled duplicate bot TP"),
                Err(e) => warn!(error = %e, %id, "failed cancelling duplicate bot TP"),
            }
        }
    }

    fn try_restore_from_tp_limit(&self, side: Side, limits: &[Value]) -> Option<Position> {
        let tp_cid = bot_tp_client_id_base_for(&side);
        let (tp_obj, _) = find_by_client_id(limits, &tp_cid)?;
        let tp_price = parse_f64_field(tp_obj, "price")?;
        let lot = parse_f64_field(tp_obj, "amount")?;
        if lot <= 0.0 {
            return None;
        }
        let lot = round_amount(lot, &self.meta());
        let cfg = self.client.config();
        let buy = matches!(side, Side::Buy);
        let live_cid = tp_obj
            .get("client_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // Prefer first-entry encoded in client_id — TP price alone is wrong after soft-SL shifts.
        let open_price = parse_open_from_bot_tp_client_id(live_cid)
            .unwrap_or_else(|| cfg.infer_open_from_tp(buy, tp_price));
        // Working min lot lives on BotState / LiveMarket, not client.config().
        let initial_lot = self
            .state
            .try_lock()
            .ok()
            .map(|st| st.initial_lot)
            .filter(|v| *v > 0.0)
            .unwrap_or_else(|| self.client.working_min_lot());
        let steps = cfg.soft_sl_steps_for(lot, initial_lot);
        let (_, sl_price) = cfg.calc_tp_sl_at_step(buy, open_price, steps);
        let open_rounded = round_price(open_price, &self.meta());
        let sl_rounded = round_price(sl_price, &self.meta());
        let tp_rounded = round_price(tp_price, &self.meta());
        let order_id = id_from(tp_obj, "order_id");
        info!(
            order_id = %order_id,
            lot,
            soft_sl_steps = steps,
            tp_order_price = tp_rounded,
            first_entry = open_rounded,
            sl_from_first_entry = sl_rounded,
            client_id = %live_cid,
            "restore: first entry from client_id; SL from step; TP left unchanged"
        );
        Some(Position {
            id: format!("restored_{}", side.label()),
            side,
            open_price: open_rounded,
            tp_price: tp_rounded,
            sl_price: sl_rounded,
            lot,
            is_open: true,
            tp_order_id: Some(order_id),
            sl_order_id: None,
            tp_is_stop: false,
            sl_is_stop: false,
        })
    }

    async fn log_state_ready(&self) {
        let st = self.state.lock().await;
        info!(
            side = st.trade_side.label(),
            position = ?st.position.as_ref().map(|p| (&p.id, p.lot, p.open_price, p.tp_price, p.sl_price)),
            current_lot = st.current_lot,
            initial_lot = st.initial_lot,
            max_lot = st.max_lot,
            "one-sided martingale ready"
        );
    }

    async fn exchange_to_position(&self, side: Side, p: &ExchangePosition) -> Result<Position> {
        let open_price: f64 = p.avg_entry_price.parse().unwrap_or(0.0);
        let lot = round_amount(p.open_interest.parse().unwrap_or(0.0), &self.meta());
        let (default_tp, default_sl) = {
            let st = self.state.lock().await;
            st.calc_tp_sl(&side, open_price)
        };
        let had_tp = parse_optional_price(&p.take_profit_price);
        let had_sl = parse_optional_price(&p.stop_loss_price);
        let tp_price = round_price(had_tp.unwrap_or(default_tp), &self.meta());
        let sl_price = round_price(had_sl.unwrap_or(default_sl), &self.meta());
        if had_tp.is_none() {
            self.client.set_take_profit(&side, tp_price).await?;
        }
        if had_sl.is_none() {
            self.client.set_stop_loss(&side, sl_price).await?;
        }
        Ok(Position {
            id: p.position_id.to_string(),
            side,
            open_price,
            tp_price,
            sl_price,
            lot,
            is_open: true,
            tp_order_id: None,
            sl_order_id: None,
            tp_is_stop: false,
            sl_is_stop: false,
        })
    }

    /// Futures: no pending limits/stops before new entry.
    async fn ensure_no_open_orders(&self) -> Result<()> {
        let limits = self.client.get_pending_orders().await?;
        let stops = self.client.get_pending_stop_orders().await?;
        if limits.is_empty() && stops.is_empty() {
            return Ok(());
        }
        warn!(
            market = %self.client.market(),
            limits = limits.len(),
            stops = stops.len(),
            "open orders present — blocking new entry"
        );
        Err(anyhow::anyhow!(
            "refusing to open: {} pending limit and {} pending stop order(s) on {}",
            limits.len(),
            stops.len(),
            self.client.market()
        ))
    }

    /// Spot invariant: at most one bot TP — refuse a second open if ours already exists.
    async fn ensure_no_bot_tp(&self, side: &Side) -> Result<()> {
        let tp_cid = bot_tp_client_id_base_for(side);
        let limits = self.client.get_pending_orders().await.unwrap_or_default();
        if let Some((item, _)) = find_by_client_id(&limits, &tp_cid) {
            let id = id_from(item, "order_id");
            let live_cid = item
                .get("client_id")
                .and_then(|v| v.as_str())
                .unwrap_or(&tp_cid);
            warn!(%id, client_id = %live_cid, "bot TP already open — refusing second entry");
            return Err(anyhow::anyhow!(
                "refusing to open: {live_cid} already pending ({id})"
            ));
        }
        Ok(())
    }

    async fn wait_until_ready_to_reopen(&self, side: &Side) -> Result<()> {
        if self.client.config().is_spot() {
            let tp_cid = bot_tp_client_id_base_for(side);
            for attempt in 1..=10 {
                let limits = self.client.get_pending_orders().await.unwrap_or_default();
                if find_by_client_id(&limits, &tp_cid).is_none() {
                    return Ok(());
                }
                debug!(attempt, %tp_cid, "waiting for bot TP to clear before reopen");
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            self.ensure_no_bot_tp(side).await
        } else {
            for attempt in 1..=10 {
                let limits = self.client.get_pending_orders().await.unwrap_or_default();
                let stops = self.client.get_pending_stop_orders().await.unwrap_or_default();
                if limits.is_empty() && stops.is_empty() {
                    return Ok(());
                }
                debug!(
                    attempt,
                    limits = limits.len(),
                    stops = stops.len(),
                    "waiting for pending orders to clear"
                );
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            self.ensure_no_open_orders().await
        }
    }

    /// Open a **new** cycle at min lot: shortage buy if needed, one limit TP.
    /// Spot: always forces `initial_lot` — step sizing is soft-SL modify only.
    async fn open_position(&self, side: Side, lot: f64) -> Result<()> {
        if *self.opening.lock().await {
            warn!("open_position skipped — another entry already in flight");
            return Ok(());
        }
        *self.opening.lock().await = true;
        let result = self.open_position_inner(side, lot).await;
        *self.opening.lock().await = false;
        result
    }

    async fn open_position_inner(&self, side: Side, lot: f64) -> Result<()> {
        let mut lot = round_amount(lot, &self.meta());
        if self.client.config().is_spot() {
            let initial = round_amount(self.state.lock().await.initial_lot, &self.meta());
            if (lot - initial).abs() > f64::EPSILON {
                warn!(
                    requested = lot,
                    initial,
                    "new spot cycle forced to working min lot (lot step only via soft-SL modify)"
                );
                lot = initial;
            }
        }
        if self.client.config().is_spot() {
            self.ensure_no_bot_tp(&side)
                .await
                .context("bot TP still present before entry")?;
            // Quietly clear leftover stops so we never mix soft-SL with exchange stops.
            let _ = self.client.cancel_all_spot_stops().await;
        } else {
            self.ensure_no_open_orders()
                .await
                .context("open orders still present before entry")?;
        }

        let price = self.client.get_last_price().await?;
        // Spot BUY: quote needed only for shortage (lot − free base).
        if self.client.config().is_spot() && matches!(side, Side::Buy) {
            let available_base = self
                .client
                .get_available_balance(&self.client.base_ccy())
                .await?;
            let need = (lot - available_base).max(0.0);
            if available_base + f64::EPSILON >= lot {
                info!(
                    available_base,
                    lot,
                    base = %self.client.base_ccy(),
                    "open: free base covers lot — limit TP only"
                );
            } else if let Some(buy_qty) = spot_buy_gross_for_shortage(available_base, lot, &self.meta()) {
                info!(
                    available_base,
                    lot,
                    need,
                    buy_qty,
                    "open: USDT check for fee-padded shortage"
                );
                self.client
                    .ensure_balance_for_lot(&side, buy_qty, price)
                    .await
                    .context("insufficient quote balance for spot shortage top-up")?;
            } else {
                info!(
                    available_base,
                    lot,
                    need,
                    "open: fee-padded shortage below min_amount — no extra buy"
                );
            }
        } else {
            self.client
                .ensure_balance_for_lot(&side, lot, price)
                .await
                .context("insufficient balance for martingale entry")?;
        }

        let (tp, sl) = {
            let st = self.state.lock().await;
            st.calc_tp_sl(&side, price)
        };

        let mut position = self
            .client
            .open_position_with_tp_sl(&side, lot, tp, sl, &self.meta())
            .await?;

        // Futures: refresh exchange TP/SL around actual fill if needed.
        if !self.client.config().is_spot() && (position.open_price - price).abs() > f64::EPSILON {
            let (tp2, sl2) = {
                let st = self.state.lock().await;
                st.calc_tp_sl(&side, position.open_price)
            };
            let tp2 = round_price(tp2, &self.meta());
            let sl2 = round_price(sl2, &self.meta());
            let tick = self.meta().tick_size.max(1e-12);
            if (tp2 - position.tp_price).abs() > tick / 2.0
                || (sl2 - position.sl_price).abs() > tick / 2.0
            {
                let _ = self.client.set_take_profit(&side, tp2).await;
                let _ = self.client.set_stop_loss(&side, sl2).await;
                position.tp_price = tp2;
                position.sl_price = sl2;
            }
        }

        let opened_lot = position.lot;
        info!(
            side = side.label(),
            lot = opened_lot,
            open_price = position.open_price,
            tp = position.tp_price,
            sl = position.sl_price,
            tp_order = ?position.tp_order_id,
            "martingale position opened"
        );

        let mut st = self.state.lock().await;
        st.current_lot = opened_lot;
        st.sl_frozen = false;
        let tp_oid = position.tp_order_id.clone();
        st.set_position(position);
        drop(st);
        self.watch_tp(tp_oid.as_deref()).await;
        self.arm_tp_guard().await;
        Ok(())
    }

    async fn handle_event(&self, event: WsEvent) -> Result<()> {
        match event {
            WsEvent::Connected => {
                info!("websocket connected");
                Ok(())
            }
            WsEvent::Disconnected => {
                warn!("websocket disconnected — reconnecting");
                Ok(())
            }
            WsEvent::PriceTick { price } => self.on_price_tick(price).await,
            WsEvent::OrderFinished {
                order_id,
                side,
                last_filled_price,
                market,
                client_id,
            } => {
                info!(%order_id, %side, last_filled_price, %market, %client_id, "execution update");
                if self.client.config().is_spot() {
                    self.on_spot_tp_fill(order_id, last_filled_price, &client_id)
                        .await?;
                }
                Ok(())
            }
            WsEvent::PositionClosed {
                position_id,
                side,
                close_price,
            } => self.on_futures_closed(position_id, side, close_price).await,
        }
    }

    /// Keep exactly one spot cycle alive: if memory lost the position (failed reopen,
    /// missed fill, restart gap), restore from a pending bot TP or open a new min-lot entry.
    /// Never opens a second bot TP — `ensure_no_bot_tp` / restore guard that.
    async fn ensure_idle_spot_entry(&self) -> Result<()> {
        if !self.client.config().is_spot() {
            return Ok(());
        }
        if *self.closing.lock().await {
            return Ok(());
        }
        if *self.opening.lock().await {
            return Ok(());
        }
        if self.state.lock().await.position.is_some() {
            return Ok(());
        }

        let side = self.state.lock().await.trade_side.clone();
        let limits = self.client.get_pending_orders().await.unwrap_or_default();
        self.dedupe_bot_tp_limits(&limits, &side).await;

        let limits = self.client.get_pending_orders().await.unwrap_or_default();
        if let Some(mut pos) = self.try_restore_from_tp_limit(side.clone(), &limits) {
            if let Err(e) = self.align_tp_amount_to_inventory(&mut pos).await {
                warn!(error = %e, "idle ensure: TP amount align skipped");
            }
            info!(
                lot = pos.lot,
                open = pos.open_price,
                tp = pos.tp_price,
                sl = pos.sl_price,
                tp_order = ?pos.tp_order_id,
                "idle ensure: restored existing bot TP (no in-memory position)"
            );
            self.watch_tp(pos.tp_order_id.as_deref()).await;
            let mut st = self.state.lock().await;
            st.current_lot = pos.lot;
            st.sl_frozen = false;
            st.set_position(pos);
            drop(st);
            self.arm_tp_guard().await;
            return Ok(());
        }

        *self.closing.lock().await = true;
        if !self.select_market_if_idle(false).await {
            *self.closing.lock().await = false;
            return Ok(());
        }
        let lot = self.state.lock().await.initial_lot;
        info!(
            side = side.label(),
            lot,
            market = %self.client.market(),
            "idle ensure: no position and no bot TP — opening new cycle"
        );
        let result = self.open_position(side.clone(), lot).await;
        *self.closing.lock().await = false;

        if let Err(ref e) = result {
            // Race: TP appeared while we were entering — attach it instead of staying idle.
            let limits = self.client.get_pending_orders().await.unwrap_or_default();
            if let Some(pos) = self.try_restore_from_tp_limit(side, &limits) {
                warn!(
                    error = %e,
                    lot = pos.lot,
                    tp_order = ?pos.tp_order_id,
                    "idle ensure open failed but bot TP exists — restoring"
                );
                self.watch_tp(pos.tp_order_id.as_deref()).await;
                let mut st = self.state.lock().await;
                st.current_lot = pos.lot;
                st.sl_frozen = false;
                st.set_position(pos);
                drop(st);
                self.arm_tp_guard().await;
                return Ok(());
            }
        }
        result
    }

    async fn on_price_tick(&self, last: f64) -> Result<()> {
        if !self.client.config().is_spot() {
            return Ok(());
        }
        if *self.closing.lock().await {
            return Ok(());
        }

        // Detect TP fills missed while WS was down / reconnecting.
        if let Err(e) = self.reconcile_spot_tp_presence().await {
            return Err(e);
        }
        if *self.closing.lock().await || self.state.lock().await.position.is_none() {
            return Ok(());
        }
        if self.tp_guard_active().await {
            return Ok(());
        }
        if self.state.lock().await.sl_frozen {
            return Ok(());
        }

        // Always re-check against a fresh last — poll ticks can be stale vs open/amend.
        let fresh = self.client.get_last_price().await.unwrap_or(last);
        let hit = {
            let st = self.state.lock().await;
            match st.position.as_ref() {
                Some(p) if BotState::soft_sl_hit(&p.side, fresh, p.sl_price) => {
                    Some((
                        p.id.clone(),
                        p.side.clone(),
                        p.lot,
                        p.open_price,
                        p.sl_price,
                        p.tp_price,
                    ))
                }
                _ => None,
            }
        };
        let Some((id, side, lot, open_price, sl, tp)) = hit else {
            return Ok(());
        };

        info!(
            last = fresh,
            sl,
            tp,
            lot,
            open_price,
            "soft SL hit — adding size (no sell), amending TP in place"
        );
        *self.closing.lock().await = true;
        self.clear_busy_tp_fill().await;

        let result = self.on_spot_soft_sl_add(id, side, lot, open_price, fresh).await;
        *self.closing.lock().await = false;

        // TP may have filled while closing was held (soft SL abort → reopen).
        if let Some(oid) = self.take_busy_tp_fill().await {
            let has_pos = self.state.lock().await.position.is_some();
            if has_pos {
                warn!(%oid, "processing TP fill that arrived during soft-SL busy window");
                if let Err(e) = self.on_spot_tp_fill(oid, 0.0, "botB_tp").await {
                    error!(error = %e, "deferred TP fill handler failed");
                }
            }
        }
        result
    }

    /// If we track an open spot position but its limit TP is gone — treat as TP fill.
    async fn reconcile_spot_tp_presence(&self) -> Result<()> {
        if !self.client.config().is_spot() {
            return Ok(());
        }
        if *self.closing.lock().await {
            return Ok(());
        }
        if self.tp_guard_active().await {
            return Ok(());
        }
        let (oid, side, lot) = {
            let st = self.state.lock().await;
            match st.position.as_ref() {
                Some(p) => match &p.tp_order_id {
                    Some(id) => (id.clone(), p.side.clone(), p.lot),
                    None => return Ok(()),
                },
                None => return Ok(()),
            }
        };
        let tp_cid = bot_tp_client_id_base_for(&side);
        // API lag right after place — retry before declaring fill.
        for attempt in 1..=4u32 {
            let limits = self.client.get_pending_orders().await.unwrap_or_default();
            let still_open = limits.iter().any(|item| {
                id_from(item, "order_id") == oid
                    || item
                        .get("client_id")
                        .and_then(|v| v.as_str())
                        .is_some_and(|c| is_bot_tp_client_id(c, &tp_cid))
            });
            if still_open {
                // Id may have changed after modify — re-bind watch to live bot TP.
                if let Some((item, _)) = find_by_client_id(&limits, &tp_cid) {
                    let live = id_from(item, "order_id");
                    if !live.is_empty() && live != oid {
                        info!(old = %oid, new = %live, "reconcile: rebinding TP order id after modify");
                        self.watch_tp(Some(&live)).await;
                        let mut st = self.state.lock().await;
                        if let Some(p) = st.position.as_mut() {
                            p.tp_order_id = Some(live);
                        }
                    }
                }
                return Ok(());
            }
            if attempt < 4 {
                tokio::time::sleep(Duration::from_millis(300 * u64::from(attempt))).await;
            }
        }

        // Pending API can briefly omit an order while KOMA is still frozen in it —
        // do not treat that as a fill (would buy a second lot and break the invariant).
        let base = self.client.base_ccy().clone();
        if let Ok((available, frozen)) = self.client.get_balance_parts(&base).await {
            if frozen + f64::EPSILON >= self.meta().min_amount.min(lot * 0.5) {
                warn!(
                    %oid,
                    available,
                    frozen,
                    lot,
                    "spot TP id missing but KOMA still frozen — skip false reconcile"
                );
                return Ok(());
            }
        }

        let last = self.client.get_last_price().await.unwrap_or(0.0);
        warn!(
            %oid,
            side = side.label(),
            last,
            "spot TP missing on exchange — treating as filled (reconcile)"
        );
        self.on_spot_tp_fill(oid, last, "").await
    }

    /// Spot soft SL: market-buy only shortage to next lot (× LOT_STEP_COEFF) while TP stays on the book,
    /// then amend the existing limit TP (price+amount) via `/spot/modify-order`.
    /// If TP fills during the add — abort soft SL and reopen at INITIAL (never leave
    /// stranded KOMA without a single TP, never buy a fresh full stepped lot from zero).
    async fn on_spot_soft_sl_add(
        &self,
        id: String,
        side: Side,
        current_lot: f64,
        open_price: f64,
        last: f64,
    ) -> Result<()> {
        let target_lot = {
            let mut st = self.state.lock().await;
            // Do not snap current_lot first: 39.88 vs min 20 must stay step 0.
            match st.next_lot_after_sl_add(current_lot) {
                Ok(lot) => snap_amount_to_min(lot, &self.meta()),
                Err(BotError::LotExceedsMax { lot, max_lot }) => {
                    warn!(
                        current_lot,
                        next_lot = lot,
                        max_lot,
                        "soft SL: next lot exceeds max_lot — freezing TP until fill"
                    );
                    st.sl_frozen = true;
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            }
        };
        let theoretical_add = round_amount(target_lot - current_lot, &self.meta());
        if theoretical_add < self.meta().min_amount {
            warn!(
                current_lot,
                target_lot,
                "soft SL: lot cap reached — freezing TP until fill"
            );
            self.state.lock().await.sl_frozen = true;
            return Ok(());
        }

        let position = {
            let mut st = self.state.lock().await;
            st.clear_position()
        };
        let Some(mut position) = position else {
            return Ok(());
        };
        let tp_id = position.tp_order_id.clone();
        self.watch_tp(tp_id.as_deref()).await;
        self.clear_busy_tp_fill().await;

        let price = self.client.get_last_price().await.unwrap_or(last);
        let base = self.client.base_ccy().clone();
        let (available, frozen) = self.client.get_balance_parts(&base).await?;
        let inventory = available + frozen;
        // Shortage only: never market-buy the whole target_lot when base is already held
        // (including amount frozen in the resting TP).
        let need = (target_lot - inventory).max(0.0);
        let buy_qty = spot_buy_gross_for_shortage(inventory, target_lot, &self.meta()).unwrap_or(0.0);

        info!(
            available,
            frozen,
            inventory,
            current_lot,
            target_lot,
            need,
            buy_qty,
            "soft SL: shortage-only top-up (inventory = available + frozen)"
        );

        let mut filled_add = 0.0;
        let mut add_price = price;
        let mut did_buy = false;
        if buy_qty >= self.meta().min_amount {
            if let Err(e) = self
                .client
                .ensure_balance_for_lot(&side, buy_qty, price)
                .await
            {
                warn!(
                    error = %e,
                    buy_qty,
                    need,
                    "soft SL: insufficient USDT for shortage — freezing TP until fill"
                );
                let mut st = self.state.lock().await;
                st.sl_frozen = true;
                st.set_position(position);
                return Ok(());
            }
            match self.client.place_market_order(&side, buy_qty).await {
                Ok(add) => {
                    add_price = if add.filled_price > 0.0 {
                        add.filled_price
                    } else {
                        price
                    };
                    filled_add = if add.filled_amount > 0.0 {
                        round_amount(add.filled_amount, &self.meta())
                    } else {
                        buy_qty
                    };
                    if filled_add + f64::EPSILON < self.meta().min_amount {
                        error!(
                            filled_add,
                            buy_qty,
                            "soft SL: shortage buy returned empty fill — freeze, TP untouched"
                        );
                        let mut st = self.state.lock().await;
                        st.sl_frozen = true;
                        st.set_position(position);
                        return Ok(());
                    }
                    did_buy = true;
                    info!(
                        filled_add,
                        add_price,
                        first_entry = open_price,
                        "soft SL: shortage buy filled — enlarge TP; shift TP/SL by sl% of first entry"
                    );
                }
                Err(e) if Self::is_insufficient_balance(&e) => {
                    warn!(
                        error = %e,
                        buy_qty,
                        "soft SL: market add InsufficientBalance — freezing TP until fill"
                    );
                    let mut st = self.state.lock().await;
                    st.sl_frozen = true;
                    st.set_position(position);
                    return Ok(());
                }
                Err(e) => {
                    self.state.lock().await.set_position(position);
                    return Err(e).context("soft SL market add failed");
                }
            }
        } else {
            info!(
                inventory,
                target_lot,
                available,
                frozen,
                "soft SL: inventory already covers stepped target — no market buy"
            );
        }

        // TP may have filled while we were buying — do not amend/place a wrong-sized TP.
        let filled_busy = self.take_busy_tp_fill().await;
        let tp_alive = self.tp_still_pending(tp_id.as_deref()).await;
        if filled_busy.is_some() || !tp_alive {
            let close_id = filled_busy
                .or(tp_id)
                .unwrap_or_else(|| id.clone());
            warn!(
                %close_id,
                bought = filled_add,
                target_lot,
                "soft SL: TP filled during shortage buy — switching to TP reopen at INITIAL"
            );
            return self
                .finish_close_and_reopen(
                    position,
                    close_id,
                    self.client.get_last_price().await.unwrap_or(last),
                    CloseReason::TakeProfit,
                )
                .await;
        }

        // Market buy may settle asynchronously — wait until inventory covers the stepped lot
        // before asking CoinEx to enlarge the resting sell TP.
        let (avail_now, frozen_now) = match self
            .client
            .wait_spot_base_cover(target_lot, 10)
            .await
        {
            Ok(parts) => parts,
            Err(e) => {
                error!(
                    error = %e,
                    target_lot,
                    bought = filled_add,
                    "soft SL: base not available for TP amount enlarge — not marking success"
                );
                self.state.lock().await.set_position(position);
                return Err(e).context("soft SL: cannot enlarge TP — inventory not settled");
            }
        };

        let new_avg = BotState::average_after_add(open_price, current_lot, add_price, filled_add);
        // After this add, lot is × LOT_STEP_COEFF → one more soft-SL step. Levels shift by % of
        // **first entry**, never by the add-fill / last price.
        let open_rounded = round_price(open_price, &self.meta());
        let steps_after = {
            let st = self.state.lock().await;
            st.soft_sl_steps_from_lot(target_lot)
        };
        let (raw_tp, raw_sl) = {
            let st = self.state.lock().await;
            st.calc_tp_sl_at_step(&side, open_rounded, steps_after)
        };
        let tp_price = round_price(raw_tp, &self.meta());
        let sl_price = round_price(raw_sl, &self.meta());
        info!(
            did_buy,
            add_price,
            first_entry = open_rounded,
            soft_sl_steps = steps_after,
            tp = tp_price,
            sl = sl_price,
            "soft SL: TP/SL shifted from first entry (not add fill)"
        );

        let prefix = side_prefix(&side);
        let exit_side = side.opposite();
        let sellable = round_amount(avail_now + frozen_now, &self.meta());

        // TP everything we hold so leftover dust is not left free on the wallet.
        let mut new_lot = target_lot;
        if sellable + f64::EPSILON < target_lot {
            let shortfall = target_lot - sellable;
            if shortfall <= target_lot * 0.01 || shortfall < self.meta().min_amount {
                warn!(
                    target = target_lot,
                    sellable,
                    "soft SL: tiny fee shortfall — TP amount = sellable"
                );
                new_lot = sellable;
            } else {
                warn!(
                    target = target_lot,
                    sellable,
                    "soft SL: KOMA still below stepped lot after buy — freeze, TP untouched"
                );
                let mut st = self.state.lock().await;
                st.sl_frozen = true;
                st.set_position(position);
                return Ok(());
            }
        } else if sellable > target_lot + f64::EPSILON {
            info!(
                target = target_lot,
                sellable,
                surplus = sellable - target_lot,
                "soft SL: surplus left free (TP stays on stepped working lot)"
            );
        }

        // Final pre-modify check: KOMA >= new_lot (no credit).
        if let Err(e) = self.client.require_spot_base_cover(new_lot).await {
            error!(
                error = %e,
                new_lot,
                "soft SL: pre-modify KOMA check failed — freeze, TP untouched"
            );
            let mut st = self.state.lock().await;
            st.sl_frozen = true;
            st.set_position(position);
            return Ok(());
        }

        let tp_order_id = match self
            .client
            .ensure_spot_tp_order(
                position.tp_order_id.as_deref(),
                &exit_side,
                new_lot,
                tp_price,
                // Modify ONLY after shortage buy (if any) + KOMA >= new_lot verified.
                // client_id keeps first-entry open price (not add fill).
                &make_bot_tp_client_id(prefix, open_rounded),
                &self.meta(),
            )
            .await
        {
            Ok(oid) => oid,
            Err(e) => {
                // Modify may fail if TP filled between pending-check and amend.
                let filled_busy = self.take_busy_tp_fill().await;
                if filled_busy.is_some() || !self.tp_still_pending(tp_id.as_deref()).await {
                    let close_id = filled_busy
                        .or(tp_id)
                        .unwrap_or_else(|| id.clone());
                    warn!(
                        error = %e,
                        %close_id,
                        "soft SL: ensure TP failed because order filled — TP reopen"
                    );
                    return self
                        .finish_close_and_reopen(
                            position,
                            close_id,
                            self.client.get_last_price().await.unwrap_or(last),
                            CloseReason::TakeProfit,
                        )
                        .await;
                }
                error!(
                    error = %e,
                    target = new_lot,
                    tp = tp_price,
                    "soft SL: ensure TP failed — state NOT updated to stepped lot (no silent success)"
                );
                self.state.lock().await.set_position(position);
                return Err(e).context("soft SL failed to ensure limit TP — cycle not complete");
            }
        };

        // Final gate: live book amount must match the stepped lot before we commit memory.
        if let Err(e) = self
            .client
            .confirm_pending_order_size(&tp_order_id, new_lot, tp_price)
            .await
        {
            error!(
                error = %e,
                %tp_order_id,
                new_lot,
                tp_price,
                "soft SL: exchange TP amount still wrong after ensure — aborting state update"
            );
            self.state.lock().await.set_position(position);
            return Err(e).context("soft SL: TP amount not enlarged on exchange");
        }

        position.id = id;
        // Keep first-entry open forever in this cycle — soft-SL add does not re-anchor.
        position.open_price = open_rounded;
        position.tp_price = tp_price;
        position.sl_price = sl_price;
        position.lot = new_lot;
        position.tp_order_id = Some(tp_order_id.clone());
        position.sl_order_id = None;
        position.tp_is_stop = false;
        position.sl_is_stop = false;
        position.is_open = true;

        info!(
            side = side.label(),
            add_qty = filled_add,
            add_price,
            old_lot = current_lot,
            new_lot,
            avg_entry = round_price(new_avg, &self.meta()),
            first_entry = open_rounded,
            tp = tp_price,
            sl = sl_price,
            %tp_order_id,
            "soft SL add complete — TP amount+price updated; levels from first entry × steps"
        );

        let mut st = self.state.lock().await;
        st.current_lot = new_lot;
        st.sl_frozen = false;
        st.set_position(position);
        drop(st);
        self.watch_tp(Some(&tp_order_id)).await;
        self.arm_tp_guard().await;
        Ok(())
    }

    async fn replace_spot_tp(&self, position: &mut Position) -> Result<()> {
        let exit_side = position.side.opposite();
        let tp_cid = make_bot_tp_client_id(side_prefix(&position.side), position.open_price);
        let tp_order_id = self
            .client
            .place_limit_order(
                &exit_side,
                position.lot,
                position.tp_price,
                &tp_cid,
            )
            .await?;
        info!(
            %tp_order_id,
            client_id = %tp_cid,
            open_price = position.open_price,
            "replaced spot TP limit"
        );
        position.tp_order_id = Some(tp_order_id);
        Ok(())
    }

    async fn on_spot_tp_fill(
        &self,
        order_id: String,
        close_price: f64,
        client_id: &str,
    ) -> Result<()> {
        let is_bot_tp = is_bot_tp_client_id(client_id, "botB_tp")
            || is_bot_tp_client_id(client_id, "botS_tp");
        let watched = self.watched_tp_order_id.lock().await.clone();
        // Only the exact TP we own — never match a stale botB_tp fill against a newer cycle.
        let is_our_tp = watched.as_deref() == Some(order_id.as_str());

        if *self.closing.lock().await {
            let matched = {
                let st = self.state.lock().await;
                st.position.as_ref().is_some_and(|p| {
                    p.tp_order_id.as_deref() == Some(order_id.as_str())
                }) || is_our_tp
            };
            if matched {
                info!(
                    %order_id,
                    %client_id,
                    close_price,
                    "TP fill during busy cycle — recorded for soft-SL abort"
                );
                *self.tp_filled_during_busy.lock().await = Some(order_id);
            } else if is_bot_tp {
                debug!(
                    %order_id,
                    %client_id,
                    watched = ?watched,
                    "ignoring stale bot TP fill during busy cycle"
                );
            }
            return Ok(());
        }
        let matched = {
            let st = self.state.lock().await;
            match st.position.as_ref() {
                Some(p) => p.tp_order_id.as_deref() == Some(order_id.as_str()) || is_our_tp,
                None => false,
            }
        };
        if !matched {
            if is_bot_tp {
                debug!(
                    %order_id,
                    watched = ?watched,
                    "ignoring unmatched bot TP fill"
                );
            }
            return Ok(());
        }

        *self.closing.lock().await = true;
        let position = {
            let mut st = self.state.lock().await;
            st.clear_position()
        };
        let Some(position) = position else {
            *self.closing.lock().await = false;
            return Ok(());
        };

        let result = self
            .finish_close_and_reopen(position, order_id, close_price, CloseReason::TakeProfit)
            .await;
        *self.closing.lock().await = false;
        result
    }

    async fn on_futures_closed(
        &self,
        position_id: String,
        side: Side,
        close_price: f64,
    ) -> Result<()> {
        let trade_side = self.state.lock().await.trade_side.clone();
        if side != trade_side {
            debug!(
                closed = side.label(),
                trade = trade_side.label(),
                "ignoring non-trade-side close"
            );
            return Ok(());
        }

        {
            let mut handled = self.handled_closes.lock().await;
            if !handled.insert(position_id.clone()) {
                return Ok(());
            }
        }

        let position = {
            let mut st = self.state.lock().await;
            if st.position.is_none() {
                warn!(%position_id, "close for unknown position ignored");
                return Ok(());
            }
            st.clear_position()
        };
        let Some(position) = position else {
            return Ok(());
        };

        let reason = BotState::detect_close_reason(&position, close_price);
        self.finish_close_and_reopen(position, position_id, close_price, reason)
            .await
    }

    async fn finish_close_and_reopen(
        &self,
        position: Position,
        position_id: String,
        mut close_price: f64,
        reason: CloseReason,
    ) -> Result<()> {
        {
            let mut handled = self.handled_closes.lock().await;
            handled.insert(position_id.clone());
        }

        if close_price <= 0.0 {
            close_price = self
                .client
                .get_last_price()
                .await
                .unwrap_or(position.tp_price);
        }

        info!(
            side = position.side.label(),
            %position_id,
            close_price,
            tp = position.tp_price,
            sl = position.sl_price,
            closed_lot = position.lot,
            reason = reason.as_str(),
            "position closed"
        );

        match self.wait_until_ready_to_reopen(&position.side).await {
            Ok(()) => {}
            Err(e) => {
                // False reconcile / stale fill can leave our new TP on the book while
                // memory thinks we must reopen — re-attach instead of crashing the cycle.
                if self.client.config().is_spot() {
                    let limits = self.client.get_pending_orders().await.unwrap_or_default();
                    if let Some(pos) =
                        self.try_restore_from_tp_limit(position.side.clone(), &limits)
                    {
                        // Same as startup restore: open/SL from TP order; do not touch TP.
                        warn!(
                            error = %e,
                            lot = pos.lot,
                            open = pos.open_price,
                            tp = pos.tp_price,
                            sl = pos.sl_price,
                            tp_order = ?pos.tp_order_id,
                            "reopen blocked by existing bot TP — restoring it (open/SL from order, TP untouched)"
                        );
                        self.watch_tp(pos.tp_order_id.as_deref()).await;
                        let mut st = self.state.lock().await;
                        st.current_lot = pos.lot;
                        st.sl_frozen = false;
                        st.set_position(pos);
                        drop(st);
                        self.arm_tp_guard().await;
                        return Ok(());
                    }
                }
                return Err(e).context("orders still open before reopen");
            }
        }

        // After TP: sell leftover base of the CLOSED market, then maybe switch pair.
        if self.client.config().is_spot() && matches!(reason, CloseReason::TakeProfit) {
            if let Err(e) = self.client.drain_spot_base_tail(&self.meta()).await {
                warn!(error = %e, "drain base tail after TP failed");
            }
            if !self.select_market_if_idle(true).await {
                warn!("spot reopen: no still-nervous market after TP — stay idle");
                return Ok(());
            }
        }

        let next_lot = {
            let st = self.state.lock().await;
            if self.client.config().is_spot() {
                info!(
                    reason = reason.as_str(),
                    closed_lot = position.lot,
                    next_lot = st.initial_lot,
                    market = %self.client.market(),
                    "spot reopen: NEW cycle at working min lot (exchange_min × LOT_COEFF)"
                );
                st.initial_lot
            } else {
                st.next_lot_after_close(position.lot, reason)?
            }
        };
        {
            let mut st = self.state.lock().await;
            st.current_lot = next_lot;
            st.sl_frozen = false;
        }

        info!(
            side = position.side.label(),
            reason = reason.as_str(),
            next_lot,
            market = %self.client.market(),
            "reopening one-sided martingale entry"
        );
        self.open_position(position.side, next_lot).await?;
        Ok(())
    }
}

fn bot_tp_client_id_base_for(side: &Side) -> String {
    format!("{}_tp", side_prefix(side))
}

fn find_by_client_id<'a>(items: &'a [Value], client_id: &str) -> Option<(&'a Value, bool)> {
    for item in items {
        let cid = item.get("client_id").and_then(|v| v.as_str()).unwrap_or("");
        // Exact or prefix match so restore finds `botB_tp` and `botB_tp_0p015373`.
        if is_bot_tp_client_id(cid, client_id) || cid == client_id {
            let is_stop = item.get("stop_id").is_some() || item.get("trigger_price").is_some();
            return Some((item, is_stop));
        }
    }
    None
}

fn id_from(v: &Value, key: &str) -> String {
    v.get(key)
        .map(|x| match x {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        })
        .unwrap_or_default()
}

fn parse_f64_field(v: &Value, key: &str) -> Option<f64> {
    v.get(key)?.as_str()?.parse().ok()
}

fn parse_optional_price(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let v: f64 = t.parse().ok()?;
    if v <= 0.0 {
        None
    } else {
        Some(v)
    }
}
