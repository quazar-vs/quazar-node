//! CoinEx REST + WebSocket client (API v2) for futures and spot.

use crate::config::{BotConfig, MarketKind};
use crate::error::BotError;
use crate::strategy::{Position, Side};
use anyhow::{anyhow, bail, Context, Result};
use flate2::read::GzDecoder;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy)]
pub struct MarketMeta {
    pub tick_size: f64,
    pub min_amount: f64,
    pub amount_precision: u32,
    pub price_precision: u32,
    /// Spot taker fee on bought base (e.g. 0.003 = 0.3%).
    pub taker_fee_rate: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExchangePosition {
    pub position_id: i64,
    pub market: String,
    pub side: String,
    pub open_interest: String,
    pub avg_entry_price: String,
    #[serde(default)]
    pub take_profit_price: String,
    #[serde(default)]
    pub stop_loss_price: String,
    #[serde(default)]
    pub latest_filled_price: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PlacedOrder {
    pub order_id: String,
    pub filled_price: f64,
    pub filled_amount: f64,
}

#[derive(Debug, Clone)]
pub enum WsEvent {
    PositionClosed {
        position_id: String,
        side: Side,
        close_price: f64,
    },
    OrderFinished {
        order_id: String,
        side: String,
        last_filled_price: f64,
        market: String,
        client_id: String,
    },
    /// Public last price from `state.update` (spot) — used for soft TP/SL.
    PriceTick {
        price: f64,
    },
    Connected,
    Disconnected,
}

struct LiveMarket {
    market: String,
    base_ccy: String,
    quote_ccy: String,
    working_min_lot: f64,
}

pub struct CoinExClient {
    http: Client,
    config: BotConfig,
    live: Mutex<LiveMarket>,
}

impl CoinExClient {
    pub fn new(config: BotConfig) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build HTTP client")?;
        let live = LiveMarket {
            market: config.market.clone(),
            base_ccy: config.base_ccy.clone(),
            quote_ccy: config.quote_ccy.clone(),
            working_min_lot: 0.0,
        };
        Ok(Self {
            http,
            config,
            live: Mutex::new(live),
        })
    }

    pub fn config(&self) -> &BotConfig {
        &self.config
    }

    pub fn market(&self) -> String {
        self.live.lock().expect("live market lock").market.clone()
    }

    pub fn base_ccy(&self) -> String {
        self.live.lock().expect("live market lock").base_ccy.clone()
    }

    pub fn quote_ccy(&self) -> String {
        self.live.lock().expect("live market lock").quote_ccy.clone()
    }

    pub fn set_live_market(&self, market: &str, base: &str, quote: &str) {
        let mut g = self.live.lock().expect("live market lock");
        g.market = market.to_string();
        g.base_ccy = base.to_string();
        g.quote_ccy = quote.to_string();
    }

    pub fn set_working_min_lot(&self, lot: f64) {
        self.live.lock().expect("live market lock").working_min_lot = lot;
    }

    pub fn working_min_lot(&self) -> f64 {
        self.live.lock().expect("live market lock").working_min_lot
    }

    fn api_market_type(&self) -> &'static str {
        self.config.market_kind.as_api_str()
    }

    fn timestamp_ms() -> Result<u64> {
        Ok(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock before UNIX_EPOCH")?
            .as_millis() as u64)
    }

    fn sign(&self, prepared: &str) -> Result<String> {
        let mut mac = HmacSha256::new_from_slice(self.config.api_secret.as_bytes())
            .map_err(|e| anyhow!("HMAC key error: {e}"))?;
        mac.update(prepared.as_bytes());
        Ok(hex::encode(mac.finalize().into_bytes()))
    }

    fn sign_http(&self, method: &str, path_with_query: &str, body: &str, ts: u64) -> Result<String> {
        let sign_path = if path_with_query.starts_with("/v2/") {
            path_with_query.to_string()
        } else {
            format!("/v2{path_with_query}")
        };
        let prepared = format!("{method}{sign_path}{body}{ts}");
        self.sign(&prepared)
    }

    fn sign_ws(&self, ts: u64) -> Result<String> {
        self.sign(&ts.to_string())
    }

    async fn request<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        path: &str,
        query: Option<&str>,
        body: Option<Value>,
    ) -> Result<T> {
        let path_with_query = match query {
            Some(q) if !q.is_empty() => format!("{path}?{q}"),
            _ => path.to_string(),
        };
        let body_str = match &body {
            Some(v) => serde_json::to_string(v)?,
            None => String::new(),
        };
        let ts = Self::timestamp_ms()?;
        let sign = self.sign_http(method, &path_with_query, &body_str, ts)?;
        let url = format!("{}{}", self.config.rest_base_url, path_with_query);
        let mut req = match method {
            "GET" => self.http.get(&url),
            "POST" => self.http.post(&url),
            "DELETE" => self.http.delete(&url),
            other => bail!("unsupported HTTP method: {other}"),
        };
        req = req
            .header("X-COINEX-KEY", &self.config.api_key)
            .header("X-COINEX-SIGN", sign)
            .header("X-COINEX-TIMESTAMP", ts.to_string())
            .header("Content-Type", "application/json");
        if let Some(v) = body {
            req = req.json(&v);
        }
        let resp = req.send().await.context("HTTP request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("failed to read response body")?;
        debug!(%status, %path, body = %text, "CoinEx HTTP response");
        let envelope: Value =
            serde_json::from_str(&text).with_context(|| format!("invalid JSON: {text}"))?;
        let code = envelope.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        let message = envelope
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string();
        if code != 0 {
            return Err(BotError::Api { code, message }.into());
        }
        let data_value = envelope
            .get("data")
            .cloned()
            .ok_or_else(|| anyhow!("CoinEx response missing data field"))?;
        serde_json::from_value::<T>(data_value)
            .with_context(|| format!("failed to deserialize response data: {text}"))
    }

    pub async fn get_market_meta(&self) -> Result<MarketMeta> {
        let path = match self.config.market_kind {
            MarketKind::Futures => "/futures/market",
            MarketKind::Spot => "/spot/market",
        };
        let query = format!("market={}", self.market());
        let data: Vec<Value> = self.request("GET", path, Some(&query), None).await?;
        let first = data
            .first()
            .ok_or_else(|| anyhow!("market {} not found", self.market()))?;
        let amount_precision = first
            .get("base_ccy_precision")
            .and_then(|v| v.as_u64())
            .unwrap_or(8) as u32;
        let price_precision = first
            .get("quote_ccy_precision")
            .and_then(|v| v.as_u64())
            .unwrap_or(2) as u32;
        let tick_size = first
            .get("tick_size")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or_else(|| 10f64.powi(-(price_precision as i32)));
        let min_amount = first
            .get("min_amount")
            .and_then(|v| v.as_str())
            .unwrap_or("0")
            .parse::<f64>()
            .unwrap_or(0.0);
        let taker_fee_rate = first
            .get("taker_fee_rate")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.003);
        Ok(MarketMeta {
            tick_size,
            min_amount,
            amount_precision,
            price_precision,
            taker_fee_rate,
        })
    }

    pub async fn get_spot_tickers(&self) -> Result<Vec<Value>> {
        self.request("GET", "/spot/ticker", None, None).await
    }

    pub async fn get_spot_depth(&self, market: &str) -> Result<Value> {
        let query = format!("market={market}&limit=5&interval=0");
        self.request("GET", "/spot/depth", Some(&query), None).await
    }

    pub async fn get_spot_kline(&self, market: &str, period: &str, limit: u32) -> Result<Vec<Value>> {
        let query = format!("market={market}&period={period}&limit={limit}");
        let data: Value = self.request("GET", "/spot/kline", Some(&query), None).await?;
        if let Some(arr) = data.as_array() {
            return Ok(arr.clone());
        }
        Ok(Vec::new())
    }

    pub async fn get_all_pending_orders(&self) -> Result<Vec<Value>> {
        let path = if self.config.is_spot() {
            "/spot/pending-order"
        } else {
            "/futures/pending-order"
        };
        let query = format!(
            "market_type={}&page=1&limit=100",
            self.api_market_type()
        );
        let data: Value = self.request("GET", path, Some(&query), None).await?;
        if let Some(arr) = data.as_array() {
            return Ok(arr.clone());
        }
        if let Some(arr) = data.get("records").and_then(|v| v.as_array()) {
            return Ok(arr.clone());
        }
        Ok(Vec::new())
    }

    pub async fn get_last_price(&self) -> Result<f64> {
        let path = match self.config.market_kind {
            MarketKind::Futures => "/futures/ticker",
            MarketKind::Spot => "/spot/ticker",
        };
        let query = format!("market={}", self.market());
        let data: Vec<Value> = self.request("GET", path, Some(&query), None).await?;
        let last = data
            .first()
            .and_then(|v| v.get("last"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing last price"))?;
        last.parse().context("parse last price")
    }

    pub async fn get_available_balance(&self, ccy: &str) -> Result<f64> {
        Ok(self.get_balance_parts(ccy).await?.0)
    }

    /// Returns `(available, frozen)` for a spot/futures asset.
    pub async fn get_balance_parts(&self, ccy: &str) -> Result<(f64, f64)> {
        let path = match self.config.market_kind {
            MarketKind::Futures => "/assets/futures/balance",
            MarketKind::Spot => "/assets/spot/balance",
        };
        let data: Vec<Value> = self.request("GET", path, None, None).await?;
        for item in data {
            if item.get("ccy").and_then(|v| v.as_str()) == Some(ccy) {
                let available = item
                    .get("available")
                    .and_then(|v| v.as_str())
                    .unwrap_or("0")
                    .parse()
                    .context("parse available balance")?;
                let frozen = item
                    .get("frozen")
                    .and_then(|v| v.as_str())
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0.0);
                return Ok((available, frozen));
            }
        }
        Ok((0.0, 0.0))
    }

    /// Cancel every pending limit + stop on the configured spot market.
    pub async fn cancel_all_spot_pending(&self) -> Result<usize> {
        if !self.config.is_spot() {
            return Ok(0);
        }
        let limits = self.get_pending_orders().await.unwrap_or_default();
        let stops = self.get_pending_stop_orders().await.unwrap_or_default();
        let mut cancelled = 0usize;
        for item in &limits {
            let id = extract_id(item, "order_id");
            if id.is_empty() {
                continue;
            }
            match self.cancel_order(&id).await {
                Ok(()) => {
                    cancelled += 1;
                    info!(%id, "cancelled pending limit order");
                }
                Err(e) => warn!(error = %e, %id, "failed cancelling pending limit"),
            }
        }
        for item in &stops {
            let id = extract_id(item, "stop_id");
            if id.is_empty() {
                continue;
            }
            match self.cancel_stop_order(&id).await {
                Ok(()) => {
                    cancelled += 1;
                    info!(%id, "cancelled pending stop order");
                }
                Err(e) => warn!(error = %e, %id, "failed cancelling pending stop"),
            }
        }
        Ok(cancelled)
    }

    pub async fn get_open_positions(&self) -> Result<Vec<ExchangePosition>> {
        if self.config.is_spot() {
            return Ok(Vec::new());
        }
        let query = format!(
            "market={}&market_type=FUTURES&page=1&limit=100",
            self.market()
        );
        let data: Vec<ExchangePosition> = self
            .request("GET", "/futures/pending-position", Some(&query), None)
            .await?;
        Ok(data
            .into_iter()
            .filter(|p| {
                p.market.eq_ignore_ascii_case(&self.market())
                    && p.open_interest.parse::<f64>().unwrap_or(0.0) > 0.0
            })
            .collect())
    }

    pub async fn get_pending_orders(&self) -> Result<Vec<Value>> {
        let path = if self.config.is_spot() {
            "/spot/pending-order"
        } else {
            "/futures/pending-order"
        };
        let query = format!(
            "market={}&market_type={}&page=1&limit=100",
            self.market(),
            self.api_market_type()
        );
        self.request("GET", path, Some(&query), None).await
    }

    pub async fn get_pending_stop_orders(&self) -> Result<Vec<Value>> {
        if !self.config.is_spot() {
            return Ok(Vec::new());
        }
        let query = format!(
            "market={}&market_type=SPOT&page=1&limit=100",
            self.market()
        );
        self.request("GET", "/spot/pending-stop-order", Some(&query), None)
            .await
    }

    pub async fn place_market_order(&self, side: &Side, amount: f64) -> Result<PlacedOrder> {
        let amount_str = format_amount(amount);
        let (path, body) = if self.config.is_spot() {
            (
                "/spot/order",
                json!({
                    "market": self.market(),
                    "market_type": "SPOT",
                    "side": side.as_order_side(),
                    "type": "market",
                    "amount": amount_str,
                    "ccy": self.base_ccy(),
                }),
            )
        } else {
            (
                "/futures/order",
                json!({
                    "market": self.market(),
                    "market_type": "FUTURES",
                    "side": side.as_order_side(),
                    "type": "market",
                    "amount": amount_str,
                }),
            )
        };
        info!(
            side = side.label(),
            amount = %amount_str,
            market = %self.market(),
            mode = self.config.market_kind.label(),
            "placing market order"
        );
        let data: Value = self.request("POST", path, None, Some(body)).await?;
        Ok(parse_placed_order(&data, amount))
    }

    pub async fn place_limit_order(
        &self,
        side: &Side,
        amount: f64,
        price: f64,
        client_id: &str,
    ) -> Result<String> {
        let body = json!({
            "market": self.market(),
            "market_type": "SPOT",
            "side": side.as_order_side(),
            "type": "limit",
            "amount": format_amount(amount),
            "price": format_price(price),
            "client_id": client_id,
        });
        info!(
            %client_id,
            side = side.label(),
            amount,
            price,
            market = %self.market(),
            "placing spot limit order"
        );
        let data: Value = self.request("POST", "/spot/order", None, Some(body)).await?;
        Ok(extract_id(&data, "order_id"))
    }

    /// Place limit with retries. Cycle is not successful until this returns Ok.
    pub async fn place_limit_order_retry(
        &self,
        side: &Side,
        amount: f64,
        price: f64,
        client_id: &str,
        attempts: u32,
    ) -> Result<String> {
        let mut last_err = None;
        for i in 1..=attempts.max(1) {
            match self.place_limit_order(side, amount, price, client_id).await {
                Ok(id) => return Ok(id),
                Err(e) => {
                    warn!(
                        attempt = i,
                        attempts,
                        error = %e,
                        amount,
                        price,
                        "place limit failed — retrying"
                    );
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(250 * u64::from(i))).await;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("place limit failed")))
            .context("place limit exhausted retries")
    }

    /// Amend an existing spot limit in place (same order stays on the book).
    /// CoinEx `POST /spot/modify-order` — prefer this over cancel+place on soft SL.
    ///
    /// Verifies that the response `amount`/`price` match the request. CoinEx can
    /// return code=0 after a price-only amend when amount increase is rejected
    /// (insufficient available while base is frozen in the order) — that must not
    /// be treated as a successful lot×2.
    pub async fn modify_spot_limit_order(
        &self,
        order_id: &str,
        amount: f64,
        price: f64,
    ) -> Result<String> {
        let order_id_num: u64 = order_id
            .parse()
            .with_context(|| format!("invalid spot order_id for modify: {order_id}"))?;
        let body = json!({
            "market": self.market(),
            "market_type": "SPOT",
            "order_id": order_id_num,
            "amount": format_amount(amount),
            "price": format_price(price),
        });
        info!(
            %order_id,
            amount,
            price,
            market = %self.market(),
            "modifying spot limit order in place"
        );
        let data: Value = self
            .request("POST", "/spot/modify-order", None, Some(body))
            .await?;
        let new_id = extract_id(&data, "order_id");
        let id = if new_id.is_empty() {
            order_id.to_string()
        } else {
            new_id
        };
        self.verify_order_amount_price(&data, amount, price)
            .with_context(|| {
                format!("modify order {order_id} did not apply requested amount/price")
            })?;
        info!(
            %id,
            amount,
            price,
            resp_amount = value_f64(&data, "amount").unwrap_or(0.0),
            resp_price = value_f64(&data, "price").unwrap_or(0.0),
            "spot limit modify confirmed (amount+price)"
        );
        Ok(id)
    }

    /// Wait until `available + frozen` covers `need` (post market-buy settlement).
    pub async fn wait_spot_base_cover(&self, need: f64, attempts: u32) -> Result<(f64, f64)> {
        let base = self.base_ccy().clone();
        let mut last = (0.0_f64, 0.0_f64);
        for i in 1..=attempts.max(1) {
            last = self.get_balance_parts(&base).await?;
            let inv = last.0 + last.1;
            if inv + f64::EPSILON >= need {
                if i > 1 {
                    info!(
                        available = last.0,
                        frozen = last.1,
                        need,
                        attempt = i,
                        "spot base cover ready after wait"
                    );
                }
                return Ok(last);
            }
            warn!(
                available = last.0,
                frozen = last.1,
                need,
                attempt = i,
                attempts,
                "spot base cover not ready — waiting for balance settle"
            );
            tokio::time::sleep(std::time::Duration::from_millis(250 * u64::from(i))).await;
        }
        bail!(
            "spot base cover timeout: available={} frozen={} need={}",
            last.0,
            last.1,
            need
        )
    }

    /// Hard gate: `available + frozen` must be ≥ `need` or Err (no place/modify on credit).
    pub async fn require_spot_base_cover(&self, need: f64) -> Result<(f64, f64)> {
        self.wait_spot_base_cover(need, 8).await.with_context(|| {
            format!("KOMA cover required before place/modify: need={need}")
        })
    }

    /// Re-read pending order and confirm amount/price (post-modify / post-place).
    pub async fn confirm_pending_order_size(
        &self,
        order_id: &str,
        amount: f64,
        price: f64,
    ) -> Result<()> {
        let pending = self.get_pending_orders().await?;
        let item = pending
            .iter()
            .find(|o| extract_id(o, "order_id") == order_id)
            .ok_or_else(|| anyhow!("pending order {order_id} not found after amend/place"))?;
        self.verify_order_amount_price(item, amount, price)
            .with_context(|| format!("pending order {order_id} size mismatch after amend/place"))
    }

    fn verify_order_amount_price(&self, data: &Value, amount: f64, price: f64) -> Result<()> {
        let got_amount = value_f64(data, "amount").unwrap_or(0.0);
        let got_price = value_f64(data, "price").unwrap_or(0.0);
        let amt_tol = (amount.abs() * 0.002).max(1e-8);
        let px_tol = (price.abs() * 0.0001).max(1e-12);
        if (got_amount - amount).abs() > amt_tol {
            error!(
                requested = amount,
                got = got_amount,
                "modify/place amount NOT applied on exchange"
            );
            bail!(
                "order amount mismatch: requested {amount}, exchange {got_amount}"
            );
        }
        if price > 0.0 && (got_price - price).abs() > px_tol {
            error!(
                requested = price,
                got = got_price,
                "modify/place price NOT applied on exchange"
            );
            bail!("order price mismatch: requested {price}, exchange {got_price}");
        }
        Ok(())
    }

    /// Place or modify spot TP **only after** KOMA inventory covers `amount`.
    /// Caller must already have done shortage buy if needed; this re-checks and refuses
    /// to place/modify on credit.
    pub async fn ensure_spot_tp_order(
        &self,
        existing_tp_id: Option<&str>,
        exit_side: &Side,
        amount: f64,
        price: f64,
        client_id: &str,
        meta: &MarketMeta,
    ) -> Result<String> {
        let amount = round_amount(amount, meta);
        if amount < meta.min_amount {
            bail!(
                "cannot place TP: amount {amount} < min_amount {}",
                meta.min_amount
            );
        }

        // Invariant: never place/modify until KOMA >= amount.
        let (avail, frozen) = self.require_spot_base_cover(amount).await?;
        info!(
            available = avail,
            frozen,
            amount,
            price,
            "TP place/modify allowed — KOMA cover verified"
        );

        if let Some(oid) = existing_tp_id {
            for i in 1..=5u32 {
                // Re-verify before every modify attempt (balance can race).
                if let Err(e) = self.require_spot_base_cover(amount).await {
                    error!(
                        attempt = i,
                        error = %e,
                        amount,
                        "ensure TP: KOMA < amount — abort modify (no credit)"
                    );
                    return Err(e).context("refusing modify: KOMA below TP amount");
                }
                match self.modify_spot_limit_order(oid, amount, price).await {
                    Ok(id) => {
                        match self.confirm_pending_order_size(&id, amount, price).await {
                            Ok(()) => return Ok(id),
                            Err(e) => {
                                error!(
                                    error = %e,
                                    %id,
                                    amount,
                                    price,
                                    "modify reported ok but pending amount/price wrong — retry"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        if msg.contains("3600") || msg.to_ascii_lowercase().contains("not found") {
                            warn!(%oid, error = %e, "modify TP: order gone — will re-place if needed");
                            break;
                        }
                        warn!(
                            attempt = i,
                            error = %e,
                            %oid,
                            amount,
                            price,
                            "modify TP failed — retry"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(250 * u64::from(i)))
                            .await;
                    }
                }
            }
            // Alternate bot TP on book (id changed after prior modify).
            if let Ok(pending) = self.get_pending_orders().await {
                for item in &pending {
                    let cid = item.get("client_id").and_then(|v| v.as_str()).unwrap_or("");
                    if !is_bot_tp_client_id(cid, client_id) {
                        continue;
                    }
                    let other = extract_id(item, "order_id");
                    if other.is_empty() || other == oid {
                        continue;
                    }
                    self.require_spot_base_cover(amount)
                        .await
                        .context("refusing alternate modify: KOMA below TP amount")?;
                    match self.modify_spot_limit_order(&other, amount, price).await {
                        Ok(id) => {
                            if self.confirm_pending_order_size(&id, amount, price).await.is_ok() {
                                return Ok(id);
                            }
                            error!(%id, amount, "alternate TP modify amount not confirmed");
                        }
                        Err(e) => warn!(error = %e, %other, "modify alternate TP failed"),
                    }
                }
                for item in &pending {
                    let cid = item.get("client_id").and_then(|v| v.as_str()).unwrap_or("");
                    if !is_bot_tp_client_id(cid, client_id) {
                        continue;
                    }
                    let id = extract_id(item, "order_id");
                    if id.is_empty() {
                        continue;
                    }
                    warn!(%id, amount, price, "modify TP exhausted — cancel+place fallback");
                    let _ = self.cancel_order(&id).await;
                }
            } else {
                warn!(%oid, amount, price, "modify TP exhausted — cancel+place fallback");
                let _ = self.cancel_order(oid).await;
            }
        }

        // Fresh place — still require cover (cancel freed frozen; buy must have settled).
        self.require_spot_base_cover(amount)
            .await
            .context("refusing place TP: KOMA below amount")?;
        let id = self
            .place_limit_order_retry(exit_side, amount, price, client_id, 4)
            .await?;
        self.confirm_pending_order_size(&id, amount, price)
            .await
            .context("place TP amount/price not confirmed on book")?;
        info!(%id, amount, price, "TP placed — amount confirmed on book");
        Ok(id)
    }

    /// Sell all free (available) base ≥ min_amount without touching a frozen TP.
    pub async fn sell_spot_available_excess(&self, meta: &MarketMeta) -> Result<()> {
        let base = &self.base_ccy();
        let available = self.get_available_balance(base).await?;
        let sell_qty = round_amount(available, meta);
        if sell_qty >= meta.min_amount {
            info!(
                available,
                sell_qty,
                base = %base,
                "selling free base excess (TP stays on book)"
            );
            self.place_market_order(&Side::Sell, sell_qty).await?;
        } else if sell_qty > 0.0 {
            info!(
                available,
                sell_qty,
                "free base dust below min_amount left on balance"
            );
        }
        Ok(())
    }

    pub async fn place_stop_market(
        &self,
        side: &Side,
        amount: f64,
        trigger_price: f64,
        client_id: &str,
    ) -> Result<String> {
        let body = json!({
            "market": self.market(),
            "market_type": "SPOT",
            "side": side.as_order_side(),
            "type": "market",
            "amount": format_amount(amount),
            "ccy": self.base_ccy(),
            "trigger_price": format_price(trigger_price),
            "client_id": client_id,
        });
        let data: Value = self
            .request("POST", "/spot/stop-order", None, Some(body))
            .await?;
        Ok(extract_id(&data, "stop_id"))
    }

    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        let body = json!({
            "market": self.market(),
            "market_type": self.api_market_type(),
            "order_id": order_id.parse::<i64>().unwrap_or(0),
        });
        let path = if self.config.is_spot() {
            "/spot/cancel-order"
        } else {
            "/futures/cancel-order"
        };
        let _: Value = self.request("POST", path, None, Some(body)).await?;
        Ok(())
    }

    pub async fn cancel_stop_order(&self, stop_id: &str) -> Result<()> {
        let body = json!({
            "market": self.market(),
            "market_type": "SPOT",
            "stop_id": stop_id.parse::<i64>().unwrap_or(0),
        });
        let _: Value = self
            .request("POST", "/spot/cancel-stop-order", None, Some(body))
            .await?;
        Ok(())
    }

    pub async fn set_take_profit(&self, side: &Side, price: f64) -> Result<()> {
        if self.config.is_spot() {
            bail!("set_take_profit is futures-only");
        }
        let body = json!({
            "market": self.market(),
            "market_type": "FUTURES",
            "side": side.as_position_side(),
            "take_profit_type": self.config.tp_sl_trigger,
            "take_profit_price": format_price(price),
        });
        match self
            .request::<Value>("POST", "/futures/set-position-take-profit", None, Some(body))
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                warn!(error = %e, "set TP with side failed, retrying without side");
                let body = json!({
                    "market": self.market(),
                    "market_type": "FUTURES",
                    "take_profit_type": self.config.tp_sl_trigger,
                    "take_profit_price": format_price(price),
                });
                self.request::<Value>("POST", "/futures/set-position-take-profit", None, Some(body))
                    .await?;
                Ok(())
            }
        }
    }

    pub async fn set_stop_loss(&self, side: &Side, price: f64) -> Result<()> {
        if self.config.is_spot() {
            bail!("set_stop_loss is futures-only");
        }
        let body = json!({
            "market": self.market(),
            "market_type": "FUTURES",
            "side": side.as_position_side(),
            "stop_loss_type": self.config.tp_sl_trigger,
            "stop_loss_price": format_price(price),
        });
        match self
            .request::<Value>("POST", "/futures/set-position-stop-loss", None, Some(body))
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                warn!(error = %e, "set SL with side failed, retrying without side");
                let body = json!({
                    "market": self.market(),
                    "market_type": "FUTURES",
                    "stop_loss_type": self.config.tp_sl_trigger,
                    "stop_loss_price": format_price(price),
                });
                self.request::<Value>("POST", "/futures/set-position-stop-loss", None, Some(body))
                    .await?;
                Ok(())
            }
        }
    }

    pub async fn open_position_with_tp_sl(
        &self,
        side: &Side,
        lot: f64,
        tp_price: f64,
        sl_price: f64,
        meta: &MarketMeta,
    ) -> Result<Position> {
        if self.config.is_spot() {
            self.open_spot_leg(side, lot, tp_price, sl_price, meta).await
        } else {
            self.open_futures_leg(side, lot, tp_price, sl_price, meta).await
        }
    }

    async fn open_futures_leg(
        &self,
        side: &Side,
        lot: f64,
        tp_price: f64,
        sl_price: f64,
        meta: &MarketMeta,
    ) -> Result<Position> {
        let lot = round_amount(lot, meta);
        if lot < meta.min_amount {
            bail!("lot {lot} below market min_amount {}", meta.min_amount);
        }
        let order = self.place_market_order(side, lot).await?;
        let mut open_price = order.filled_price;
        if open_price <= 0.0 {
            tokio::time::sleep(Duration::from_millis(400)).await;
            open_price = self.find_position_price(side).await.unwrap_or(0.0);
            if open_price <= 0.0 {
                open_price = self.get_last_price().await?;
            }
        }
        let tp_price = round_price(tp_price, meta);
        let sl_price = round_price(sl_price, meta);
        self.set_take_profit(side, tp_price).await?;
        self.set_stop_loss(side, sl_price).await?;
        let position_id = self
            .find_position_id(side)
            .await
            .unwrap_or_else(|_| order.order_id.clone());
        Ok(Position {
            id: position_id,
            side: side.clone(),
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

    /// Spot BUY: buy only when free base < working lot.
    /// Gross = ceil(shortage / (1 − taker_fee)); never bump a tiny fee gap to min_amount.
    pub async fn ensure_spot_base_for_lot(
        &self,
        lot: f64,
        meta: &MarketMeta,
    ) -> Result<(f64, Option<f64>)> {
        let lot = round_amount(lot, meta);
        let base = &self.base_ccy();
        let available = self.get_available_balance(base).await?;
        if available + f64::EPSILON >= lot {
            info!(
                available,
                lot,
                base = %base,
                "spot open: enough free base for lot — no market buy"
            );
            return Ok((available, None));
        }
        let need = lot - available;
        let Some(buy_qty) = spot_buy_gross_for_shortage(available, lot, meta) else {
            info!(
                available,
                lot,
                need,
                base = %base,
                "spot open: fee-padded gross below min_amount — skip buy"
            );
            return Ok((available, None));
        };
        info!(
            available,
            lot,
            need,
            buy_qty,
            fee = meta.taker_fee_rate,
            base = %base,
            "spot open: market-buying shortage (gross covers taker fee)"
        );
        let order = self.place_market_order(&Side::Buy, buy_qty).await?;
        let fill = if order.filled_price > 0.0 {
            Some(order.filled_price)
        } else {
            None
        };
        let after = self.get_available_balance(base).await?;
        Ok((after, fill))
    }

    pub async fn align_spot_base_to_lot(&self, lot: f64, meta: &MarketMeta) -> Result<f64> {
        Ok(self.ensure_spot_base_for_lot(lot, meta).await?.0)
    }

    /// Sell all free base (≥ min_amount). Used after TP so no KOMA tail remains.
    pub async fn drain_spot_base_tail(&self, meta: &MarketMeta) -> Result<()> {
        let base = &self.base_ccy();
        let available = self.get_available_balance(base).await?;
        let sell_qty = round_amount(available, meta);
        if sell_qty >= meta.min_amount {
            info!(
                available,
                sell_qty,
                base = %base,
                "spot TP cleanup: selling remaining base tail"
            );
            self.place_market_order(&Side::Sell, sell_qty).await?;
        } else if sell_qty > 0.0 {
            info!(
                available,
                sell_qty,
                "spot TP cleanup: dust below min_amount left on balance"
            );
        }
        Ok(())
    }

    async fn open_spot_leg(
        &self,
        side: &Side,
        lot: f64,
        _tp_price: f64,
        _sl_price: f64,
        meta: &MarketMeta,
    ) -> Result<Position> {
        let requested = round_amount(lot, meta);
        if requested < meta.min_amount {
            bail!(
                "lot {requested} below market min_amount {}",
                meta.min_amount
            );
        }

        let mut fill_anchor = 0.0_f64;
        let mut bought = false;
        let mut entry_id = format!("spot_tp_only_{}", side.label());
        let mut lot = requested;

        match side {
            Side::Buy => {
                // 1) inventory check + shortage buy first
                // 2) only then TP/SL from anchor (buy fill OR current last)
                let (after, buy_fill) = self.ensure_spot_base_for_lot(requested, meta).await?;
                entry_id = format!("spot_open_{}", side.label());
                let sellable = round_amount(after, meta);
                if sellable < meta.min_amount {
                    bail!(
                        "available base {after} < min_amount {} — cannot place TP",
                        meta.min_amount
                    );
                }
                // TP = working lot only. Surplus from fee-pad stays free as dust.
                lot = requested;
                if sellable + f64::EPSILON < requested {
                    warn!(
                        requested,
                        sellable,
                        "spot open: sellable below working lot after buy"
                    );
                } else if sellable > requested + f64::EPSILON {
                    info!(
                        requested,
                        sellable,
                        surplus = sellable - requested,
                        "spot open: surplus left free (TP on working lot)"
                    );
                }
                if let Some(fp) = buy_fill.filter(|p| *p > 0.0) {
                    fill_anchor = fp;
                    bought = true;
                }
            }
            Side::Sell => {
                let order = self.place_market_order(side, requested).await?;
                entry_id = order.order_id;
                fill_anchor = order.filled_price;
                bought = fill_anchor > 0.0;
                lot = requested;
            }
        }

        // Anchor: shortage-buy fill price if we bought; otherwise current market last.
        let last = self.get_last_price().await?;
        let anchor = if bought && fill_anchor > 0.0 {
            fill_anchor
        } else {
            last
        };
        let buy = matches!(side, Side::Buy);
        let (raw_tp, raw_sl) = self.config.calc_tp_sl(buy, anchor);
        let tp_price = round_price(raw_tp, meta);
        let sl_price = round_price(raw_sl, meta);
        let anchor_rounded = round_price(anchor, meta);

        info!(
            bought,
            fill_anchor,
            last,
            anchor = anchor_rounded,
            tp_price,
            sl_price,
            "spot open: TP/SL from anchor after inventory step"
        );

        // Hard gate: KOMA must cover lot before placing TP.
        self.require_spot_base_cover(lot)
            .await
            .context("refusing open TP place: KOMA below lot after inventory sync")?;

        let exit_side = side.opposite();
        let tp_cid = make_bot_tp_client_id(side_prefix(side), anchor_rounded);
        let tp_order_id = self
            .ensure_spot_tp_order(
                None,
                &exit_side,
                lot,
                tp_price,
                &tp_cid,
                meta,
            )
            .await
            .context("failed to place limit TP after entry — cycle not open")?;

        info!(
            side = side.label(),
            id = %entry_id,
            open_price = anchor_rounded,
            tp_price,
            sl_price,
            lot,
            %tp_order_id,
            client_id = %tp_cid,
            "spot entry open: one limit TP on working lot"
        );
        Ok(Position {
            id: entry_id,
            side: side.clone(),
            open_price: anchor_rounded,
            tp_price,
            sl_price,
            lot,
            is_open: true,
            tp_order_id: Some(tp_order_id),
            sl_order_id: None,
            tp_is_stop: false,
            sl_is_stop: false,
        })
    }

    /// Cancel the single limit TP if present.
    pub async fn cancel_spot_tp(&self, position: &Position) -> Result<()> {
        let Some(tp) = &position.tp_order_id else {
            return Ok(());
        };
        if position.tp_is_stop {
            self.cancel_stop_order(tp).await
        } else {
            self.cancel_order(tp).await
        }
    }

    /// Close spot inventory with one opposite-side market order.
    pub async fn close_spot_market(&self, side: &Side, lot: f64) -> Result<PlacedOrder> {
        let exit_side = side.opposite();
        self.place_market_order(&exit_side, lot).await
    }

    /// Cancel every pending stop on the spot market (legacy paired SL).
    pub async fn cancel_all_spot_stops(&self) -> Result<usize> {
        if !self.config.is_spot() {
            return Ok(0);
        }
        let stops = self.get_pending_stop_orders().await.unwrap_or_default();
        let mut cancelled = 0usize;
        for item in &stops {
            let id = extract_id(item, "stop_id");
            if id.is_empty() {
                continue;
            }
            match self.cancel_stop_order(&id).await {
                Ok(()) => {
                    cancelled += 1;
                    info!(%id, "cancelled legacy stop order");
                }
                Err(e) => warn!(error = %e, %id, "failed cancelling stop"),
            }
        }
        Ok(cancelled)
    }

    async fn find_position_id(&self, side: &Side) -> Result<String> {
        for p in self.get_open_positions().await? {
            if Side::from_position_side(&p.side) == Some(side.clone()) {
                return Ok(p.position_id.to_string());
            }
        }
        bail!("open position for side {} not found", side.label())
    }

    async fn find_position_price(&self, side: &Side) -> Result<f64> {
        for p in self.get_open_positions().await? {
            if Side::from_position_side(&p.side) == Some(side.clone()) {
                return p.avg_entry_price.parse().context("parse avg_entry_price");
            }
        }
        bail!("position price not found")
    }

    pub async fn ensure_balance_for_lot(&self, side: &Side, lot: f64, price: f64) -> Result<()> {
        if self.config.is_spot() {
            match side {
                Side::Buy => {
                    let required = lot * price * 1.05;
                    let available = self.get_available_balance(&self.quote_ccy()).await?;
                    if available < required {
                        return Err(BotError::InsufficientBalance { lot, available }.into());
                    }
                }
                Side::Sell => {
                    let available = self.get_available_balance(&self.base_ccy()).await?;
                    if available < lot {
                        return Err(BotError::InsufficientBalance { lot, available }.into());
                    }
                }
            }
            return Ok(());
        }
        let required = (lot * price) / self.config.leverage as f64 * 1.05;
        let available = self.get_available_balance("USDT").await?;
        if available < required {
            return Err(BotError::InsufficientBalance { lot, available }.into());
        }
        Ok(())
    }

    pub async fn run_private_ws(
        &self,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        tx: mpsc::Sender<WsEvent>,
    ) -> Result<()> {
        loop {
            if *shutdown.borrow() {
                break;
            }
            info!(url = %self.config.ws_url, mode = self.config.market_kind.label(), "connecting WebSocket");
            match self.ws_session(&mut shutdown, &tx).await {
                Ok(()) => info!("websocket session ended cleanly"),
                Err(e) => error!(error = %e, "websocket session error"),
            }
            let _ = tx.send(WsEvent::Disconnected).await;
            if *shutdown.borrow() {
                break;
            }
            warn!("reconnecting websocket in 3s...");
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        Ok(())
    }

    async fn ws_session(
        &self,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
        tx: &mpsc::Sender<WsEvent>,
    ) -> Result<()> {
        let (ws, _) = connect_async(&self.config.ws_url)
            .await
            .map_err(|e| BotError::WebSocket(e.to_string()))?;
        let (mut write, mut read) = ws.split();
        let ts = Self::timestamp_ms()?;
        let signed = self.sign_ws(ts)?;
        write
            .send(Message::Text(
                json!({
                    "id": 1,
                    "method": "server.sign",
                    "params": {
                        "access_id": self.config.api_key,
                        "signed_str": signed,
                        "timestamp": ts,
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .context("send auth")?;
        let order_markets = if self.config.auto_market {
            json!([])
        } else {
            json!([self.market()])
        };
        write
            .send(Message::Text(
                json!({
                    "id": 2,
                    "method": "order.subscribe",
                    "params": { "market_list": order_markets }
                })
                .to_string()
                .into(),
            ))
            .await
            .context("subscribe orders")?;
        if self.config.is_spot() {
            // Soft SL needs last price; stop.subscribe no longer used (no SL orders).
            write
                .send(Message::Text(
                    json!({
                        "id": 3,
                        "method": "state.subscribe",
                        "params": { "market_list": [self.market()] }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .context("subscribe market state")?;
        } else {
            write
                .send(Message::Text(
                    json!({
                        "id": 3,
                        "method": "position.subscribe",
                        "params": { "market_list": [self.market()] }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .context("subscribe positions")?;
        }
        let _ = tx.send(WsEvent::Connected).await;
        let mut ping_interval = tokio::time::interval(Duration::from_secs(20));
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        let _ = write.send(Message::Close(None)).await;
                        break;
                    }
                }
                _ = ping_interval.tick() => {
                    if write.send(Message::Text(json!({"id":999,"method":"server.ping","params":{}}).to_string().into())).await.is_err() {
                        bail!("failed to send ping");
                    }
                }
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Binary(bin))) => {
                            let text = decompress_ws(&bin)?;
                            self.handle_ws_text(&text, tx).await?;
                        }
                        Some(Ok(Message::Text(text))) => {
                            self.handle_ws_text(&text, tx).await?;
                        }
                        Some(Ok(Message::Ping(p))) => { let _ = write.send(Message::Pong(p)).await; }
                        Some(Ok(Message::Close(_))) => { warn!("websocket closed by server"); break; }
                        Some(Err(e)) => return Err(BotError::WebSocket(e.to_string()).into()),
                        None => break,
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_ws_text(&self, text: &str, tx: &mpsc::Sender<WsEvent>) -> Result<()> {
        let value: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, raw = %text, "failed to parse WS message");
                return Ok(());
            }
        };
        if let Some(method) = value.get("method").and_then(|m| m.as_str()) {
            match method {
                "position.update" => {
                    if let Some(event) = parse_position_update(&value) {
                        let _ = tx.send(event).await;
                    }
                }
                "order.update" => {
                    if let Some(event) = parse_order_update(&value) {
                        let _ = tx.send(event).await;
                    }
                }
                "stop.update" => {
                    if let Some(event) = parse_stop_update(&value) {
                        let _ = tx.send(event).await;
                    }
                }
                "state.update" => {
                    if let Some(event) = parse_state_update(&value, &self.market()) {
                        let _ = tx.send(event).await;
                    }
                }
                "server.pong" | "pong" => {}
                other => debug!(method = other, "ignored WS method"),
            }
        } else if let Some(code) = value.get("code").and_then(|c| c.as_i64()) {
            if code != 0 {
                warn!(raw = %text, "WS response error");
            }
        }
        Ok(())
    }
}

fn extract_id(data: &Value, key: &str) -> String {
    data.get(key)
        .map(|v| match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        })
        .unwrap_or_default()
}

fn value_f64(data: &Value, key: &str) -> Option<f64> {
    data.get(key).and_then(|v| match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    })
}

fn parse_placed_order(data: &Value, fallback_amount: f64) -> PlacedOrder {
    let order_id = extract_id(data, "order_id");
    let filled_price = data
        .get("last_filled_price")
        .or_else(|| data.get("last_fill_price"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|p| *p > 0.0)
        .or_else(|| {
            let filled_value = data
                .get("filled_value")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let filled_amount = data
                .get("filled_amount")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            if filled_amount > 0.0 {
                Some(filled_value / filled_amount)
            } else {
                None
            }
        })
        .unwrap_or(0.0);
    let filled_amount = data
        .get("filled_amount")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(fallback_amount);
    PlacedOrder {
        order_id,
        filled_price,
        filled_amount,
    }
}

fn parse_position_update(value: &Value) -> Option<WsEvent> {
    let data = value.get("data")?;
    let event_name = data.get("event").and_then(|e| e.as_str()).unwrap_or("");
    let pos = data.get("position")?;
    let side = Side::from_position_side(pos.get("side")?.as_str()?)?;
    let position_id = extract_id(pos, "position_id");
    let open_interest = pos
        .get("open_interest")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let close_price = pos
        .get("latest_filled_price")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|p| *p > 0.0)
        .or_else(|| {
            pos.get("settle_price")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
        })
        .unwrap_or(0.0);
    let is_closed = matches!(event_name, "close" | "sys_close" | "liq" | "adl")
        || open_interest <= f64::EPSILON;
    if is_closed {
        Some(WsEvent::PositionClosed {
            position_id,
            side,
            close_price,
        })
    } else {
        None
    }
}

fn parse_order_update(value: &Value) -> Option<WsEvent> {
    let data = value.get("data")?;
    if data.get("event").and_then(|e| e.as_str()).unwrap_or("") != "finish" {
        return None;
    }
    let order = data.get("order")?;
    Some(WsEvent::OrderFinished {
        order_id: extract_id(order, "order_id"),
        side: order.get("side")?.as_str()?.to_string(),
        last_filled_price: order
            .get("last_filled_price")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0),
        market: order.get("market")?.as_str()?.to_string(),
        client_id: order
            .get("client_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

fn parse_state_update(value: &Value, market: &str) -> Option<WsEvent> {
    let data = value.get("data")?;
    let list = data.get("state_list")?.as_array()?;
    for item in list {
        let m = item.get("market").and_then(|v| v.as_str()).unwrap_or("");
        if !m.eq_ignore_ascii_case(market) {
            continue;
        }
        let last = item
            .get("last")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|p| *p > 0.0)?;
        return Some(WsEvent::PriceTick { price: last });
    }
    None
}

fn parse_stop_update(value: &Value) -> Option<WsEvent> {
    let data = value.get("data")?;
    let event_name = data.get("event").and_then(|e| e.as_str()).unwrap_or("");
    if event_name != "active" && event_name != "finish" {
        return None;
    }
    let stop = data.get("stop")?;
    let trigger = stop
        .get("trigger_price")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    Some(WsEvent::OrderFinished {
        order_id: extract_id(stop, "stop_id"),
        side: stop.get("side")?.as_str()?.to_string(),
        last_filled_price: stop
            .get("price")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|p| *p > 0.0)
            .unwrap_or(trigger),
        market: stop.get("market")?.as_str()?.to_string(),
        client_id: stop
            .get("client_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

fn decompress_ws(data: &[u8]) -> Result<String> {
    let mut decoder = GzDecoder::new(data);
    let mut out = String::new();
    decoder
        .read_to_string(&mut out)
        .context("failed to gunzip WS payload")?;
    Ok(out)
}

pub fn round_amount(amount: f64, meta: &MarketMeta) -> f64 {
    let factor = 10f64.powi(meta.amount_precision as i32);
    (amount * factor).floor() / factor
}

pub fn ceil_amount(amount: f64, meta: &MarketMeta) -> f64 {
    let factor = 10f64.powi(meta.amount_precision as i32);
    (amount * factor).ceil() / factor
}

/// Gross market-buy to cover `target` after taker fee on base:
/// `ceil((target − available) / (1 − fee))`. None if no buy needed or gross < min_amount.
pub fn spot_buy_gross_for_shortage(available: f64, target: f64, meta: &MarketMeta) -> Option<f64> {
    if available + f64::EPSILON >= target {
        return None;
    }
    let need = target - available;
    let fee = meta.taker_fee_rate.clamp(0.0, 0.99);
    let gross = ceil_amount(need / (1.0 - fee), meta);
    if gross + f64::EPSILON < meta.min_amount {
        return None;
    }
    Some(gross)
}

/// Smallest whole `min_amount` that covers `amount` (500, 1000, … — no 502.20 tails).
pub fn ceil_amount_to_min(amount: f64, meta: &MarketMeta) -> f64 {
    if amount <= 0.0 || meta.min_amount <= 0.0 {
        return 0.0;
    }
    let steps = (amount / meta.min_amount).ceil().max(1.0);
    round_amount(steps * meta.min_amount, meta)
}

/// Nearest whole `min_amount` (at least one step). Used to keep martingale lots round.
pub fn snap_amount_to_min(amount: f64, meta: &MarketMeta) -> f64 {
    if amount <= 0.0 || meta.min_amount <= 0.0 {
        return round_amount(amount.max(0.0), meta);
    }
    let steps = (amount / meta.min_amount).round().max(1.0);
    round_amount(steps * meta.min_amount, meta)
}

pub fn round_price(price: f64, meta: &MarketMeta) -> f64 {
    if meta.tick_size > 0.0 {
        (price / meta.tick_size).round() * meta.tick_size
    } else {
        let factor = 10f64.powi(meta.price_precision as i32);
        (price * factor).round() / factor
    }
}

fn format_amount(v: f64) -> String {
    trim_float(&format!("{v:.8}"))
}

fn format_price(v: f64) -> String {
    trim_float(&format!("{v:.8}"))
}

fn trim_float(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Spot side prefix used in TP `client_id` (`botB` / `botS`).
pub fn side_prefix(side: &Side) -> &'static str {
    match side {
        Side::Buy => "botB",
        Side::Sell => "botS",
    }
}

/// Base TP client_id without open-price suffix: `botB_tp` / `botS_tp`.
pub fn bot_tp_client_id_base(side_prefix: &str) -> String {
    format!("{side_prefix}_tp")
}

/// CoinEx `client_id`: ≤32 bytes, `[A-Za-z0-9_-]` only (no dots).
/// Encodes open/last at place time, e.g. `botB_tp_0p015373`.
pub fn make_bot_tp_client_id(side_prefix: &str, open_price: f64) -> String {
    let base = bot_tp_client_id_base(side_prefix);
    let tag = trim_float(&format!("{open_price:.8}")).replace('.', "p");
    let mut cid = format!("{base}_{tag}");
    if cid.len() > 32 {
        cid.truncate(32);
        while cid.ends_with('_') {
            cid.pop();
        }
    }
    cid
}

/// Parse first-entry open encoded in `botB_tp_0p012855` (None if suffix missing).
pub fn parse_open_from_bot_tp_client_id(cid: &str) -> Option<f64> {
    let tag = cid
        .strip_prefix("botB_tp_")
        .or_else(|| cid.strip_prefix("botS_tp_"))?;
    if tag.is_empty() {
        return None;
    }
    tag.replace('p', ".").parse().ok().filter(|p| *p > 0.0)
}

/// True if `cid` is our bot TP: exact `botB_tp` or prefix `botB_tp…` (with open-price suffix).
/// `wanted` may be the base (`botB_tp`) or a full id (`botB_tp_0p015373`).
pub fn is_bot_tp_client_id(cid: &str, wanted: &str) -> bool {
    if cid.is_empty() {
        return false;
    }
    let base = bot_tp_cid_base(wanted);
    cid == base || cid.starts_with(&format!("{base}_"))
}

fn bot_tp_cid_base(wanted: &str) -> &str {
    // Prefer the known bases so a full id still matches siblings.
    if wanted.starts_with("botB_tp") {
        "botB_tp"
    } else if wanted.starts_with("botS_tp") {
        "botS_tp"
    } else if let Some(i) = wanted.find("_tp") {
        &wanted[..=i + 2]
    } else {
        wanted
    }
}

#[cfg(test)]
mod client_id_tests {
    use super::*;

    #[test]
    fn tp_client_id_encodes_open_without_dot() {
        let cid = make_bot_tp_client_id("botB", 0.015373);
        assert_eq!(cid, "botB_tp_0p015373");
        assert!(cid.len() <= 32);
        assert!(cid.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
        assert!((parse_open_from_bot_tp_client_id(&cid).unwrap() - 0.015373).abs() < 1e-12);
    }

    #[test]
    fn tp_client_id_prefix_match() {
        assert!(is_bot_tp_client_id("botB_tp", "botB_tp"));
        assert!(is_bot_tp_client_id("botB_tp_0p015373", "botB_tp"));
        assert!(is_bot_tp_client_id("botB_tp", "botB_tp_0p015373"));
        assert!(is_bot_tp_client_id("botB_tp_0p01", "botB_tp_0p015373"));
        assert!(!is_bot_tp_client_id("botS_tp", "botB_tp"));
        assert!(!is_bot_tp_client_id("", "botB_tp"));
        assert!(!is_bot_tp_client_id("other", "botB_tp"));
    }
}
