//! State-mediated exchange (citizen ↔ Quazar treasury via Security).
//! Встроенный **Агент Биржи**: потребность гражданина → проверка резерва Successful
//! → лот республики → сделка со списанием КВАЗИ в резерв; лот остаётся active.
//! Every mutation emits a chain event immediately in-core.

use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use sqlx::{PgPool, Postgres, Transaction};
use std::sync::Arc;
use uuid::Uuid;

use crate::auth::AuthContext;
use crate::chain_events;
use crate::offices::is_active_mayor;
use crate::response::ApiResponse;
use crate::shop;
use crate::svod;
use crate::types::Role;
use crate::validator::FOUNDER_DISPLAY_NAME;
use crate::AppState;

pub const STATE_RESERVE_ID: &str = "QZ-1";

/// Legacy name — settlement goes to Successful reserve (QZ-1).
pub const TREASURY_ID: &str = STATE_RESERVE_ID;

#[derive(Debug)]
pub enum ExchangeError {
    DatabaseError(String),
    NotFound(String),
    InsufficientBalance,
    InsufficientInventory,
    Unauthorized(String),
    BadRequest(String),
}

impl ExchangeError {
    fn status_code(&self) -> StatusCode {
        match self {
            ExchangeError::NotFound(_) => StatusCode::NOT_FOUND,
            ExchangeError::InsufficientBalance | ExchangeError::InsufficientInventory => {
                StatusCode::BAD_REQUEST
            }
            ExchangeError::Unauthorized(_) => StatusCode::FORBIDDEN,
            ExchangeError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ExchangeError::DatabaseError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl std::fmt::Display for ExchangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExchangeError::DatabaseError(m) => write!(f, "Database error: {m}"),
            ExchangeError::NotFound(m) => write!(f, "Not found: {m}"),
            ExchangeError::InsufficientBalance => write!(f, "Insufficient balance"),
            ExchangeError::InsufficientInventory => write!(f, "Insufficient inventory"),
            ExchangeError::Unauthorized(m) => write!(f, "Unauthorized: {m}"),
            ExchangeError::BadRequest(m) => write!(f, "Bad request: {m}"),
        }
    }
}

impl IntoResponse for ExchangeError {
    fn into_response(self) -> axum::response::Response {
        (self.status_code(), Json(ApiResponse::error(self.to_string()))).into_response()
    }
}

pub(crate) type ExchangeResult<T> = Result<T, ExchangeError>;

pub(crate) fn buyer_unit_price(svod: i64) -> i64 {
    (svod * 11 + 9) / 10
}

pub(crate) fn seller_unit_price(svod: i64) -> i64 {
    (svod * 9) / 10
}

fn now() -> i64 {
    Utc::now().timestamp()
}

/// Default listing TTL (30 days) unless `EXCHANGE_DEFAULT_SHELF_LIFE_SECS` is set.
pub const DEFAULT_SHELF_LIFE_SECS: i64 = 2_592_000;

pub fn default_shelf_life_secs() -> i64 {
    std::env::var("EXCHANGE_DEFAULT_SHELF_LIFE_SECS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_SHELF_LIFE_SECS)
}

fn resolve_expires_at(shelf_life_secs: Option<i64>, from_ts: i64) -> i64 {
    let secs = shelf_life_secs
        .filter(|&s| s > 0)
        .unwrap_or_else(default_shelf_life_secs);
    from_ts.saturating_add(secs)
}

fn remaining_shelf_life_secs(expires_at: Option<i64>, now_ts: i64) -> Option<i64> {
    expires_at.map(|e| (e - now_ts).max(0))
}

/// Available local stock at a node (qty − reserved).
async fn node_lot_available(
    pool: &PgPool,
    node_id: &str,
    svod_code: &str,
) -> ExchangeResult<i64> {
    let row: Option<(i64, i64)> = sqlx::query_as(
        "SELECT qty, reserved FROM node_lot_balances WHERE node_id = $1 AND svod_code = $2",
    )
    .bind(node_id)
    .bind(svod_code)
    .fetch_optional(pool)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    Ok(row.map(|(q, r)| (q - r).max(0)).unwrap_or(0))
}

async fn ensure_node_lot_row(
    tx: &mut Transaction<'_, Postgres>,
    node_id: &str,
    svod_code: &str,
) -> ExchangeResult<()> {
    sqlx::query(
        "INSERT INTO node_lot_balances (node_id, svod_code, qty, reserved, updated_at)
         VALUES ($1, $2, 0, 0, $3)
         ON CONFLICT (node_id, svod_code) DO NOTHING",
    )
    .bind(node_id)
    .bind(svod_code)
    .bind(now())
    .execute(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    Ok(())
}

/// Seed demo export stock so «Экспорт» is visible when local city has 0.
async fn seed_export_demo_if_needed(pool: &PgPool, local_node: &str) -> ExchangeResult<()> {
    let peer = "node-export-demo";
    if peer == local_node {
        return Ok(());
    }
    let has_any: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM node_lot_balances WHERE qty > reserved)",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    let ts = now();
    if !has_any {
        for (code, qty, price) in [
            ("video_create", 3_i64, 50_i64),
            ("review", 2, 20),
            ("code_commit", 1, 100),
        ] {
            sqlx::query(
                "INSERT INTO node_lot_balances (node_id, svod_code, qty, reserved, updated_at)
                 VALUES ($1, $2, $3, 0, $4)
                 ON CONFLICT (node_id, svod_code) DO UPDATE
                 SET qty = GREATEST(node_lot_balances.qty, EXCLUDED.qty), updated_at = EXCLUDED.updated_at",
            )
            .bind(peer)
            .bind(code)
            .bind(qty)
            .bind(ts)
            .execute(pool)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
            sqlx::query(
                "INSERT INTO node_lot_balances (node_id, svod_code, qty, reserved, updated_at)
                 VALUES ($1, $2, 0, 0, $3)
                 ON CONFLICT (node_id, svod_code) DO NOTHING",
            )
            .bind(local_node)
            .bind(code)
            .bind(ts)
            .execute(pool)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
            let _ = upsert_local_city_listing(pool, peer, code, qty, price, None).await?;
        }
        return Ok(());
    }

    // Ensure active listings exist for any peer stock without a live listing.
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT b.node_id, b.svod_code, (b.qty - b.reserved)
         FROM node_lot_balances b
         JOIN service_catalog s ON s.code = b.svod_code AND s.lot_scope = 'local' AND s.is_active
         WHERE (b.qty - b.reserved) > 0",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    for (node_id, code, avail) in rows {
        let has_listing = active_local_listing_meta(pool, &node_id, &code, ts)
            .await?
            .is_some();
        if !has_listing {
            let price: i64 = sqlx::query_scalar(
                "SELECT base_price FROM service_catalog WHERE code = $1",
            )
            .bind(&code)
            .fetch_optional(pool)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?
            .unwrap_or(1);
            let _ = upsert_local_city_listing(pool, &node_id, &code, avail, price, None).await?;
        }
    }
    Ok(())
}

const EXPORT_STATUSES: &[&str] = &[
    "opened",           // 1
    "buyer_sb",         // 2
    "owner_sb",         // 3
    "reserved",         // 4
    "owner_guardian",   // 5
    "transferred",      // 6
    "listed_buyer",     // 7
    "buyer_receipt",    // 8
    "completed",        // 9
];

fn export_status_for_step(step: i32) -> Option<&'static str> {
    EXPORT_STATUSES.get((step - 1) as usize).copied()
}

pub(crate) async fn ensure_active_citizen(pool: &PgPool, citizen_id: &str) -> ExchangeResult<()> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM citizens WHERE id = $1")
        .bind(citizen_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    match status.as_deref() {
        Some("active") => Ok(()),
        Some("pending") => Err(ExchangeError::BadRequest(
            "Биржа недоступна: паспорт ещё не выдан".into(),
        )),
        Some(other) => Err(ExchangeError::BadRequest(format!(
            "Биржа недоступна: статус {other}"
        ))),
        None => Err(ExchangeError::NotFound("Citizen not found".into())),
    }
}

pub(crate) async fn get_balance(pool: &PgPool, citizen_id: &str) -> ExchangeResult<i64> {
    let amount: Option<i64> =
        sqlx::query_scalar("SELECT amount FROM balances WHERE citizen_id = $1")
            .bind(citizen_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    if let Some(v) = amount {
        return Ok(v);
    }
    sqlx::query("INSERT INTO balances (citizen_id, amount) VALUES ($1, 0)")
        .bind(citizen_id)
        .execute(pool)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    Ok(0)
}

pub(crate) async fn lock_balance(
    tx: &mut Transaction<'_, Postgres>,
    citizen_id: &str,
) -> ExchangeResult<i64> {
    sqlx::query(
        "INSERT INTO balances (citizen_id, amount) VALUES ($1, 0)
         ON CONFLICT (citizen_id) DO NOTHING",
    )
    .bind(citizen_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    sqlx::query_scalar("SELECT amount FROM balances WHERE citizen_id = $1 FOR UPDATE")
        .bind(citizen_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))
}

pub(crate) async fn set_balance(
    tx: &mut Transaction<'_, Postgres>,
    citizen_id: &str,
    amount: i64,
) -> ExchangeResult<()> {
    sqlx::query("UPDATE balances SET amount = $1 WHERE citizen_id = $2")
        .bind(amount)
        .bind(citizen_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    Ok(())
}

async fn get_inventory(pool: &PgPool, citizen_id: &str, svod_code: &str) -> ExchangeResult<i64> {
    let q: Option<i64> = sqlx::query_scalar(
        "SELECT quantity FROM exchange_inventory WHERE citizen_id = $1 AND svod_code = $2",
    )
    .bind(citizen_id)
    .bind(svod_code)
    .fetch_optional(pool)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    Ok(q.unwrap_or(0))
}

async fn lock_inventory(
    tx: &mut Transaction<'_, Postgres>,
    citizen_id: &str,
    svod_code: &str,
) -> ExchangeResult<i64> {
    sqlx::query(
        "INSERT INTO exchange_inventory (citizen_id, svod_code, quantity, updated_at)
         VALUES ($1, $2, 0, $3)
         ON CONFLICT (citizen_id, svod_code) DO NOTHING",
    )
    .bind(citizen_id)
    .bind(svod_code)
    .bind(now())
    .execute(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    sqlx::query_scalar(
        "SELECT quantity FROM exchange_inventory
         WHERE citizen_id = $1 AND svod_code = $2 FOR UPDATE",
    )
    .bind(citizen_id)
    .bind(svod_code)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))
}

async fn set_inventory(
    tx: &mut Transaction<'_, Postgres>,
    citizen_id: &str,
    svod_code: &str,
    quantity: i64,
) -> ExchangeResult<()> {
    sqlx::query(
        "UPDATE exchange_inventory SET quantity = $1, updated_at = $2
         WHERE citizen_id = $3 AND svod_code = $4",
    )
    .bind(quantity)
    .bind(now())
    .bind(citizen_id)
    .bind(svod_code)
    .execute(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    Ok(())
}

pub(crate) async fn emit_event(
    state: &Arc<AppState>,
    event_type: &str,
    initiator: &str,
    title: &str,
    description: impl AsRef<str>,
    event_id: &str,
    data: serde_json::Value,
) -> ExchangeResult<()> {
    chain_events::emit_signed_event(
        state,
        event_type,
        initiator,
        title,
        description.as_ref(),
        Some(event_id),
        data,
    )
    .await
    .map_err(ExchangeError::BadRequest)
}

#[derive(Debug, Deserialize)]
pub struct CreateRequestBody {
    pub side: String,
    pub svod_code: String,
    pub quantity: u64,
}

#[derive(Debug, Deserialize)]
pub struct CreateNeedBody {
    /// `action` (default) | `item` — два свода на бирже.
    pub kind: Option<String>,
    /// Код лота Свода действий (`SVOD.md` / service_catalog).
    pub svod_code: Option<String>,
    /// Код предмета Свода предметов (`ITEMS.md` / shop_items).
    pub item_code: Option<String>,
    pub quantity: u64,
    /// Обязателен, если потребность подаёт Agent (Секретарь) от имени гражданина.
    pub citizen_id: Option<String>,
    /// Кто подал: `api` | `secretary` (информативно).
    pub via: Option<String>,
    /// TTL лота в секундах (Агент Биржи). Иначе дефолт ядра.
    pub shelf_life_secs: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct SbVerifyBody {
    pub approve: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BuyBody {
    pub quantity: u64,
}

#[derive(Debug, Deserialize)]
pub struct AddBalanceRequest {
    pub citizen_id: String,
    pub amount: u64,
}

#[derive(Debug, Deserialize)]
pub struct InventoryAdjustBody {
    pub citizen_id: String,
    pub svod_code: String,
    pub quantity: i64,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub status: Option<String>,
    /// Filter by Свод work_kind: `development` | `implementation`
    pub work_kind: Option<String>,
    /// Optional position code from Республиканский Свод (`parts.*.items`)
    pub svod_code: Option<String>,
    /// If true (default for authenticated Citizen), only lots with unit_buyer_price <= balance
    pub affordable: Option<bool>,
    /// `global` | `local` — filter by lot scope from SVOD Доступ
    pub lot_scope: Option<String>,
    /// City/node whose local board is being viewed (defaults to this node).
    pub node_id: Option<String>,
    /// `action` | `item` | `all` (default) — unified exchange board filter.
    pub kind: Option<String>,
    /// Optional Items svod code filter for board/needs.
    pub item_code: Option<String>,
}

fn parse_lot_kind(raw: Option<&str>) -> ExchangeResult<&'static str> {
    match raw.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("action") {
        "action" | "actions" => Ok("action"),
        "item" | "items" => Ok("item"),
        other => Err(ExchangeError::BadRequest(format!(
            "kind must be action|item, got {other}"
        ))),
    }
}

fn can_procure_items(auth: &AuthContext) -> bool {
    matches!(
        auth.role,
        Role::Aiya | Role::Founder | Role::Agent | Role::Mayor
    )
}

/// Credit warehouse + bump exchange_item_created after item-lot fulfillment.
async fn fulfill_item_to_warehouse(
    tx: &mut Transaction<'_, Postgres>,
    item_code: &str,
    qty: i64,
    ts: i64,
) -> ExchangeResult<(String, Uuid, i64, i64)> {
    let node = shop::this_node_id();
    let (item_id, _) = shop::item_id_by_code_tx(&mut *tx, item_code)
        .await
        .map_err(ExchangeError::BadRequest)?;
    let (prev, new_qty) = shop::credit_warehouse_delta(&mut *tx, &node, item_id, qty)
        .await
        .map_err(ExchangeError::DatabaseError)?;
    sqlx::query(
        "INSERT INTO exchange_item_created (item_code, created_qty, updated_at)
         VALUES ($1, $2, $3)
         ON CONFLICT (item_code) DO UPDATE SET
           created_qty = exchange_item_created.created_qty + EXCLUDED.created_qty,
           updated_at = EXCLUDED.updated_at",
    )
    .bind(item_code)
    .bind(qty)
    .bind(ts)
    .execute(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    Ok((node, item_id, prev, new_qty))
}

/// Встроенный Агент Биржи: проверка резерва Successful и выставление лота республики.
async fn exchange_agent_list_need(
    state: &Arc<AppState>,
    need_id: &str,
    citizen_id: &str,
    svod_code: &str,
    qty: i64,
    unit_svod: i64,
    initiator: &str,
    shelf_life_secs: Option<i64>,
) -> ExchangeResult<(String, i64)> {
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let reserve_need = unit_svod.saturating_mul(qty);
    let reserve_bal = lock_balance(&mut tx, STATE_RESERVE_ID).await?;
    if reserve_bal < reserve_need {
        let reason = format!(
            "Недостаточный резерв республики ({FOUNDER_DISPLAY_NAME}): нужно {reserve_need} КВАЗИ, доступно {reserve_bal}"
        );
        sqlx::query(
            "UPDATE exchange_needs SET status = 'rejected', reason = $1, reserve_balance = $2,
             unit_svod = $3, updated_at = $4 WHERE id = $5",
        )
        .bind(&reason)
        .bind(reserve_bal)
        .bind(unit_svod)
        .bind(now())
        .bind(need_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        tx.commit()
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        return Err(ExchangeError::BadRequest(reason));
    }

    let lot_scope: String = sqlx::query_scalar(
        "SELECT COALESCE(lot_scope, 'global') FROM service_catalog WHERE code = $1",
    )
    .bind(svod_code)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?
    .unwrap_or_else(|| "global".into());

    // Переиспользуем активный лот республики по той же позиции Свода.
    let existing: Option<(String, i64)> = sqlx::query_as(
        "SELECT id, qty_remaining FROM exchange_listings
         WHERE is_state_agent = TRUE AND status = 'active' AND svod_code = $1
           AND COALESCE(lot_kind, 'action') = 'action'
         ORDER BY created_at ASC LIMIT 1 FOR UPDATE",
    )
    .bind(svod_code)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let unit_buyer = buyer_unit_price(unit_svod);
    let unit_seller = seller_unit_price(unit_svod);
    let ts = now();
    let expires_at = resolve_expires_at(shelf_life_secs, ts);

    let listing_id = if let Some((lid, rem)) = existing {
        sqlx::query(
            "UPDATE exchange_listings SET
               qty_total = GREATEST(qty_total, $1),
               qty_remaining = GREATEST(qty_remaining, $1),
               unit_buyer_price = $2,
               unit_seller_price = $3,
               lot_scope = $4,
               updated_at = $5,
               expires_at = COALESCE(expires_at, $6),
               status = 'active'
             WHERE id = $7",
        )
        .bind(qty)
        .bind(unit_buyer)
        .bind(unit_seller)
        .bind(&lot_scope)
        .bind(ts)
        .bind(expires_at)
        .bind(&lid)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        let _ = rem;
        lid
    } else {
        let request_id = format!("exreq_{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO exchange_requests
             (id, citizen_id, side, svod_code, qty_total, qty_remaining, unit_svod, status, created_at, updated_at, lot_kind)
             VALUES ($1,$2,'sell',$3,$4,$4,$5,'accepted',$6,$6,'action')",
        )
        .bind(&request_id)
        .bind(STATE_RESERVE_ID)
        .bind(svod_code)
        .bind(qty)
        .bind(unit_svod)
        .bind(ts)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        let lid = format!("exlist_{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO exchange_listings
             (id, request_id, seller_id, svod_code, qty_total, qty_remaining,
              unit_svod, unit_buyer_price, unit_seller_price, status, created_at, updated_at,
              is_state_agent, lot_scope, expires_at, lot_kind)
             VALUES ($1,$2,$3,$4,$5,$5,$6,$7,$8,'active',$9,$9,TRUE,$10,$11,'action')",
        )
        .bind(&lid)
        .bind(&request_id)
        .bind(STATE_RESERVE_ID)
        .bind(svod_code)
        .bind(qty)
        .bind(unit_svod)
        .bind(unit_buyer)
        .bind(unit_seller)
        .bind(ts)
        .bind(&lot_scope)
        .bind(expires_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        sqlx::query("UPDATE exchange_requests SET listing_id = $1, updated_at = $2 WHERE id = $3")
            .bind(&lid)
            .bind(ts)
            .bind(&request_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        lid
    };

    sqlx::query(
        "UPDATE exchange_needs SET status = 'listed', listing_id = $1, unit_svod = $2,
         reserve_balance = $3, reason = NULL, updated_at = $4 WHERE id = $5",
    )
    .bind(&listing_id)
    .bind(unit_svod)
    .bind(reserve_bal)
    .bind(ts)
    .bind(need_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    tx.commit()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    emit_event(
        state,
        "ExchangeAgentListed",
        initiator,
        "Агент Биржи выставил лот",
        format!(
            "need {need_id} → listing {listing_id} {qty} x {svod_code} reserve_ok={reserve_bal}"
        ),
        &format!("evt_agent_{need_id}"),
        json!({
            "need_id": need_id,
            "listing_id": listing_id,
            "citizen_id": citizen_id,
            "svod_code": svod_code,
            "quantity": qty,
            "unit_svod": unit_svod,
            "reserve_id": STATE_RESERVE_ID,
            "reserve_balance": reserve_bal,
            "seller_id": STATE_RESERVE_ID,
            "seller_name": FOUNDER_DISPLAY_NAME,
        }),
    )
    .await?;

    Ok((listing_id, reserve_bal))
}

/// Агент Биржи: предметный лот (обеспечение склада) → listing с lot_kind=item.
async fn exchange_agent_list_item_need(
    state: &Arc<AppState>,
    need_id: &str,
    citizen_id: &str,
    item_code: &str,
    qty: i64,
    unit_svod: i64,
    initiator: &str,
    shelf_life_secs: Option<i64>,
) -> ExchangeResult<(String, i64)> {
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let reserve_need = unit_svod.saturating_mul(qty);
    let reserve_bal = lock_balance(&mut tx, STATE_RESERVE_ID).await?;
    if reserve_bal < reserve_need {
        let reason = format!(
            "Недостаточный резерв республики ({FOUNDER_DISPLAY_NAME}): нужно {reserve_need} КВАЗИ, доступно {reserve_bal}"
        );
        sqlx::query(
            "UPDATE exchange_needs SET status = 'rejected', reason = $1, reserve_balance = $2,
             unit_svod = $3, updated_at = $4 WHERE id = $5",
        )
        .bind(&reason)
        .bind(reserve_bal)
        .bind(unit_svod)
        .bind(now())
        .bind(need_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        tx.commit()
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        return Err(ExchangeError::BadRequest(reason));
    }

    let existing: Option<(String, i64)> = sqlx::query_as(
        "SELECT id, qty_remaining FROM exchange_listings
         WHERE is_state_agent = TRUE AND status = 'active' AND lot_kind = 'item'
           AND item_code = $1
         ORDER BY created_at ASC LIMIT 1 FOR UPDATE",
    )
    .bind(item_code)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let unit_buyer = buyer_unit_price(unit_svod);
    let unit_seller = seller_unit_price(unit_svod);
    let ts = now();
    let expires_at = resolve_expires_at(shelf_life_secs, ts);
    let lot_scope = "global";

    let listing_id = if let Some((lid, rem)) = existing {
        sqlx::query(
            "UPDATE exchange_listings SET
               qty_total = GREATEST(qty_total, $1),
               qty_remaining = GREATEST(qty_remaining, $1),
               unit_buyer_price = $2,
               unit_seller_price = $3,
               updated_at = $4,
               expires_at = COALESCE(expires_at, $5),
               status = 'active'
             WHERE id = $6",
        )
        .bind(qty)
        .bind(unit_buyer)
        .bind(unit_seller)
        .bind(ts)
        .bind(expires_at)
        .bind(&lid)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        let _ = rem;
        lid
    } else {
        let request_id = format!("exreq_{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO exchange_requests
             (id, citizen_id, side, svod_code, item_code, qty_total, qty_remaining, unit_svod,
              status, created_at, updated_at, lot_kind)
             VALUES ($1,$2,'sell',NULL,$3,$4,$4,$5,'accepted',$6,$6,'item')",
        )
        .bind(&request_id)
        .bind(STATE_RESERVE_ID)
        .bind(item_code)
        .bind(qty)
        .bind(unit_svod)
        .bind(ts)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        let lid = format!("exlist_{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO exchange_listings
             (id, request_id, seller_id, svod_code, item_code, qty_total, qty_remaining,
              unit_svod, unit_buyer_price, unit_seller_price, status, created_at, updated_at,
              is_state_agent, lot_scope, expires_at, lot_kind)
             VALUES ($1,$2,$3,NULL,$4,$5,$5,$6,$7,$8,'active',$9,$9,TRUE,$10,$11,'item')",
        )
        .bind(&lid)
        .bind(&request_id)
        .bind(STATE_RESERVE_ID)
        .bind(item_code)
        .bind(qty)
        .bind(unit_svod)
        .bind(unit_buyer)
        .bind(unit_seller)
        .bind(ts)
        .bind(lot_scope)
        .bind(expires_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        sqlx::query("UPDATE exchange_requests SET listing_id = $1, updated_at = $2 WHERE id = $3")
            .bind(&lid)
            .bind(ts)
            .bind(&request_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        lid
    };

    sqlx::query(
        "UPDATE exchange_needs SET status = 'listed', listing_id = $1, unit_svod = $2,
         reserve_balance = $3, reason = NULL, updated_at = $4 WHERE id = $5",
    )
    .bind(&listing_id)
    .bind(unit_svod)
    .bind(reserve_bal)
    .bind(ts)
    .bind(need_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    tx.commit()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    emit_event(
        state,
        "ExchangeAgentListed",
        initiator,
        "Агент Биржи выставил предметный лот",
        format!("need {need_id} → listing {listing_id} {qty} x item:{item_code}"),
        &format!("evt_agent_{need_id}"),
        json!({
            "need_id": need_id,
            "listing_id": listing_id,
            "citizen_id": citizen_id,
            "kind": "item",
            "item_code": item_code,
            "quantity": qty,
            "unit_svod": unit_svod,
            "reserve_id": STATE_RESERVE_ID,
            "reserve_balance": reserve_bal,
            "seller_id": STATE_RESERVE_ID,
            "seller_name": FOUNDER_DISPLAY_NAME,
        }),
    )
    .await?;

    Ok((listing_id, reserve_bal))
}

/// POST /exchange/needs — гражданин (или Секретарь от его имени) выражает потребность.
/// Агент Биржи сразу проверяет резерв Successful и выставляет лот республики.
/// `kind=item` + `item_code` — обеспечение склада (Свод предметов).
pub async fn create_need(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<CreateNeedBody>,
) -> ExchangeResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ExchangeError::Unauthorized(
            "Node credentials cannot create exchange needs".into(),
        ));
    }
    if body.quantity == 0 {
        return Err(ExchangeError::BadRequest("quantity must be > 0".into()));
    }

    let lot_kind = parse_lot_kind(body.kind.as_deref())?;

    let (citizen_id, created_via) = if auth.can_submit_need_for_citizen() {
        let cid = body.citizen_id.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let Some(cid) = cid else {
            return Err(ExchangeError::BadRequest(
                "Agent must pass citizen_id when submitting a need".into(),
            ));
        };
        ensure_active_citizen(&state.db, cid).await?;
        let via = body
            .via
            .as_deref()
            .unwrap_or("secretary")
            .trim()
            .to_string();
        (cid.to_string(), via)
    } else if auth.can_express_exchange_need() {
        let cid = auth.resolve_account_id(&state.db).await;
        ensure_active_citizen(&state.db, &cid).await?;
        let via = body.via.as_deref().unwrap_or("api").trim().to_string();
        (cid, via)
    } else {
        return Err(ExchangeError::Unauthorized(
            "Потребность на бирже доступна гражданам (или Секретарю от их имени)".into(),
        ));
    };

    let qty = body.quantity as i64;

    if lot_kind == "item" {
        let item_code = body
            .item_code
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ExchangeError::BadRequest("item_code required for kind=item".into()))?
            .to_string();
        let (_, unit_svod) = shop::item_id_by_code(&state.db, &item_code)
            .await
            .map_err(ExchangeError::BadRequest)?;
        if unit_svod <= 0 {
            return Err(ExchangeError::BadRequest(
                "item price_kvazi must be > 0 for exchange listing".into(),
            ));
        }

        let need_id = format!("exneed_{}", Uuid::new_v4());
        let ts = now();
        sqlx::query(
            "INSERT INTO exchange_needs
             (id, citizen_id, svod_code, item_code, quantity, status, unit_svod, created_via,
              created_at, updated_at, lot_kind)
             VALUES ($1,$2,NULL,$3,$4,'pending',$5,$6,$7,$7,'item')",
        )
        .bind(&need_id)
        .bind(&citizen_id)
        .bind(&item_code)
        .bind(qty)
        .bind(unit_svod)
        .bind(&created_via)
        .bind(ts)
        .execute(&state.db)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        emit_event(
            &state,
            "ExchangeNeedCreated",
            &auth.citizen_name,
            "Потребность в предмете на бирже",
            format!("{qty} x item:{item_code} for {citizen_id}"),
            &format!("evt_{need_id}"),
            json!({
                "need_id": need_id,
                "citizen_id": citizen_id,
                "kind": "item",
                "item_code": item_code,
                "quantity": qty,
                "unit_svod": unit_svod,
                "created_via": created_via,
            }),
        )
        .await?;

        let (listing_id, reserve_balance) = exchange_agent_list_item_need(
            &state,
            &need_id,
            &citizen_id,
            &item_code,
            qty,
            unit_svod,
            &auth.citizen_name,
            body.shelf_life_secs,
        )
        .await?;

        return Ok(Json(ApiResponse::success(json!({
            "need_id": need_id,
            "citizen_id": citizen_id,
            "kind": "item",
            "item_code": item_code,
            "quantity": qty,
            "unit_svod": unit_svod,
            "unit_buyer_price": buyer_unit_price(unit_svod),
            "status": "listed",
            "listing_id": listing_id,
            "seller_id": STATE_RESERVE_ID,
            "seller_name": FOUNDER_DISPLAY_NAME,
            "reserve_balance": reserve_balance,
            "shelf_life_secs": body.shelf_life_secs.unwrap_or_else(default_shelf_life_secs),
            "expires_at": resolve_expires_at(body.shelf_life_secs, now()),
            "agent": "exchange",
            "next": format!("POST /exchange/listings/{listing_id}/buy"),
            "note": "Исполнение предметного лота: мэр/Айя/Секретарь → +склад узла",
        }))));
    }

    let svod_code = body
        .svod_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ExchangeError::BadRequest("svod_code required for kind=action".into()))?
        .to_string();

    let service = svod::get_service_by_code(&state.db, &svod_code, true)
        .await
        .map_err(|e| ExchangeError::BadRequest(e.to_string()))?;
    let unit_svod = service.base_price;
    if unit_svod <= 0 {
        return Err(ExchangeError::BadRequest("invalid svod base_price".into()));
    }

    let need_id = format!("exneed_{}", Uuid::new_v4());
    let ts = now();
    sqlx::query(
        "INSERT INTO exchange_needs
         (id, citizen_id, svod_code, quantity, status, unit_svod, created_via, created_at, updated_at, lot_kind)
         VALUES ($1,$2,$3,$4,'pending',$5,$6,$7,$7,'action')",
    )
    .bind(&need_id)
    .bind(&citizen_id)
    .bind(&service.code)
    .bind(qty)
    .bind(unit_svod)
    .bind(&created_via)
    .bind(ts)
    .execute(&state.db)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    emit_event(
        &state,
        "ExchangeNeedCreated",
        &auth.citizen_name,
        "Потребность на бирже",
        format!("{qty} x {} for {citizen_id}", service.code),
        &format!("evt_{need_id}"),
        json!({
            "need_id": need_id,
            "citizen_id": citizen_id,
            "kind": "action",
            "svod_code": service.code,
            "quantity": qty,
            "unit_svod": unit_svod,
            "created_via": created_via,
        }),
    )
    .await?;

    // Секретарь / Agent: выставляет только global (канон v3).
    if auth.can_submit_need_for_citizen() && service.lot_scope != "global" {
        return Err(ExchangeError::BadRequest(
            "Секретарь через Агента биржи выставляет только глобальные лоты (в Доступе есть Г)".into(),
        ));
    }

    let (listing_id, reserve_balance) = exchange_agent_list_need(
        &state,
        &need_id,
        &citizen_id,
        &service.code,
        qty,
        unit_svod,
        &auth.citizen_name,
        body.shelf_life_secs,
    )
    .await?;

    Ok(Json(ApiResponse::success(json!({
        "need_id": need_id,
        "citizen_id": citizen_id,
        "kind": "action",
        "svod_code": service.code,
        "quantity": qty,
        "unit_svod": unit_svod,
        "unit_buyer_price": buyer_unit_price(unit_svod),
        "status": "listed",
        "listing_id": listing_id,
        "seller_id": STATE_RESERVE_ID,
        "seller_name": FOUNDER_DISPLAY_NAME,
        "reserve_balance": reserve_balance,
        "shelf_life_secs": body.shelf_life_secs.unwrap_or_else(default_shelf_life_secs),
        "expires_at": resolve_expires_at(body.shelf_life_secs, now()),
        "agent": "exchange",
        "next": format!("POST /exchange/listings/{listing_id}/buy"),
        "note": "Сделка завершается после подтверждения Агентом СБ",
    }))))
}

/// GET /exchange/board — единая витрина: действия (ordered/completed) + предметы (created/total/on_node).
pub async fn list_board(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<ListQuery>,
) -> ExchangeResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ExchangeError::Unauthorized(
            "Node credentials cannot browse exchange board".into(),
        ));
    }

    let kind_filter = q
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("all");
    if !matches!(kind_filter, "all" | "action" | "actions" | "item" | "items") {
        return Err(ExchangeError::BadRequest(
            "kind must be all|action|item".into(),
        ));
    }
    let include_actions = matches!(kind_filter, "all" | "action" | "actions");
    let include_items = matches!(kind_filter, "all" | "item" | "items");

    let view_node = q
        .node_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(state.node_id.as_str())
        .to_string();

    let mut rows = Vec::new();

    if include_actions {
        let svod_code = q
            .svod_code
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let action_rows: Vec<(String, String, i64, String, String, i64, i64)> = sqlx::query_as(
            "SELECT svod_code, title, unit_svod, work_kind, lot_scope, ordered_qty, completed_qty
             FROM exchange_action_board
             WHERE ($1::text IS NULL OR svod_code = $1)
             ORDER BY svod_code",
        )
        .bind(svod_code)
        .fetch_all(&state.db)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        for (code, title, unit, wk, scope, ordered, completed) in action_rows {
            rows.push(json!({
                "kind": "action",
                "code": code,
                "svod_code": code,
                "title": title,
                "unit_svod": unit,
                "work_kind": wk,
                "lot_scope": scope,
                "ordered": ordered,
                "completed": completed,
            }));
        }
    }

    if include_items {
        let item_code = q
            .item_code
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let item_rows: Vec<(String, String, String, String, i64, i64, i64)> = sqlx::query_as(
            "SELECT b.item_code, b.title, b.category, b.unit, b.unit_svod, b.created_qty, b.total_qty
             FROM exchange_item_board b
             WHERE ($1::text IS NULL OR b.item_code = $1)
             ORDER BY b.item_code",
        )
        .bind(item_code)
        .fetch_all(&state.db)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        for (code, title, category, unit, unit_svod, created, total) in item_rows {
            let on_node: i64 = sqlx::query_scalar(
                "SELECT COALESCE(w.quantity, 0)
                 FROM shop_items i
                 LEFT JOIN shop_warehouse w ON w.item_id = i.id AND w.node_id = $1
                 WHERE i.code = $2",
            )
            .bind(&view_node)
            .bind(&code)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?
            .unwrap_or(0);

            let active_listing: Option<(String, i64)> = sqlx::query_as(
                "SELECT id, qty_remaining FROM exchange_listings
                 WHERE lot_kind = 'item' AND item_code = $1 AND status = 'active'
                   AND qty_remaining > 0
                 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(&code)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

            rows.push(json!({
                "kind": "item",
                "code": code,
                "item_code": code,
                "title": title,
                "category": category,
                "unit": unit,
                "unit_svod": unit_svod,
                "created": created,
                "total": total,
                "on_node": on_node,
                "node_id": view_node,
                "listing_id": active_listing.as_ref().map(|l| l.0.clone()),
                "qty_remaining": active_listing.map(|l| l.1),
            }));
        }
    }

    Ok(Json(ApiResponse::success(json!({
        "node_id": view_node,
        "kind": kind_filter,
        "count": rows.len(),
        "rows": rows,
    }))))
}

/// GET /exchange/needs — свои потребности; Agent/SB/Founder — фильтр по citizen_id.
pub async fn list_needs(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<ListQuery>,
) -> ExchangeResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ExchangeError::Unauthorized("Node cannot list needs".into()));
    }

    let own_id = auth.resolve_account_id(&state.db).await;
    let work_kind = match q.work_kind.as_deref() {
        Some(raw) => Some(
            svod::normalize_work_kind(raw).map_err(|e| ExchangeError::BadRequest(e.to_string()))?,
        ),
        None => None,
    };
    let svod_code = q
        .svod_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    type NeedRow = (
        String,
        String,
        Option<String>,
        i64,
        String,
        Option<String>,
        Option<i64>,
        Option<String>,
        String,
        i64,
        String,
        String,
        Option<String>,
    );

    let rows: Vec<NeedRow> = if auth.can_submit_need_for_citizen()
        || auth.can_exchange_security()
        || auth.can_add_balance()
    {
        sqlx::query_as(
            "SELECT n.id, n.citizen_id, n.svod_code, n.quantity, n.status, n.listing_id,
                    n.unit_svod, n.reason, n.created_via, n.created_at,
                    COALESCE(s.work_kind, 'implementation') AS work_kind,
                    COALESCE(n.lot_kind, 'action') AS lot_kind,
                    n.item_code
             FROM exchange_needs n
             LEFT JOIN service_catalog s ON s.code = n.svod_code
             WHERE ($1::text IS NULL OR COALESCE(s.work_kind, 'implementation') = $1)
               AND ($2::text IS NULL OR n.svod_code = $2)
             ORDER BY n.created_at DESC LIMIT 200",
        )
        .bind(work_kind.as_deref())
        .bind(svod_code.as_deref())
        .fetch_all(&state.db)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?
    } else {
        sqlx::query_as(
            "SELECT n.id, n.citizen_id, n.svod_code, n.quantity, n.status, n.listing_id,
                    n.unit_svod, n.reason, n.created_via, n.created_at,
                    COALESCE(s.work_kind, 'implementation') AS work_kind,
                    COALESCE(n.lot_kind, 'action') AS lot_kind,
                    n.item_code
             FROM exchange_needs n
             LEFT JOIN service_catalog s ON s.code = n.svod_code
             WHERE n.citizen_id = $1
               AND ($2::text IS NULL OR COALESCE(s.work_kind, 'implementation') = $2)
               AND ($3::text IS NULL OR n.svod_code = $3)
             ORDER BY n.created_at DESC LIMIT 200",
        )
        .bind(&own_id)
        .bind(work_kind.as_deref())
        .bind(svod_code.as_deref())
        .fetch_all(&state.db)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?
    };

    let out: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.0,
                "citizen_id": r.1,
                "svod_code": r.2,
                "quantity": r.3,
                "status": r.4,
                "listing_id": r.5,
                "unit_svod": r.6,
                "reason": r.7,
                "created_via": r.8,
                "created_at": r.9,
                "work_kind": r.10,
                "work_kind_label": svod::work_kind_label(&r.10),
                "kind": r.11,
                "item_code": r.12,
                "mode": "needs",
                "agent": "exchange",
            })
        })
        .collect();
    Ok(Json(ApiResponse::success(out)))
}

/// POST /exchange/requests — citizen creates private sell/buy request.
pub async fn create_request(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<CreateRequestBody>,
) -> ExchangeResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ExchangeError::Unauthorized(
            "Node credentials cannot create exchange requests".into(),
        ));
    }
    if !auth.can_use_exchange() {
        return Err(ExchangeError::Unauthorized(
            "Биржа доступна только статусу Страж и выше".into(),
        ));
    }
    let citizen_id = auth.resolve_account_id(&state.db).await;
    ensure_active_citizen(&state.db, &citizen_id).await?;

    let side = body.side.trim().to_lowercase();
    if side != "sell" && side != "buy" {
        return Err(ExchangeError::BadRequest("side must be sell or buy".into()));
    }
    if body.quantity == 0 {
        return Err(ExchangeError::BadRequest("quantity must be > 0".into()));
    }

    let service = svod::get_service_by_code(&state.db, &body.svod_code, true)
        .await
        .map_err(|e| ExchangeError::BadRequest(e.to_string()))?;
    let unit_svod = service.base_price;
    if unit_svod <= 0 {
        return Err(ExchangeError::BadRequest("invalid svod base_price".into()));
    }
    let qty = body.quantity as i64;

    if side == "sell" {
        let inv = get_inventory(&state.db, &citizen_id, &service.code).await?;
        if inv < qty {
            return Err(ExchangeError::InsufficientInventory);
        }
    } else {
        let need = buyer_unit_price(unit_svod).saturating_mul(qty);
        let bal = get_balance(&state.db, &citizen_id).await?;
        if bal < need {
            return Err(ExchangeError::InsufficientBalance);
        }
    }

    let id = format!("exreq_{}", Uuid::new_v4());
    let ts = now();
    let status = if side == "sell" {
        "pending_sb"
    } else {
        "pending_sb"
    };

    sqlx::query(
        "INSERT INTO exchange_requests
         (id, citizen_id, side, svod_code, qty_total, qty_remaining, unit_svod, status, created_at, updated_at)
         VALUES ($1,$2,$3,$4,$5,$5,$6,$7,$8,$8)",
    )
    .bind(&id)
    .bind(&citizen_id)
    .bind(&side)
    .bind(&service.code)
    .bind(qty)
    .bind(unit_svod)
    .bind(status)
    .bind(ts)
    .execute(&state.db)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    emit_event(
        &state,
        "ExchangeRequestCreated",
        &auth.citizen_name,
        &format!("Exchange request {side}"),
        &format!("{side} {qty} x {} @ svod {unit_svod}", service.code),
        &format!("evt_{id}"),
        json!({
            "request_id": id,
            "side": side,
            "svod_code": service.code,
            "quantity": qty,
            "unit_svod": unit_svod,
            "citizen_id": citizen_id,
        }),
    )
    .await?;

    let mode = if side == "sell" { "offers" } else { "needs" };
    Ok(Json(ApiResponse::success(json!({
        "id": id,
        "side": side,
        "mode": mode,
        "svod_code": service.code,
        "quantity": qty,
        "unit_svod": unit_svod,
        "work_kind": service.work_kind,
        "work_kind_label": svod::work_kind_label(&service.work_kind),
        "status": status,
    }))))
}

fn request_mode(side: &str) -> &'static str {
    if side == "sell" {
        "offers"
    } else {
        "needs"
    }
}

fn request_json(
    id: String,
    citizen_id: String,
    side: String,
    svod_code: String,
    qty_total: i64,
    qty_remaining: i64,
    unit_svod: i64,
    status: String,
    created_at: i64,
    work_kind: String,
) -> serde_json::Value {
    json!({
        "id": id,
        "citizen_id": citizen_id,
        "side": side,
        "mode": request_mode(&side),
        "svod_code": svod_code,
        "qty_total": qty_total,
        "qty_remaining": qty_remaining,
        "unit_svod": unit_svod,
        "status": status,
        "created_at": created_at,
        "work_kind": work_kind,
        "work_kind_label": svod::work_kind_label(&work_kind),
    })
}

/// GET /exchange/requests — citizen: only own requests; SB: pending_sb (or ?status=).
/// Optional `?work_kind=` and `?svod_code=` filter via Свод positions.
pub async fn list_requests(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<ListQuery>,
) -> ExchangeResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ExchangeError::Unauthorized("Node cannot list requests".into()));
    }
    let citizen_id = auth.resolve_account_id(&state.db).await;
    let work_kind = match q.work_kind.as_deref() {
        Some(raw) => Some(
            svod::normalize_work_kind(raw).map_err(|e| ExchangeError::BadRequest(e.to_string()))?,
        ),
        None => None,
    };
    let svod_code = q
        .svod_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    type ReqRow = (
        String,
        String,
        String,
        String,
        i64,
        i64,
        i64,
        String,
        i64,
        String,
    );

    let rows: Vec<serde_json::Value> = if auth.can_exchange_security() {
        let status = q.status.unwrap_or_else(|| "pending_sb".into());
        let fetched: Vec<ReqRow> = sqlx::query_as(
            "SELECT r.id, r.citizen_id, r.side, r.svod_code, r.qty_total, r.qty_remaining,
                    r.unit_svod, r.status, r.created_at,
                    COALESCE(s.work_kind, 'implementation') AS work_kind
             FROM exchange_requests r
             LEFT JOIN service_catalog s ON s.code = r.svod_code
             WHERE r.status = $1
               AND ($2::text IS NULL OR COALESCE(s.work_kind, 'implementation') = $2)
               AND ($3::text IS NULL OR r.svod_code = $3)
             ORDER BY r.created_at DESC LIMIT 200",
        )
        .bind(&status)
        .bind(work_kind.as_deref())
        .bind(svod_code.as_deref())
        .fetch_all(&state.db)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        fetched
            .into_iter()
            .map(|r| request_json(r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7, r.8, r.9))
            .collect()
    } else {
        // Citizen: always only own requests (auth-scoped).
        let fetched: Vec<ReqRow> = sqlx::query_as(
            "SELECT r.id, r.citizen_id, r.side, r.svod_code, r.qty_total, r.qty_remaining,
                    r.unit_svod, r.status, r.created_at,
                    COALESCE(s.work_kind, 'implementation') AS work_kind
             FROM exchange_requests r
             LEFT JOIN service_catalog s ON s.code = r.svod_code
             WHERE r.citizen_id = $1
               AND ($2::text IS NULL OR COALESCE(s.work_kind, 'implementation') = $2)
               AND ($3::text IS NULL OR r.svod_code = $3)
             ORDER BY r.created_at DESC LIMIT 200",
        )
        .bind(&citizen_id)
        .bind(work_kind.as_deref())
        .bind(svod_code.as_deref())
        .fetch_all(&state.db)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        fetched
            .into_iter()
            .map(|r| request_json(r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7, r.8, r.9))
            .collect()
    };
    Ok(Json(ApiResponse::success(rows)))
}

/// POST /exchange/requests/:id/sb-verify — SB confirms loyalty / rejects.
pub async fn sb_verify_request(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(request_id): Path<String>,
    Json(body): Json<SbVerifyBody>,
) -> ExchangeResult<Json<ApiResponse>> {
    if !auth.can_exchange_security() {
        return Err(ExchangeError::Unauthorized(
            "Only Security (Guardian) can verify exchange requests".into(),
        ));
    }

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let row: Option<(String, String, String, String, i64, i64, String)> = sqlx::query_as(
        "SELECT id, citizen_id, side, svod_code, qty_total, unit_svod, status
         FROM exchange_requests WHERE id = $1 FOR UPDATE",
    )
    .bind(&request_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let Some((id, citizen_id, side, svod_code, qty_total, unit_svod, status)) = row else {
        return Err(ExchangeError::NotFound("request not found".into()));
    };
    if status != "pending_sb" {
        return Err(ExchangeError::BadRequest(format!(
            "request status is {status}, expected pending_sb"
        )));
    }

    let ts = now();
    if !body.approve {
        let reason = body
            .reason
            .clone()
            .unwrap_or_else(|| "rejected".to_string());
        sqlx::query(
            "UPDATE exchange_requests SET status = 'rejected', updated_at = $1 WHERE id = $2",
        )
        .bind(ts)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        tx.commit()
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        emit_event(
            &state,
            "ExchangeRequestRejected",
            &auth.citizen_name,
            "Exchange request rejected by SB",
            &reason,
            &format!("evt_rej_{id}"),
            json!({
                "request_id": id,
                "svod_code": svod_code,
                "side": side,
                "reason": reason,
            }),
        )
        .await?;
        return Ok(Json(ApiResponse::success(json!({"id": id, "status": "rejected"}))));
    }

    // Approve sell: check inventory and lock qty; open public listing at +10%.
    if side != "sell" {
        // Buy-side verify: mark accepted; listing is supply-driven in v1.
        sqlx::query(
            "UPDATE exchange_requests SET status = 'accepted', updated_at = $1 WHERE id = $2",
        )
        .bind(ts)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        tx.commit()
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        emit_event(
            &state,
            "ExchangeRequestVerified",
            &auth.citizen_name,
            "Exchange buy request verified",
            format!("verified buy request {id}"),
            &format!("evt_ver_{id}"),
            json!({
                "request_id": id,
                "svod_code": svod_code,
                "side": side,
                "quantity": qty_total,
            }),
        )
        .await?;
        return Ok(Json(ApiResponse::success(json!({"id": id, "status": "accepted"}))));
    }

    let inv = lock_inventory(&mut tx, &citizen_id, &svod_code).await?;
    if inv < qty_total {
        return Err(ExchangeError::InsufficientInventory);
    }
    set_inventory(&mut tx, &citizen_id, &svod_code, inv - qty_total).await?;

    let listing_id = format!("exlist_{}", Uuid::new_v4());
    let unit_buyer = buyer_unit_price(unit_svod);
    let unit_seller = seller_unit_price(unit_svod);
    let lot_scope: String = sqlx::query_scalar(
        "SELECT COALESCE(lot_scope, 'global') FROM service_catalog WHERE code = $1",
    )
    .bind(&svod_code)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?
    .unwrap_or_else(|| "global".into());
    let expires_at = resolve_expires_at(None, ts);

    sqlx::query(
        "UPDATE exchange_requests SET status = 'accepted', listing_id = $1, updated_at = $2 WHERE id = $3",
    )
    .bind(&listing_id)
    .bind(ts)
    .bind(&id)
    .execute(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    sqlx::query(
        "INSERT INTO exchange_listings
         (id, request_id, seller_id, svod_code, qty_total, qty_remaining, unit_svod,
          unit_buyer_price, unit_seller_price, status, created_at, updated_at, lot_scope, expires_at)
         VALUES ($1,$2,$3,$4,$5,$5,$6,$7,$8,'active',$9,$9,$10,$11)",
    )
    .bind(&listing_id)
    .bind(&id)
    .bind(&citizen_id)
    .bind(&svod_code)
    .bind(qty_total)
    .bind(unit_svod)
    .bind(unit_buyer)
    .bind(unit_seller)
    .bind(ts)
    .bind(&lot_scope)
    .bind(expires_at)
    .execute(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    tx.commit()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    emit_event(
        &state,
        "ExchangeRequestVerified",
        &auth.citizen_name,
        "Exchange sell request verified by SB",
        format!("verified sell {qty_total} x {svod_code}"),
        &format!("evt_ver_{id}"),
        json!({
            "request_id": id,
            "svod_code": svod_code,
            "side": side,
            "quantity": qty_total,
            "listing_id": listing_id,
        }),
    )
    .await?;

    emit_event(
        &state,
        "ExchangeListingOpened",
        &auth.citizen_name,
        "Public listing opened",
        format!("listing {listing_id}: {qty_total} @ {unit_buyer}"),
        &format!("evt_list_{listing_id}"),
        json!({
            "listing_id": listing_id,
            "request_id": id,
            "svod_code": svod_code,
            "qty_total": qty_total,
            "unit_buyer_price": unit_buyer,
            "unit_seller_price": unit_seller,
            "unit_svod": unit_svod,
            "seller_id": citizen_id,
        }),
    )
    .await?;

    Ok(Json(ApiResponse::success(json!({
        "request_id": id,
        "listing_id": listing_id,
        "status": "accepted",
        "unit_buyer_price": unit_buyer,
        "unit_seller_price": unit_seller,
    }))))
}

/// GET /exchange/listings — public active listings (mode=offers витрина «Предложения»).
/// Optional `?work_kind=` and `?svod_code=` filter via Свод positions (`parts.*.items`).
/// `?affordable=true` — only lots the caller can pay (requires auth).
/// `?node_id=` — локальная биржа города: остатки + «Экспорт» из других городов.
pub async fn list_listings(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<ListQuery>,
) -> ExchangeResult<Json<ApiResponse>> {
    let status = q.status.unwrap_or_else(|| "active".into());
    let work_kind = match q.work_kind.as_deref() {
        Some(raw) => Some(
            svod::normalize_work_kind(raw).map_err(|e| ExchangeError::BadRequest(e.to_string()))?,
        ),
        None => None,
    };
    let svod_code = q
        .svod_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let affordable = q.affordable.unwrap_or(false);
    let max_price: Option<i64> = if affordable && !auth.is_node {
        let cid = auth.resolve_account_id(&state.db).await;
        Some(get_balance(&state.db, &cid).await?)
    } else {
        None
    };
    let scope_filter = q
        .lot_scope
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase());
    if let Some(ref s) = scope_filter {
        if s != "global" && s != "local" {
            return Err(ExchangeError::BadRequest(
                "lot_scope must be global or local".into(),
            ));
        }
    }
    // Citizens without Guardian+: show global lots only.
    let can_local = auth.is_node || auth.can_use_exchange();
    let effective_scope = if scope_filter.is_none() && !can_local {
        Some("global".to_string())
    } else {
        scope_filter
    };

    let view_node = q
        .node_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(state.node_id.as_str())
        .to_string();

    // Global / state-agent listings (and classic listings).
    let include_global = effective_scope.as_deref() != Some("local");
    let mut listings = Vec::new();
    let now_ts = now();
    if include_global {
        let rows = sqlx::query_as::<
            _,
            (
                String,
                String,
                String,
                String,
                i64,
                i64,
                i64,
                i64,
                i64,
                String,
                i64,
                String,
                bool,
                String,
                Option<i64>,
            ),
        >(
            "SELECT l.id, l.request_id, l.seller_id, l.svod_code, l.qty_total, l.qty_remaining,
                    l.unit_svod, l.unit_buyer_price, l.unit_seller_price, l.status, l.created_at,
                    COALESCE(s.work_kind, 'implementation') AS work_kind,
                    l.is_state_agent,
                    COALESCE(l.lot_scope, 'global') AS lot_scope,
                    l.expires_at
             FROM exchange_listings l
             LEFT JOIN service_catalog s ON s.code = l.svod_code
             WHERE l.status = $1
               AND COALESCE(l.lot_kind, 'action') = 'action'
               AND COALESCE(l.lot_scope, 'global') = 'global'
               AND l.qty_remaining > 0
               AND (l.expires_at IS NULL OR l.expires_at > $5)
               AND ($2::text IS NULL OR COALESCE(s.work_kind, 'implementation') = $2)
               AND ($3::text IS NULL OR l.svod_code = $3)
               AND ($4::bigint IS NULL OR l.unit_buyer_price <= $4)
             ORDER BY l.created_at DESC LIMIT 200",
        )
        .bind(&status)
        .bind(work_kind.as_deref())
        .bind(svod_code.as_deref())
        .bind(max_price)
        .bind(now_ts)
        .fetch_all(&state.db)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        for r in rows {
            let rem = remaining_shelf_life_secs(r.14, now_ts);
            listings.push(json!({
                "id": r.0,
                "request_id": r.1,
                "seller_id": r.2,
                "kind": "action",
                "svod_code": r.3,
                "qty_total": r.4,
                "qty_remaining": r.5,
                "unit_svod": r.6,
                "unit_buyer_price": r.7,
                "unit_seller_price": r.8,
                "status": r.9,
                "created_at": r.10,
                "work_kind": r.11,
                "work_kind_label": svod::work_kind_label(&r.11),
                "is_state_agent": r.12,
                "lot_scope": "global",
                "confirm_mode": "agent_sb",
                "display_status": null,
                "is_export": false,
                "expires_at": r.14,
                "shelf_life_secs": rem,
                "seller_name": if r.12 { Some(FOUNDER_DISPLAY_NAME) } else { None },
                "side": "sell",
                "side_semantics": if r.12 { "state_agent_listing" } else { "state_mediated_listing" },
                "mode": "offers",
                "role_hint": "offers",
                "agent": if r.12 { Some("exchange") } else { None },
            }));
        }
    }

    // Local board: city stock + Export from other cities (Guardian+ only).
    // Variant A: only active non-expired listings appear on the board.
    let include_local = can_local && effective_scope.as_deref() != Some("global");
    if include_local {
        let _ = seed_export_demo_if_needed(&state.db, &view_node).await;
        let local_rows = sqlx::query_as::<_, (String, String, i64, String, Vec<String>)>(
            "SELECT code, name, base_price, work_kind, access_roles
             FROM service_catalog
             WHERE is_active AND lot_scope = 'local'
               AND ($1::text IS NULL OR work_kind = $1)
               AND ($2::text IS NULL OR code = $2)
             ORDER BY code",
        )
        .bind(work_kind.as_deref())
        .bind(svod_code.as_deref())
        .fetch_all(&state.db)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        for (code, name, base_price, wk, access) in local_rows {
            if let Some(max) = max_price {
                if base_price > max {
                    continue;
                }
            }
            let access_short = access_roles_short(&access);
            let local_qty = node_lot_available(&state.db, &view_node, &code).await?;
            if local_qty > 0 {
                if let Some((lid, exp)) =
                    active_local_listing_meta(&state.db, &view_node, &code, now_ts).await?
                {
                    let rem = remaining_shelf_life_secs(exp, now_ts);
                    let days = rem.map(|s| (s + 86399) / 86400).unwrap_or(0);
                    listings.push(json!({
                        "id": lid,
                        "request_id": null,
                        "seller_id": STATE_RESERVE_ID,
                        "svod_code": code,
                        "name": name,
                        "qty_total": local_qty,
                        "qty_remaining": local_qty,
                        "unit_svod": base_price,
                        "unit_buyer_price": base_price,
                        "unit_seller_price": base_price,
                        "status": "active",
                        "created_at": now_ts,
                        "work_kind": wk,
                        "work_kind_label": svod::work_kind_label(&wk),
                        "is_state_agent": true,
                        "lot_scope": "local",
                        "confirm_mode": "live_guardian",
                        "display_status": "Локальный",
                        "is_export": false,
                        "node_id": view_node,
                        "owner_node_id": view_node,
                        "access": access.clone(),
                        "access_short": access_short.clone(),
                        "expires_at": exp,
                        "shelf_life_secs": rem,
                        "seller_name": FOUNDER_DISPLAY_NAME,
                        "side": "sell",
                        "side_semantics": "local_city_stock",
                        "mode": "offers",
                        "role_hint": "offers",
                        "price_note": "Цена Свода (без наценок)",
                        "ui_line": format!(
                            "Лот: {code} | Цена: {base_price} КВАЗИ | Доступ: {access_short} | Статус: Локальный | Осталось: {days} дн."
                        ),
                    }));
                }
                continue;
            }

            let owners: Vec<(String, i64)> = sqlx::query_as(
                "SELECT node_id, (qty - reserved) AS avail
                 FROM node_lot_balances
                 WHERE svod_code = $1 AND node_id <> $2 AND (qty - reserved) > 0
                 ORDER BY avail DESC",
            )
            .bind(&code)
            .bind(&view_node)
            .fetch_all(&state.db)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

            for (owner_node, avail) in owners {
                let Some((lid, exp)) =
                    active_local_listing_meta(&state.db, &owner_node, &code, now_ts).await?
                else {
                    continue;
                };
                let rem = remaining_shelf_life_secs(exp, now_ts);
                let days = rem.map(|s| (s + 86399) / 86400).unwrap_or(0);
                listings.push(json!({
                    "id": lid,
                    "request_id": null,
                    "seller_id": STATE_RESERVE_ID,
                    "svod_code": code.clone(),
                    "name": name.clone(),
                    "qty_total": avail,
                    "qty_remaining": avail,
                    "unit_svod": base_price,
                    "unit_buyer_price": base_price,
                    "unit_seller_price": base_price,
                    "status": "active",
                    "created_at": now_ts,
                    "work_kind": wk.clone(),
                    "work_kind_label": svod::work_kind_label(&wk),
                    "is_state_agent": true,
                    "lot_scope": "local",
                    "confirm_mode": "live_guardian",
                    "display_status": "Экспорт",
                    "is_export": true,
                    "node_id": view_node,
                    "owner_node_id": owner_node,
                    "owner_city_hint": null,
                    "access": access.clone(),
                    "access_short": access_short.clone(),
                    "expires_at": exp,
                    "shelf_life_secs": rem,
                    "seller_name": FOUNDER_DISPLAY_NAME,
                    "side": "sell",
                    "side_semantics": "export_from_other_city",
                    "mode": "offers",
                    "role_hint": "offers",
                    "price_note": "Цена Свода — без доставки и межгородской комиссии",
                    "ui_line": format!(
                        "Лот: {code} | Цена: {base_price} КВАЗИ | Доступ: {access_short} | Статус: Экспорт | Осталось: {days} дн."
                    ),
                }));
            }
        }
    }

    // Фильтр витрины по активной роли vs Доступ лота (канон v3).
    if !auth.is_node {
        let catalog = svod::get_svod_items(&state.db, true, None)
            .await
            .map_err(|e| ExchangeError::BadRequest(e.to_string()))?;
        let by_code: std::collections::HashMap<&str, &svod::ServiceItem> =
            catalog.iter().map(|s| (s.code.as_str(), s)).collect();
        listings.retain(|item| {
            let Some(code) = item.get("svod_code").and_then(|v| v.as_str()) else {
                return true;
            };
            match by_code.get(code) {
                Some(svc) => svod::role_can_access_lot(&auth.role, &svc.access_roles),
                None => true,
            }
        });
        for item in &mut listings {
            let Some(code) = item
                .get("svod_code")
                .and_then(|v| v.as_str())
                .map(str::to_string)
            else {
                continue;
            };
            if let Some(svc) = by_code.get(code.as_str()) {
                let level = svod::access_level_from_access(&svc.access_roles);
                let access = svc.access_roles.clone();
                let short = access_roles_short(&access);
                if let Some(o) = item.as_object_mut() {
                    o.insert("access".into(), json!(access.clone()));
                    o.insert("access_roles".into(), json!(access));
                    o.insert("access_level".into(), json!(level.as_str()));
                    o.insert("access_short".into(), json!(short));
                }
            }
        }
    }

    Ok(Json(ApiResponse::success(listings)))
}

async fn active_local_listing_meta(
    pool: &PgPool,
    node_id: &str,
    svod_code: &str,
    now_ts: i64,
) -> ExchangeResult<Option<(String, Option<i64>)>> {
    let row: Option<(String, Option<i64>)> = sqlx::query_as(
        "SELECT id, expires_at FROM exchange_listings
         WHERE lot_scope = 'local' AND status = 'active' AND node_id = $1 AND svod_code = $2
           AND (expires_at IS NULL OR expires_at > $3)
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(node_id)
    .bind(svod_code)
    .bind(now_ts)
    .fetch_optional(pool)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    Ok(row)
}

/// Upsert a city listing for local stock (Variant A: listed → has expires_at).
async fn upsert_local_city_listing(
    pool: &PgPool,
    node_id: &str,
    svod_code: &str,
    qty: i64,
    unit_svod: i64,
    shelf_life_secs: Option<i64>,
) -> ExchangeResult<String> {
    let ts = now();
    let expires_at = resolve_expires_at(shelf_life_secs, ts);
    if qty <= 0 {
        sqlx::query(
            "UPDATE exchange_listings SET status = 'expired', updated_at = $1
             WHERE lot_scope = 'local' AND node_id = $2 AND svod_code = $3 AND status = 'active'",
        )
        .bind(ts)
        .bind(node_id)
        .bind(svod_code)
        .execute(pool)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        return Ok(String::new());
    }

    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM exchange_listings
         WHERE lot_scope = 'local' AND node_id = $1 AND svod_code = $2 AND status = 'active'
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(node_id)
    .bind(svod_code)
    .fetch_optional(pool)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    if let Some(lid) = existing {
        sqlx::query(
            "UPDATE exchange_listings SET qty_total = $1, qty_remaining = $1,
             unit_svod = $2, unit_buyer_price = $2, unit_seller_price = $2,
             updated_at = $3, expires_at = COALESCE(expires_at, $4)
             WHERE id = $5",
        )
        .bind(qty)
        .bind(unit_svod)
        .bind(ts)
        .bind(expires_at)
        .bind(&lid)
        .execute(pool)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        return Ok(lid);
    }

    let request_id = format!("exreq_{}", Uuid::new_v4());
    let lid = format!("exlist_{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO exchange_requests
         (id, citizen_id, side, svod_code, qty_total, qty_remaining, unit_svod, status, listing_id, created_at, updated_at)
         VALUES ($1,$2,'sell',$3,$4,$4,$5,'accepted',$6,$7,$7)",
    )
    .bind(&request_id)
    .bind(STATE_RESERVE_ID)
    .bind(svod_code)
    .bind(qty)
    .bind(unit_svod)
    .bind(&lid)
    .bind(ts)
    .execute(pool)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    sqlx::query(
        "INSERT INTO exchange_listings
         (id, request_id, seller_id, svod_code, qty_total, qty_remaining,
          unit_svod, unit_buyer_price, unit_seller_price, status, created_at, updated_at,
          is_state_agent, lot_scope, node_id, expires_at)
         VALUES ($1,$2,$3,$4,$5,$5,$6,$6,$6,'active',$7,$7,TRUE,'local',$8,$9)",
    )
    .bind(&lid)
    .bind(&request_id)
    .bind(STATE_RESERVE_ID)
    .bind(svod_code)
    .bind(qty)
    .bind(unit_svod)
    .bind(ts)
    .bind(node_id)
    .bind(expires_at)
    .execute(pool)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    Ok(lid)
}

fn access_roles_short(access: &[String]) -> String {
    let mut parts = Vec::new();
    for a in access {
        let s = a.trim();
        if s.eq_ignore_ascii_case("Гражданин") || s == "Г" {
            parts.push("Г");
        } else if s.eq_ignore_ascii_case("Страж") || s == "Ст" {
            parts.push("Ст");
        } else if s.eq_ignore_ascii_case("Судья") || s == "Сд" {
            parts.push("Сд");
        } else if s.eq_ignore_ascii_case("Мэр") || s == "М" {
            parts.push("М");
        }
    }
    if parts.is_empty() {
        "Ст+".into()
    } else {
        parts.join(", ")
    }
}

#[derive(Debug, Deserialize)]
pub struct ExtendListingBody {
    pub shelf_life_secs: i64,
}

/// POST /exchange/listings/:id/extend — продление TTL до истечения (Агент Биржи / Страж+).
pub async fn extend_listing(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(listing_id): Path<String>,
    Json(body): Json<ExtendListingBody>,
) -> ExchangeResult<Json<ApiResponse>> {
    if !(auth.can_submit_need_for_citizen()
        || auth.can_exchange_security()
        || auth.can_add_balance())
    {
        return Err(ExchangeError::Unauthorized(
            "Продление лота — Агент Биржи / Страж / Айя".into(),
        ));
    }
    if body.shelf_life_secs <= 0 {
        return Err(ExchangeError::BadRequest(
            "shelf_life_secs must be > 0".into(),
        ));
    }

    let row: Option<(String, String, Option<i64>, String)> = sqlx::query_as(
        "SELECT id, status, expires_at, svod_code FROM exchange_listings WHERE id = $1",
    )
    .bind(&listing_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    let Some((lid, status, expires_at, svod_code)) = row else {
        return Err(ExchangeError::NotFound("listing not found".into()));
    };
    let ts = now();
    if status != "active" || expires_at.map(|e| e <= ts).unwrap_or(false) {
        return Err(ExchangeError::BadRequest(
            "Lot already expired, please re-list".into(),
        ));
    }
    let base = expires_at.unwrap_or(ts).max(ts);
    let new_exp = base.saturating_add(body.shelf_life_secs);
    sqlx::query(
        "UPDATE exchange_listings SET expires_at = $1, updated_at = $2 WHERE id = $3",
    )
    .bind(new_exp)
    .bind(ts)
    .bind(&lid)
    .execute(&state.db)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    emit_event(
        &state,
        "HermesExternalAction",
        &auth.citizen_name,
        "Listing shelf life extended",
        format!("{svod_code} +{}s → {new_exp}", body.shelf_life_secs),
        &format!("evt_extend_{lid}"),
        json!({
            "action": "listing_extended",
            "source": "exchange",
            "listing_id": lid,
            "svod_code": svod_code,
            "shelf_life_secs": body.shelf_life_secs,
            "expires_at": new_exp,
        }),
    )
    .await?;

    Ok(Json(ApiResponse::success(json!({
        "listing_id": lid,
        "expires_at": new_exp,
        "shelf_life_secs": remaining_shelf_life_secs(Some(new_exp), ts),
        "status": "active",
    }))))
}

/// Expire active listings past expires_at. Does NOT touch node_lot_balances.
pub async fn expire_due_listings(state: &Arc<AppState>) -> Result<u64, String> {
    let ts = now();
    let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, svod_code, lot_scope, node_id FROM exchange_listings
         WHERE status = 'active' AND expires_at IS NOT NULL AND expires_at <= $1",
    )
    .bind(ts)
    .fetch_all(&state.db)
    .await
    .map_err(|e| e.to_string())?;

    let mut n = 0u64;
    for (lid, svod_code, lot_scope, node_id) in rows {
        sqlx::query(
            "UPDATE exchange_listings SET status = 'expired', updated_at = $1 WHERE id = $2",
        )
        .bind(ts)
        .bind(&lid)
        .execute(&state.db)
        .await
        .map_err(|e| e.to_string())?;

        let _ = emit_event(
            state,
            "LotExpired",
            "exchange_agent",
            "Lot expired (shelf_life)",
            format!("{svod_code} listing {lid}"),
            &format!("evt_lotexp_{lid}"),
            json!({
                "listing_id": lid,
                "svod_code": svod_code,
                "lot_scope": lot_scope,
                "node_id": node_id,
                "expired_at": ts,
                "node_balances_untouched": true,
            }),
        )
        .await;
        n += 1;
    }
    Ok(n)
}

/// Background: every 5 minutes expire due listings.
pub async fn background_listing_expiry(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
    loop {
        interval.tick().await;
        match expire_due_listings(&state).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(expired = n, "exchange listings expired (shelf_life)"),
            Err(e) => tracing::warn!(error = %e, "exchange listing expiry failed"),
        }
    }
}

/// POST /exchange/listings/:id/buy — partial buy: debit buyer immediately + event.
/// Item lots (`lot_kind=item`): only mayor/Aiya/Agent/Founder; fulfillment → node warehouse.
pub async fn buy_listing(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(listing_id): Path<String>,
    Json(body): Json<BuyBody>,
) -> ExchangeResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ExchangeError::Unauthorized("Node cannot buy".into()));
    }
    let buyer_id = auth.resolve_account_id(&state.db).await;
    ensure_active_citizen(&state.db, &buyer_id).await?;
    if body.quantity == 0 {
        return Err(ExchangeError::BadRequest("quantity must be > 0".into()));
    }
    let qty = body.quantity as i64;

    // Canon market lot (offer_a → listing): buy opens deal B pending SB.
    if crate::exchange_market::listing_has_offer_deal(&state.db, &listing_id).await? {
        return crate::exchange_market::buy_republic_lot(&state, &auth, &listing_id, qty).await;
    }

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let listing: Option<(
        String,
        String,
        String,
        Option<String>,
        i64,
        i64,
        i64,
        i64,
        String,
        bool,
        String,
        String,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT id, request_id, seller_id, svod_code, qty_remaining, unit_svod,
                unit_buyer_price, unit_seller_price, status, is_state_agent,
                COALESCE(lot_scope, 'global'),
                COALESCE(lot_kind, 'action'),
                item_code
         FROM exchange_listings WHERE id = $1 FOR UPDATE",
    )
    .bind(&listing_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let Some((
        lid,
        request_id,
        seller_id,
        svod_code_opt,
        qty_remaining,
        unit_svod,
        unit_buyer,
        unit_seller,
        status,
        is_state_agent,
        lot_scope,
        lot_kind,
        item_code_opt,
    )) = listing
    else {
        return Err(ExchangeError::NotFound("listing not found".into()));
    };
    if status != "active" {
        return Err(ExchangeError::BadRequest(format!(
            "listing status is {status}"
        )));
    }
    let expires_at: Option<i64> = sqlx::query_scalar(
        "SELECT expires_at FROM exchange_listings WHERE id = $1",
    )
    .bind(&lid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?
    .flatten();
    if let Some(exp) = expires_at {
        if exp <= now() {
            return Err(ExchangeError::BadRequest(
                "Lot already expired, please re-list".into(),
            ));
        }
    }
    if seller_id == buyer_id {
        return Err(ExchangeError::BadRequest("cannot buy own listing".into()));
    }

    // --- Item procurement: stock node warehouse ---
    if lot_kind == "item" {
        let item_code = item_code_opt.ok_or_else(|| {
            ExchangeError::BadRequest("item listing missing item_code".into())
        })?;
        let elevated = can_procure_items(&auth);
        let is_mayor = is_active_mayor(&state.db, &buyer_id)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        if !elevated && !is_mayor {
            return Err(ExchangeError::Unauthorized(
                "Исполнение предметного лота: только мэр узла (или Айя/Секретарь/Founder)".into(),
            ));
        }
        if qty > qty_remaining {
            return Err(ExchangeError::BadRequest("quantity exceeds remaining".into()));
        }
        let debit = unit_buyer.saturating_mul(qty);
        let buyer_bal = lock_balance(&mut tx, &buyer_id).await?;
        if buyer_bal < debit {
            return Err(ExchangeError::InsufficientBalance);
        }
        let reserve_bal = lock_balance(&mut tx, STATE_RESERVE_ID).await?;
        set_balance(&mut tx, &buyer_id, buyer_bal - debit).await?;
        set_balance(&mut tx, STATE_RESERVE_ID, reserve_bal + debit).await?;

        let new_remaining = qty_remaining - qty;
        let listing_status = if new_remaining == 0 { "filled" } else { "active" };
        let ts = now();
        sqlx::query(
            "UPDATE exchange_listings SET qty_remaining = $1, status = $2, updated_at = $3 WHERE id = $4",
        )
        .bind(new_remaining)
        .bind(listing_status)
        .bind(ts)
        .bind(&lid)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        let (node, item_id, prev, new_qty) =
            fulfill_item_to_warehouse(&mut tx, &item_code, qty, ts).await?;

        let deal_id = format!("exdeal_{}", Uuid::new_v4());
        let task_id = format!("extask_{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO exchange_deals
             (id, listing_id, request_id, buyer_id, seller_id, svod_code, item_code, qty,
              unit_svod, unit_buyer_price, unit_seller_price, escrow_amount, status,
              security_task_id, created_at, updated_at, lot_scope, confirm_mode, lot_kind)
             VALUES ($1,$2,$3,$4,$5,NULL,$6,$7,$8,$9,$10,$11,'completed',$12,$13,$13,$14,'agent_sb','item')",
        )
        .bind(&deal_id)
        .bind(&lid)
        .bind(&request_id)
        .bind(&buyer_id)
        .bind(STATE_RESERVE_ID)
        .bind(&item_code)
        .bind(qty)
        .bind(unit_svod)
        .bind(unit_buyer)
        .bind(unit_seller)
        .bind(debit)
        .bind(&task_id)
        .bind(ts)
        .bind(&lot_scope)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        sqlx::query(
            "INSERT INTO exchange_security_tasks
             (id, deal_id, from_citizen, to_citizen, svod_code, qty, status,
              created_at, updated_at, confirm_mode, confirmed_by)
             VALUES ($1,$2,$3,$4,$5,$6,'confirmed',$7,$7,'agent_sb','agent_sb')",
        )
        .bind(&task_id)
        .bind(&deal_id)
        .bind(STATE_RESERVE_ID)
        .bind(&buyer_id)
        .bind(&item_code)
        .bind(qty)
        .bind(ts)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        sqlx::query(
            "UPDATE exchange_needs SET status = 'dealt', updated_at = $1
             WHERE listing_id = $2 AND status = 'listed'",
        )
        .bind(ts)
        .bind(&lid)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        emit_event(
            &state,
            "ExchangeItemProcured",
            &auth.citizen_name,
            "Предмет обеспечен на склад узла через биржу",
            format!("buy {qty} x item:{item_code} → warehouse {node}"),
            &format!("evt_item_deal_{deal_id}"),
            json!({
                "deal_id": deal_id,
                "listing_id": lid,
                "kind": "item",
                "item_code": item_code,
                "item_id": item_id,
                "quantity": qty,
                "escrow_amount": debit,
                "buyer_id": buyer_id,
                "node_id": node,
                "warehouse_previous": prev,
                "warehouse_quantity": new_qty,
                "source": "exchange",
                "agent": "exchange",
            }),
        )
        .await?;

        let _ = chain_events::emit_signed_event(
            &state,
            "ShopWarehouseStocked",
            &buyer_id,
            "Shop warehouse stocked via exchange",
            &format!("node={node} item={item_code} qty={new_qty}"),
            Some(&format!("shop_stock_ex_{deal_id}")),
            json!({
                "node_id": node,
                "item_id": item_id,
                "item_code": item_code,
                "quantity": new_qty,
                "previous": prev,
                "by": buyer_id,
                "source": "exchange",
                "deal_id": deal_id,
            }),
        )
        .await;

        return Ok(Json(ApiResponse::success(json!({
            "deal_id": deal_id,
            "security_task_id": task_id,
            "kind": "item",
            "item_code": item_code,
            "escrow_amount": debit,
            "status": "completed",
            "listing_id": lid,
            "listing_qty_remaining": new_remaining,
            "node_id": node,
            "warehouse_quantity": new_qty,
            "warehouse_previous": prev,
            "seller_id": STATE_RESERVE_ID,
            "seller_name": FOUNDER_DISPLAY_NAME,
            "agent": "exchange",
        }))));
    }

    let svod_code = svod_code_opt.ok_or_else(|| {
        ExchangeError::BadRequest("action listing missing svod_code".into())
    })?;

    // --- Агент Биржи: эскроу → СБ по иерархии доступа лота (канон v3) ---
    if is_state_agent {
        let is_global = lot_scope == "global";
        let access_roles: Vec<String> = sqlx::query_scalar(
            "SELECT COALESCE(access_roles, ARRAY[]::text[]) FROM service_catalog WHERE code = $1",
        )
        .bind(&svod_code)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?
        .unwrap_or_default();
        let required_level = svod::access_level_from_access(&access_roles);
        if !svod::role_can_access_lot(&auth.role, &access_roles) {
            return Err(ExchangeError::Unauthorized(format!(
                "Лот {} недоступен для роли {} (нужен уровень {})",
                svod_code,
                auth.role.as_str(),
                required_level.as_str()
            )));
        }
        if is_global {
            if !auth.can_open_state_deal() {
                return Err(ExchangeError::Unauthorized(
                    "Глобальный лот: сделка доступна гражданам и выше".into(),
                ));
            }
        } else if !auth.can_use_exchange() {
            return Err(ExchangeError::Unauthorized(
                "Локальный лот: доступен только Стражам и выше".into(),
            ));
        }
        if qty > qty_remaining {
            return Err(ExchangeError::BadRequest("quantity exceeds remaining".into()));
        }
        let debit = unit_buyer.saturating_mul(qty);
        let buyer_bal = lock_balance(&mut tx, &buyer_id).await?;
        if buyer_bal < debit {
            return Err(ExchangeError::InsufficientBalance);
        }
        let reserve_bal = lock_balance(&mut tx, STATE_RESERVE_ID).await?;
        set_balance(&mut tx, &buyer_id, buyer_bal - debit).await?;
        set_balance(&mut tx, STATE_RESERVE_ID, reserve_bal + debit).await?;

        sqlx::query("UPDATE exchange_listings SET updated_at = $1 WHERE id = $2")
            .bind(now())
            .bind(&lid)
            .execute(&mut *tx)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        let deal_id = format!("exdeal_{}", Uuid::new_v4());
        let task_id = format!("extask_{}", Uuid::new_v4());
        let ts = now();
        let auto_sb = auth.role.meets_access_level(&required_level);
        let confirm_mode = if auto_sb { "agent_sb" } else { "live_guardian" };
        let deal_status = if auto_sb { "completed" } else { "escrowed" };
        let task_status = if auto_sb { "confirmed" } else { "open" };
        let confirmed_by: Option<&str> = if auto_sb {
            Some("agent_sb")
        } else {
            None
        };

        sqlx::query(
            "INSERT INTO exchange_deals
             (id, listing_id, request_id, buyer_id, seller_id, svod_code, qty,
              unit_svod, unit_buyer_price, unit_seller_price, escrow_amount, status,
              security_task_id, created_at, updated_at, lot_scope, confirm_mode, lot_kind)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$14,$15,$16,'action')",
        )
        .bind(&deal_id)
        .bind(&lid)
        .bind(&request_id)
        .bind(&buyer_id)
        .bind(STATE_RESERVE_ID)
        .bind(&svod_code)
        .bind(qty)
        .bind(unit_svod)
        .bind(unit_buyer)
        .bind(unit_seller)
        .bind(debit)
        .bind(deal_status)
        .bind(&task_id)
        .bind(ts)
        .bind(&lot_scope)
        .bind(confirm_mode)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        sqlx::query(
            "INSERT INTO exchange_security_tasks
             (id, deal_id, from_citizen, to_citizen, svod_code, qty, status,
              created_at, updated_at, confirm_mode, confirmed_by)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$8,$9,$10)",
        )
        .bind(&task_id)
        .bind(&deal_id)
        .bind(STATE_RESERVE_ID)
        .bind(&buyer_id)
        .bind(&svod_code)
        .bind(qty)
        .bind(task_status)
        .bind(ts)
        .bind(confirm_mode)
        .bind(confirmed_by)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        if is_global {
            let inv = lock_inventory(&mut tx, &buyer_id, &svod_code).await?;
            set_inventory(&mut tx, &buyer_id, &svod_code, inv + qty).await?;
            sqlx::query(
                "UPDATE exchange_needs SET status = 'dealt', updated_at = $1
                 WHERE listing_id = $2 AND citizen_id = $3 AND status = 'listed'",
            )
            .bind(ts)
            .bind(&lid)
            .bind(&buyer_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

        let evt_type = if is_global {
            "ExchangeAgentDeal"
        } else {
            "ExchangeDealOpened"
        };
        emit_event(
            &state,
            evt_type,
            &auth.citizen_name,
            if is_global {
                "Глобальный лот: Агент СБ подтвердил"
            } else {
                "Локальный лот: ожидает живого Стража"
            },
            format!("buy {qty} x {svod_code} scope={lot_scope} mode={confirm_mode}"),
            &format!("evt_agent_deal_{deal_id}"),
            json!({
                "deal_id": deal_id,
                "listing_id": lid,
                "kind": "action",
                "svod_code": svod_code,
                "quantity": qty,
                "escrow_amount": debit,
                "buyer_id": buyer_id,
                "seller_id": STATE_RESERVE_ID,
                "security_task_id": task_id,
                "lot_scope": lot_scope,
                "confirm_mode": confirm_mode,
                "listing_status": "active",
                "pending_sb": !is_global,
                "agent": "exchange",
            }),
        )
        .await?;

        return Ok(Json(ApiResponse::success(json!({
            "deal_id": deal_id,
            "security_task_id": task_id,
            "kind": "action",
            "escrow_amount": debit,
            "status": deal_status,
            "listing_id": lid,
            "listing_status": "active",
            "lot_scope": lot_scope,
            "confirm_mode": confirm_mode,
            "seller_id": STATE_RESERVE_ID,
            "seller_name": FOUNDER_DISPLAY_NAME,
            "agent": "exchange",
            "pending_sb": !is_global,
        }))));
    }

    if !auth.can_use_exchange() {
        return Err(ExchangeError::Unauthorized(
            "Биржа доступна только статусу Страж и выше".into(),
        ));
    }
    if qty > qty_remaining {
        return Err(ExchangeError::BadRequest("quantity exceeds remaining".into()));
    }

    let escrow = unit_buyer.saturating_mul(qty);
    let buyer_bal = lock_balance(&mut tx, &buyer_id).await?;
    if buyer_bal < escrow {
        return Err(ExchangeError::InsufficientBalance);
    }
    let treasury_bal = lock_balance(&mut tx, TREASURY_ID).await?;
    set_balance(&mut tx, &buyer_id, buyer_bal - escrow).await?;
    set_balance(&mut tx, TREASURY_ID, treasury_bal + escrow).await?;

    let new_remaining = qty_remaining - qty;
    let listing_status = if new_remaining == 0 { "filled" } else { "active" };
    sqlx::query(
        "UPDATE exchange_listings SET qty_remaining = $1, status = $2, updated_at = $3 WHERE id = $4",
    )
    .bind(new_remaining)
    .bind(listing_status)
    .bind(now())
    .bind(&lid)
    .execute(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let deal_id = format!("exdeal_{}", Uuid::new_v4());
    let task_id = format!("extask_{}", Uuid::new_v4());
    let ts = now();

    sqlx::query(
        "INSERT INTO exchange_deals
         (id, listing_id, request_id, buyer_id, seller_id, svod_code, qty,
          unit_svod, unit_buyer_price, unit_seller_price, escrow_amount, status,
          security_task_id, created_at, updated_at, lot_kind)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,'escrowed',$12,$13,$13,'action')",
    )
    .bind(&deal_id)
    .bind(&lid)
    .bind(&request_id)
    .bind(&buyer_id)
    .bind(&seller_id)
    .bind(&svod_code)
    .bind(qty)
    .bind(unit_svod)
    .bind(unit_buyer)
    .bind(unit_seller)
    .bind(escrow)
    .bind(&task_id)
    .bind(ts)
    .execute(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    sqlx::query(
        "INSERT INTO exchange_security_tasks
         (id, deal_id, from_citizen, to_citizen, svod_code, qty, status, created_at, updated_at)
         VALUES ($1,$2,$3,$4,$5,$6,'open',$7,$7)",
    )
    .bind(&task_id)
    .bind(&deal_id)
    .bind(&seller_id)
    .bind(&buyer_id)
    .bind(&svod_code)
    .bind(qty)
    .bind(ts)
    .execute(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    tx.commit()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    emit_event(
        &state,
        "ExchangeDealOpened",
        &auth.citizen_name,
        "Exchange deal opened",
        format!("buy {qty} x {svod_code} escrow {escrow}"),
        &format!("evt_deal_{deal_id}"),
        json!({
            "deal_id": deal_id,
            "listing_id": lid,
            "request_id": request_id,
            "kind": "action",
            "svod_code": svod_code,
            "quantity": qty,
            "escrow_amount": escrow,
            "unit_buyer_price": unit_buyer,
            "buyer_id": buyer_id,
            "seller_id": seller_id,
            "security_task_id": task_id,
        }),
    )
    .await?;

    Ok(Json(ApiResponse::success(json!({
        "deal_id": deal_id,
        "security_task_id": task_id,
        "kind": "action",
        "escrow_amount": escrow,
        "status": "escrowed",
        "listing_qty_remaining": new_remaining,
    }))))
}

/// POST /exchange/deals/:id/cancel — buyer cancel before delivery; refund at svod rate.
pub async fn cancel_deal(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(deal_id): Path<String>,
) -> ExchangeResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ExchangeError::Unauthorized("Node cannot cancel deals".into()));
    }
    let buyer_id = auth.resolve_account_id(&state.db).await;

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let deal: Option<(String, String, String, String, String, i64, i64, i64, i64, String, Option<String>)> =
        sqlx::query_as(
            "SELECT id, listing_id, buyer_id, seller_id, svod_code, qty, unit_svod,
                    unit_buyer_price, escrow_amount, status, security_task_id
             FROM exchange_deals WHERE id = $1 FOR UPDATE",
        )
        .bind(&deal_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let Some((
        did,
        listing_id,
        deal_buyer,
        _seller_id,
        svod_code,
        qty,
        unit_svod,
        unit_buyer,
        escrow,
        status,
        task_id,
    )) = deal
    else {
        return Err(ExchangeError::NotFound("deal not found".into()));
    };
    if deal_buyer != buyer_id && !auth.can_exchange_security() {
        return Err(ExchangeError::Unauthorized("not your deal".into()));
    }
    if status != "escrowed" && status != "in_delivery" {
        return Err(ExchangeError::BadRequest(format!(
            "cannot cancel deal in status {status}"
        )));
    }

    let refund = unit_svod.saturating_mul(qty);
    let keep = escrow.saturating_sub(refund);

    let buyer_bal = lock_balance(&mut tx, &deal_buyer).await?;
    let treasury_bal = lock_balance(&mut tx, TREASURY_ID).await?;
    if treasury_bal < refund {
        return Err(ExchangeError::BadRequest(
            "treasury cannot refund".into(),
        ));
    }
    set_balance(&mut tx, TREASURY_ID, treasury_bal - refund).await?;
    set_balance(&mut tx, &deal_buyer, buyer_bal + refund).await?;
    // `keep` stays in treasury (spread penalty)

    sqlx::query(
        "UPDATE exchange_listings SET qty_remaining = qty_remaining + $1,
            status = CASE WHEN status = 'filled' THEN 'active' ELSE status END,
            updated_at = $2
         WHERE id = $3",
    )
    .bind(qty)
    .bind(now())
    .bind(&listing_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    // Inventory stays committed to the listing; only qty_remaining is restored.

    sqlx::query("UPDATE exchange_deals SET status = 'cancelled', updated_at = $1 WHERE id = $2")
        .bind(now())
        .bind(&did)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    if let Some(tid) = &task_id {
        sqlx::query(
            "UPDATE exchange_security_tasks SET status = 'cancelled', updated_at = $1 WHERE id = $2",
        )
        .bind(now())
        .bind(tid)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    }

    tx.commit()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    emit_event(
        &state,
        "ExchangeDealCancelled",
        &auth.citizen_name,
        "Exchange deal cancelled",
        format!("refund {refund} keep {keep} (buyer paid {unit_buyer}/u)"),
        &format!("evt_cancel_{did}"),
        json!({
            "deal_id": did,
            "listing_id": listing_id,
            "svod_code": svod_code,
            "quantity": qty,
            "refund": refund,
            "treasury_kept": keep,
            "escrow_amount": escrow,
        }),
    )
    .await?;

    Ok(Json(ApiResponse::success(json!({
        "deal_id": did,
        "status": "cancelled",
        "refund": refund,
        "treasury_kept": keep,
    }))))
}

/// POST /exchange/security/tasks/:id/confirm — SB confirms delivery → settle seller.
pub async fn confirm_delivery(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(task_id): Path<String>,
) -> ExchangeResult<Json<ApiResponse>> {
    if !auth.can_exchange_security() {
        return Err(ExchangeError::Unauthorized(
            "Подтверждение СБ: только Страж и выше".into(),
        ));
    }

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let task: Option<(String, String, String, String, String, i64, String, String)> =
        sqlx::query_as(
            "SELECT id, deal_id, from_citizen, to_citizen, svod_code, qty, status,
                    COALESCE(confirm_mode, 'live_guardian')
             FROM exchange_security_tasks WHERE id = $1 FOR UPDATE",
        )
        .bind(&task_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let Some((tid, deal_id, from_c, to_c, svod_code, qty, tstatus, confirm_mode)) = task else {
        return Err(ExchangeError::NotFound("security task not found".into()));
    };
    if confirm_mode == "agent_sb" {
        return Err(ExchangeError::BadRequest(
            "Лот уже подтверждён СБ автоматически; повторное подтверждение не требуется"
                .into(),
        ));
    }
    if tstatus != "open" {
        return Err(ExchangeError::BadRequest(format!(
            "task status is {tstatus}"
        )));
    }

    // Canon two-deal market: offer_a / buy_b.
    let market_role = crate::exchange_market::deal_role_of(&mut tx, &deal_id).await?;
    if market_role.as_deref() == Some("offer_a") {
        let payload =
            crate::exchange_market::confirm_offer_a(&state, &auth, &mut tx, &tid, &deal_id).await?;
        tx.commit()
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        emit_event(
            &state,
            "ExchangeOfferAccepted",
            &auth.citizen_name,
            "SB accepted citizen offer → republic warehouse + lot",
            format!("offer {deal_id}"),
            &format!("evt_offer_ok_{tid}"),
            payload.clone(),
        )
        .await?;
        return Ok(Json(ApiResponse::success(payload)));
    }
    if market_role.as_deref() == Some("buy_b") {
        let payload =
            crate::exchange_market::confirm_buy_b(&state, &auth, &mut tx, &tid, &deal_id).await?;
        tx.commit()
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        emit_event(
            &state,
            "ExchangeBuySettled",
            &auth.citizen_name,
            "SB confirmed buy → item to buyer, payout to seller",
            format!("buy {deal_id}"),
            &format!("evt_buy_ok_{tid}"),
            payload.clone(),
        )
        .await?;
        return Ok(Json(ApiResponse::success(payload)));
    }

    // Иерархия: Страж не подтверждает лот Судьи; Судья может подтвердить лот Стража.
    let access_roles: Vec<String> = sqlx::query_scalar(
        "SELECT COALESCE(access_roles, ARRAY[]::text[]) FROM service_catalog WHERE code = $1",
    )
    .bind(&svod_code)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?
    .unwrap_or_default();
    let required_level = svod::access_level_from_access(&access_roles);
    if !auth.can_confirm_lot_access(&required_level) {
        return Err(ExchangeError::Unauthorized(format!(
            "Недостаточно уровня для подтверждения: нужен {}, у вас {}",
            required_level.as_str(),
            auth.role.as_str()
        )));
    }

    let deal: Option<(String, String, String, String, i64, i64, i64, String)> = sqlx::query_as(
        "SELECT id, listing_id, seller_id, buyer_id, qty, unit_seller_price, escrow_amount, status
         FROM exchange_deals WHERE id = $1 FOR UPDATE",
    )
    .bind(&deal_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let Some((did, listing_id, seller_id, buyer_id, dqty, unit_seller, escrow, dstatus)) = deal
    else {
        return Err(ExchangeError::NotFound("deal not found".into()));
    };
    if dstatus != "escrowed" && dstatus != "in_delivery" {
        return Err(ExchangeError::BadRequest(format!(
            "deal status is {dstatus}"
        )));
    }

    let pay_seller = unit_seller.saturating_mul(dqty);
    let state_sells = seller_id == STATE_RESERVE_ID;
    let state_buys = buyer_id == STATE_RESERVE_ID;

    if state_sells {
        // Escrow already in Successful reserve; spread stays.
    } else if state_buys {
        // Payout already reserved (debited from QZ-1 at open); credit citizen seller.
        let seller_bal = lock_balance(&mut tx, &seller_id).await?;
        set_balance(&mut tx, &seller_id, seller_bal + escrow).await?;
    } else {
        let treasury_bal = lock_balance(&mut tx, TREASURY_ID).await?;
        if treasury_bal < pay_seller {
            return Err(ExchangeError::BadRequest(
                "treasury insufficient for settlement".into(),
            ));
        }
        let seller_bal = lock_balance(&mut tx, &seller_id).await?;
        set_balance(&mut tx, TREASURY_ID, treasury_bal - pay_seller).await?;
        set_balance(&mut tx, &seller_id, seller_bal + pay_seller).await?;
    }

    // Credit buyer inventory for action lots (not when state is the buyer; not for items).
    let deal_meta: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT COALESCE(lot_kind, 'action'), item_code FROM exchange_deals WHERE id = $1",
    )
    .bind(&deal_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    let (deal_kind, deal_item) = deal_meta.unwrap_or_else(|| ("action".into(), None));
    if deal_kind == "item" {
        if let Some(item_code) = deal_item {
            let ts_now = now();
            let _ = fulfill_item_to_warehouse(&mut tx, &item_code, qty, ts_now).await?;
        }
    } else if !state_buys {
        let buyer_inv = lock_inventory(&mut tx, &buyer_id, &svod_code).await?;
        set_inventory(&mut tx, &buyer_id, &svod_code, buyer_inv + qty).await?;
    }

    if state_sells {
        sqlx::query(
            "UPDATE exchange_needs SET status = 'dealt', updated_at = $1
             WHERE listing_id = $2 AND citizen_id = $3 AND status = 'listed'",
        )
        .bind(now())
        .bind(&listing_id)
        .bind(&buyer_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    }

    let ts = now();
    sqlx::query(
        "UPDATE exchange_security_tasks SET status = 'confirmed', updated_at = $1,
         confirmed_by = COALESCE(confirmed_by, $2) WHERE id = $3",
    )
    .bind(ts)
    .bind(&auth.citizen_name)
    .bind(&tid)
    .execute(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    sqlx::query("UPDATE exchange_deals SET status = 'completed', updated_at = $1 WHERE id = $2")
        .bind(ts)
        .bind(&did)
        .execute(&mut *tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    tx.commit()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let is_state_deal = state_sells || state_buys;

    emit_event(
        &state,
        "ExchangeDeliveryConfirmed",
        &auth.citizen_name,
        "Delivery confirmed by SB",
        format!("{qty} x {svod_code} from {from_c} to {to_c}"),
        &format!("evt_deliv_{tid}"),
        json!({
            "deal_id": did,
            "listing_id": listing_id,
            "task_id": tid,
            "svod_code": svod_code,
            "quantity": qty,
            "from_citizen": from_c,
            "to_citizen": to_c,
            "state_deal": is_state_deal,
        }),
    )
    .await?;

    emit_event(
        &state,
        "ExchangeSettled",
        &auth.citizen_name,
        "Exchange settled",
        format!("seller +{pay_seller}; escrow was {escrow}"),
        &format!("evt_settle_{did}"),
        json!({
            "deal_id": did,
            "listing_id": listing_id,
            "svod_code": svod_code,
            "quantity": dqty,
            "seller_paid": if state_sells { 0 } else if state_buys { escrow } else { pay_seller },
            "escrow_amount": escrow,
            "treasury_spread": if state_sells {
                escrow
            } else if state_buys {
                0
            } else {
                escrow.saturating_sub(pay_seller)
            },
            "seller_id": seller_id,
            "buyer_id": buyer_id,
            "reserve_id": STATE_RESERVE_ID,
            "listing_status": if state_sells { "active" } else { "unchanged" },
        }),
    )
    .await?;

    if state_sells {
        let _ = emit_event(
            &state,
            "ExchangeAgentDeal",
            &auth.citizen_name,
            "Агент Биржи: сделка подтверждена СБ",
            format!("completed {qty} x {svod_code}"),
            &format!("evt_agent_done_{did}"),
            json!({
                "deal_id": did,
                "listing_id": listing_id,
                "svod_code": svod_code,
                "listing_status": "active",
                "agent": "exchange",
            }),
        )
        .await;
    }

    Ok(Json(ApiResponse::success(json!({
        "task_id": tid,
        "deal_id": did,
        "status": "completed",
        "seller_paid": if state_sells { 0 } else if state_buys { escrow } else { pay_seller },
        "treasury_spread": if state_sells {
            escrow
        } else if state_buys {
            0
        } else {
            escrow.saturating_sub(pay_seller)
        },
        "listing_status": if state_sells { Some("active") } else { None },
    }))))
}

/// GET /exchange/security/tasks
pub async fn list_security_tasks(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<ListQuery>,
) -> ExchangeResult<Json<ApiResponse>> {
    if !auth.can_exchange_security() {
        return Err(ExchangeError::Unauthorized(
            "Only Security can list tasks".into(),
        ));
    }
    let status = q.status.unwrap_or_else(|| "open".into());
    let rows = sqlx::query_as::<_, (String, String, String, String, String, i64, String, i64)>(
        "SELECT id, deal_id, from_citizen, to_citizen, svod_code, qty, status, created_at
         FROM exchange_security_tasks WHERE status = $1 ORDER BY created_at DESC LIMIT 200",
    )
    .bind(&status)
    .fetch_all(&state.db)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?
    .into_iter()
    .map(|r| {
        json!({
            "id": r.0, "deal_id": r.1, "from_citizen": r.2, "to_citizen": r.3,
            "svod_code": r.4, "qty": r.5, "status": r.6, "created_at": r.7,
        })
    })
    .collect::<Vec<_>>();
    Ok(Json(ApiResponse::success(rows)))
}

/// GET /exchange/inventory — own inventory; SB/Aiya may pass ?citizen_id=
#[derive(Debug, Deserialize)]
pub struct InventoryQuery {
    pub citizen_id: Option<String>,
}

pub async fn get_inventory_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<InventoryQuery>,
) -> ExchangeResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ExchangeError::Unauthorized("Node cannot read inventory".into()));
    }
    let me = auth.resolve_account_id(&state.db).await;
    let citizen_id = if auth.can_exchange_security() || auth.can_manage_svod() {
        q.citizen_id.unwrap_or(me)
    } else {
        me
    };
    // Canon: item inventory (ITEMS). Prefer /exchange/inventory/items.
    let items = sqlx::query_as::<_, (String, String, i64, i64)>(
        "SELECT i.item_code, COALESCE(s.title, i.item_code), i.quantity, i.updated_at
         FROM citizen_item_inventory i
         LEFT JOIN shop_items s ON s.code = i.item_code
         WHERE i.citizen_id = $1 AND i.quantity > 0
         ORDER BY i.item_code",
    )
    .bind(&citizen_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?
    .into_iter()
    .map(|r| {
        json!({
            "item_code": r.0,
            "title": r.1,
            "quantity": r.2,
            "updated_at": r.3,
        })
    })
    .collect::<Vec<_>>();
    Ok(Json(ApiResponse::success(json!({
        "citizen_id": citizen_id,
        "inventory": items,
        "note": "Инвентарь — предметы Свода предметов. См. также GET /exchange/inventory/items",
    }))))
}

/// POST /exchange/inventory — Aiya/SB adjust inventory (admin seed).
pub async fn adjust_inventory(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<InventoryAdjustBody>,
) -> ExchangeResult<Json<ApiResponse>> {
    if !(auth.can_exchange_security() || auth.can_manage_svod() || auth.can_add_balance()) {
        return Err(ExchangeError::Unauthorized(
            "Insufficient permissions to adjust inventory".into(),
        ));
    }
    if body.quantity < 0 {
        return Err(ExchangeError::BadRequest("quantity must be >= 0".into()));
    }
    let _ = svod::get_service_by_code(&state.db, &body.svod_code, false)
        .await
        .map_err(|e| ExchangeError::BadRequest(e.to_string()))?;
    ensure_active_citizen(&state.db, &body.citizen_id).await?;

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    let _ = lock_inventory(&mut tx, &body.citizen_id, &body.svod_code).await?;
    set_inventory(&mut tx, &body.citizen_id, &body.svod_code, body.quantity).await?;
    tx.commit()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    emit_event(
        &state,
        "ExchangeInventoryAdjusted",
        &auth.citizen_name,
        "Inventory adjusted",
        format!("{} {} = {}", body.citizen_id, body.svod_code, body.quantity),
        &format!("evt_inv_{}_{}", body.citizen_id, Uuid::new_v4()),
        json!({
            "citizen_id": body.citizen_id,
            "svod_code": body.svod_code,
            "quantity": body.quantity,
        }),
    )
    .await?;

    Ok(Json(ApiResponse::success(json!({
        "citizen_id": body.citizen_id,
        "svod_code": body.svod_code,
        "quantity": body.quantity,
    }))))
}

pub async fn get_balance_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> ExchangeResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ExchangeError::Unauthorized(
            "Node credentials cannot query balance".into(),
        ));
    }
    let citizen_id = auth.resolve_account_id(&state.db).await;
    let amount = get_balance(&state.db, &citizen_id).await?;
    Ok(Json(ApiResponse::success(json!({
        "citizen_id": citizen_id,
        "amount": amount,
        "currency": "КВАЗИ",
    }))))
}

pub async fn add_balance(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(request): Json<AddBalanceRequest>,
) -> ExchangeResult<Json<ApiResponse>> {
    if !auth.can_add_balance() {
        return Err(ExchangeError::Unauthorized(
            "Only Aiya/Founder can add balance (резервом распоряжайтесь через POST /exchange/reserve/dispose)".into(),
        ));
    }
    ensure_active_citizen(&state.db, &request.citizen_id).await?;
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    let current = lock_balance(&mut tx, &request.citizen_id).await?;
    let new_amount = current + request.amount as i64;
    set_balance(&mut tx, &request.citizen_id, new_amount).await?;
    tx.commit()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    Ok(Json(ApiResponse::success(json!({
        "citizen_id": request.citizen_id,
        "amount": new_amount,
        "added": request.amount,
    }))))
}

#[derive(Debug, Deserialize)]
pub struct ReserveDisposeBody {
    pub to_citizen_id: String,
    pub amount: u64,
    /// Must be true — Aiya confirms via Secretary
    pub aiya_authorized: bool,
    pub reason: Option<String>,
}

/// POST /exchange/reserve/dispose — Айя через Секретаря распоряжается резервом Successful.
pub async fn dispose_reserve(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<ReserveDisposeBody>,
) -> ExchangeResult<Json<ApiResponse>> {
    if body.amount == 0 {
        return Err(ExchangeError::BadRequest("amount must be > 0".into()));
    }
    if !body.aiya_authorized {
        return Err(ExchangeError::BadRequest(
            "aiya_authorized must be true".into(),
        ));
    }
    // Secretary (Agent) or Aiya/Founder
    let ok = auth.can_submit_need_for_citizen() || auth.can_add_balance();
    if !ok {
        return Err(ExchangeError::Unauthorized(
            "Распоряжение резервом — только Айя через Секретаря".into(),
        ));
    }
    ensure_active_citizen(&state.db, &body.to_citizen_id).await?;

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    let reserve = lock_balance(&mut tx, STATE_RESERVE_ID).await?;
    let amt = body.amount as i64;
    if reserve < amt {
        return Err(ExchangeError::InsufficientBalance);
    }
    let dest = lock_balance(&mut tx, &body.to_citizen_id).await?;
    set_balance(&mut tx, STATE_RESERVE_ID, reserve - amt).await?;
    set_balance(&mut tx, &body.to_citizen_id, dest + amt).await?;
    tx.commit()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    emit_event(
        &state,
        "ExchangeInventoryAdjusted",
        &auth.citizen_name,
        "Reserve disposed via Secretary",
        format!("{amt} КВАЗИ → {}", body.to_citizen_id),
        &format!("evt_reserve_{}", Uuid::new_v4()),
        json!({
            "from": STATE_RESERVE_ID,
            "to_citizen_id": body.to_citizen_id,
            "amount": amt,
            "reason": body.reason,
            "via": "secretary",
        }),
    )
    .await?;

    Ok(Json(ApiResponse::success(json!({
        "from": STATE_RESERVE_ID,
        "to_citizen_id": body.to_citizen_id,
        "amount": amt,
        "reserve_remaining": reserve - amt,
    }))))
}

#[derive(Debug, Deserialize)]
pub struct LotTransferBody {
    /// Local lot code (e.g. video_create). Preferred over listing_id for export.
    pub svod_code: Option<String>,
    /// Optional virtual listing id: `export:{owner_node}:{code}`
    pub listing_id: Option<String>,
    /// City that currently holds the stock (owner).
    pub from_node_id: String,
    /// Buyer city (where Guardian opens the export).
    pub to_node_id: String,
    pub quantity: u64,
}

/// POST /exchange/transfers — шаг 1: Страж города-покупателя открывает экспортную сделку.
/// Цена всегда = цена Свода (без наценок).
pub async fn create_lot_transfer(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<LotTransferBody>,
) -> ExchangeResult<Json<ApiResponse>> {
    if !auth.can_exchange_security() {
        return Err(ExchangeError::Unauthorized(
            "Шаг 1 экспорта — только живой Страж города-покупателя".into(),
        ));
    }
    if body.quantity == 0 {
        return Err(ExchangeError::BadRequest("quantity must be > 0".into()));
    }
    let from_node = body.from_node_id.trim().to_string();
    let to_node = body.to_node_id.trim().to_string();
    if from_node.is_empty() || to_node.is_empty() {
        return Err(ExchangeError::BadRequest(
            "from_node_id and to_node_id required".into(),
        ));
    }
    if from_node == to_node {
        return Err(ExchangeError::BadRequest(
            "Экспорт только между разными городами".into(),
        ));
    }

    let mut svod_code = body
        .svod_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if svod_code.is_none() {
        if let Some(lid) = body.listing_id.as_deref() {
            // export:{owner}:{code} or local:{node}:{code}
            let parts: Vec<&str> = lid.split(':').collect();
            if parts.len() >= 3 && (parts[0] == "export" || parts[0] == "local") {
                svod_code = Some(parts[2..].join(":"));
            }
        }
    }
    let Some(svod_code) = svod_code else {
        return Err(ExchangeError::BadRequest(
            "svod_code or listing_id=export:owner:code required".into(),
        ));
    };

    let service = svod::get_service_by_code(&state.db, &svod_code, true)
        .await
        .map_err(|e| ExchangeError::BadRequest(e.to_string()))?;
    if service.lot_scope != "local" {
        return Err(ExchangeError::BadRequest(
            "Экспорт только для локальных лотов (Доступ без Г)".into(),
        ));
    }
    let unit_svod = service.base_price; // цена Свода, без наценок
    let qty = body.quantity as i64;
    let avail = node_lot_available(&state.db, &from_node, &svod_code).await?;
    if qty > avail {
        return Err(ExchangeError::BadRequest(format!(
            "Недостаточно остатка в городе {from_node}: доступно {avail}"
        )));
    }

    let id = format!("exexport_{}", Uuid::new_v4());
    let ts = now();
    let opened_by = auth.resolve_account_id(&state.db).await;

    sqlx::query(
        "INSERT INTO exchange_lot_transfers
         (id, listing_id, deal_id, svod_code, from_node_id, to_node_id, qty, unit_svod,
          status, step, opened_by, created_at, updated_at)
         VALUES ($1,NULL,NULL,$2,$3,$4,$5,$6,'opened',1,$7,$8,$8)",
    )
    .bind(&id)
    .bind(&svod_code)
    .bind(&from_node)
    .bind(&to_node)
    .bind(qty)
    .bind(unit_svod)
    .bind(&opened_by)
    .bind(ts)
    .execute(&state.db)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    emit_event(
        &state,
        "HermesExternalAction",
        &auth.citizen_name,
        "Export deal opened (step 1)",
        format!("{svod_code}×{qty} @ {unit_svod}: {from_node} → {to_node}"),
        &format!("evt_{id}_1"),
        json!({
            "action": "export_step",
            "source": "exchange",
            "step": 1,
            "status": "opened",
            "transfer_id": id,
            "svod_code": svod_code,
            "from_node_id": from_node,
            "to_node_id": to_node,
            "quantity": qty,
            "unit_svod": unit_svod,
            "price_note": "SVOD price, no export markup",
            "display_status": "Экспорт",
        }),
    )
    .await?;

    Ok(Json(ApiResponse::success(json!({
        "transfer_id": id,
        "svod_code": svod_code,
        "lot_scope": "local",
        "is_export": true,
        "display_status": "Экспорт",
        "unit_svod": unit_svod,
        "unit_buyer_price": unit_svod,
        "from_node_id": from_node,
        "to_node_id": to_node,
        "quantity": qty,
        "step": 1,
        "status": "opened",
        "next_step": 2,
        "next": "POST /exchange/transfers/:id/advance {\"step\":2} — Страж → локальный СБ",
        "ui_line": format!(
            "Лот: {svod_code} | Цена: {unit_svod} КВАЗИ | Доступ: Ст+ | Статус: Экспорт"
        ),
    }))))
}

#[derive(Debug, Deserialize)]
pub struct TransferAdvanceBody {
    /// Protocol step 2..=9
    pub step: i32,
}

/// POST /exchange/transfers/:id/advance — шаги 2–9 протокола экспорта.
pub async fn advance_lot_transfer(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(transfer_id): Path<String>,
    Json(body): Json<TransferAdvanceBody>,
) -> ExchangeResult<Json<ApiResponse>> {
    let target_step = body.step;
    if !(2..=9).contains(&target_step) {
        return Err(ExchangeError::BadRequest(
            "step must be 2..9 (шаг 1 = POST /exchange/transfers)".into(),
        ));
    }

    // Auth by step role
    match target_step {
        2 | 8 => {
            // Buyer-city Guardian (or live guardian)
            if !auth.can_exchange_security() {
                return Err(ExchangeError::Unauthorized(
                    "Шаги 2 и 8 — живой Страж города-покупателя".into(),
                ));
            }
        }
        3 | 4 => {
            // SB agents (buyer→owner / owner reserve)
            if !(auth.can_exchange_security() || auth.can_submit_need_for_citizen()) {
                return Err(ExchangeError::Unauthorized(
                    "Шаги 3–4 — агент СБ или Страж".into(),
                ));
            }
        }
        5 | 6 => {
            // Owner guardian path: step 5 call, step 6 confirm by owner Guardian
            if target_step == 6 && !auth.can_exchange_security() {
                return Err(ExchangeError::Unauthorized(
                    "Шаг 6 — подтверждение передачи живым Стражем города-владельца".into(),
                ));
            }
            if target_step == 5
                && !(auth.can_exchange_security() || auth.can_submit_need_for_citizen())
            {
                return Err(ExchangeError::Unauthorized(
                    "Шаг 5 — СБ владельца вызывает Стража".into(),
                ));
            }
        }
        7 | 9 => {
            // System / SB may finalize listing & close
            if !(auth.can_exchange_security() || auth.can_submit_need_for_citizen()) {
                return Err(ExchangeError::Unauthorized(
                    "Шаги 7 и 9 — СБ / Страж".into(),
                ));
            }
        }
        _ => {}
    }

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let row: Option<(String, String, String, String, i64, i64, i32, String)> = sqlx::query_as(
        "SELECT id, svod_code, from_node_id, to_node_id, qty, unit_svod, step, status
         FROM exchange_lot_transfers WHERE id = $1 FOR UPDATE",
    )
    .bind(&transfer_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    let Some((tid, svod_code, from_node, to_node, qty, unit_svod, cur_step, status)) = row else {
        return Err(ExchangeError::NotFound("export transfer not found".into()));
    };
    if status == "cancelled" || status == "completed" {
        return Err(ExchangeError::BadRequest(format!(
            "transfer already {status}"
        )));
    }
    if target_step != cur_step + 1 {
        return Err(ExchangeError::BadRequest(format!(
            "ожидается шаг {}, сейчас step={cur_step} status={status}",
            cur_step + 1
        )));
    }

    let new_status = export_status_for_step(target_step).unwrap_or("unknown");
    let ts = now();

    match target_step {
        4 => {
            // Owner SB reserves stock
            ensure_node_lot_row(&mut tx, &from_node, &svod_code).await?;
            let bal: Option<(i64, i64)> = sqlx::query_as(
                "SELECT qty, reserved FROM node_lot_balances
                 WHERE node_id = $1 AND svod_code = $2 FOR UPDATE",
            )
            .bind(&from_node)
            .bind(&svod_code)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
            let Some((q, r)) = bal else {
                return Err(ExchangeError::BadRequest("нет остатка у владельца".into()));
            };
            if q - r < qty {
                return Err(ExchangeError::BadRequest(format!(
                    "нельзя резервировать: доступно {}",
                    q - r
                )));
            }
            sqlx::query(
                "UPDATE node_lot_balances SET reserved = reserved + $1, updated_at = $2
                 WHERE node_id = $3 AND svod_code = $4",
            )
            .bind(qty)
            .bind(ts)
            .bind(&from_node)
            .bind(&svod_code)
            .execute(&mut *tx)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        }
        6 => {
            // Owner Guardian confirms → move stock owner → buyer
            ensure_node_lot_row(&mut tx, &from_node, &svod_code).await?;
            ensure_node_lot_row(&mut tx, &to_node, &svod_code).await?;
            let from_bal: Option<(i64, i64)> = sqlx::query_as(
                "SELECT qty, reserved FROM node_lot_balances
                 WHERE node_id = $1 AND svod_code = $2 FOR UPDATE",
            )
            .bind(&from_node)
            .bind(&svod_code)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
            let Some((fq, fr)) = from_bal else {
                return Err(ExchangeError::BadRequest("нет остатка у владельца".into()));
            };
            if fr < qty || fq < qty {
                return Err(ExchangeError::BadRequest(
                    "резерв/остаток владельца недостаточен".into(),
                ));
            }
            sqlx::query(
                "UPDATE node_lot_balances SET qty = qty - $1, reserved = reserved - $1, updated_at = $2
                 WHERE node_id = $3 AND svod_code = $4",
            )
            .bind(qty)
            .bind(ts)
            .bind(&from_node)
            .bind(&svod_code)
            .execute(&mut *tx)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

            let to_bal: Option<(i64, i64)> = sqlx::query_as(
                "SELECT qty, reserved FROM node_lot_balances
                 WHERE node_id = $1 AND svod_code = $2 FOR UPDATE",
            )
            .bind(&to_node)
            .bind(&svod_code)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
            let _ = to_bal;
            sqlx::query(
                "UPDATE node_lot_balances SET qty = qty + $1, updated_at = $2
                 WHERE node_id = $3 AND svod_code = $4",
            )
            .bind(qty)
            .bind(ts)
            .bind(&to_node)
            .bind(&svod_code)
            .execute(&mut *tx)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        }
        7 => {
            // Ensure buyer city has stock visible (already credited at step 6)
            let avail = {
                let row: Option<(i64, i64)> = sqlx::query_as(
                    "SELECT qty, reserved FROM node_lot_balances
                     WHERE node_id = $1 AND svod_code = $2",
                )
                .bind(&to_node)
                .bind(&svod_code)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
                row.map(|(q, r)| q - r).unwrap_or(0)
            };
            if avail <= 0 {
                return Err(ExchangeError::BadRequest(
                    "шаг 7: на бирже покупателя остаток должен быть > 0 (сначала шаг 6)".into(),
                ));
            }
        }
        _ => {}
    }

    sqlx::query(
        "UPDATE exchange_lot_transfers SET status = $1, step = $2, updated_at = $3 WHERE id = $4",
    )
    .bind(new_status)
    .bind(target_step)
    .bind(ts)
    .bind(&tid)
    .execute(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    tx.commit()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let step_title = match target_step {
        2 => "Страж → локальный СБ",
        3 => "Локальный СБ → СБ владельца",
        4 => "СБ владельца резервирует лот",
        5 => "СБ владельца вызывает Стража",
        6 => "Страж владельца подтвердил; остаток перенесён",
        7 => "Лот на локальной бирже покупателя",
        8 => "Страж покупателя подтвердил получение",
        9 => "Сделка закрыта",
        _ => "export step",
    };

    emit_event(
        &state,
        "HermesExternalAction",
        &auth.citizen_name,
        step_title,
        format!("{svod_code}×{qty} step {target_step} → {new_status}"),
        &format!("evt_{tid}_{target_step}"),
        json!({
            "action": "export_step",
            "source": "exchange",
            "step": target_step,
            "status": new_status,
            "transfer_id": tid,
            "svod_code": svod_code,
            "from_node_id": from_node,
            "to_node_id": to_node,
            "quantity": qty,
            "unit_svod": unit_svod,
        }),
    )
    .await?;

    Ok(Json(ApiResponse::success(json!({
        "transfer_id": tid,
        "step": target_step,
        "status": new_status,
        "svod_code": svod_code,
        "from_node_id": from_node,
        "to_node_id": to_node,
        "quantity": qty,
        "unit_svod": unit_svod,
        "display_status": if target_step < 9 { "Экспорт" } else { "Завершено" },
        "next_step": if target_step < 9 { Some(target_step + 1) } else { None },
    }))))
}

#[derive(Debug, Deserialize)]
pub struct NodeLotAdjustBody {
    pub node_id: String,
    pub svod_code: String,
    pub quantity: i64,
    pub reason: Option<String>,
    pub shelf_life_secs: Option<i64>,
}

/// POST /exchange/node-lots — Страж/Айя выставляет остаток локального лота в городе.
pub async fn adjust_node_lot(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<NodeLotAdjustBody>,
) -> ExchangeResult<Json<ApiResponse>> {
    if !(auth.can_exchange_security() || auth.can_add_balance()) {
        return Err(ExchangeError::Unauthorized(
            "Остатки локальных лотов — Страж / Айя".into(),
        ));
    }
    if body.quantity < 0 {
        return Err(ExchangeError::BadRequest("quantity >= 0".into()));
    }
    let service = svod::get_service_by_code(&state.db, &body.svod_code, true)
        .await
        .map_err(|e| ExchangeError::BadRequest(e.to_string()))?;
    if service.lot_scope != "local" {
        return Err(ExchangeError::BadRequest(
            "Только локальные лоты имеют остаток по городу".into(),
        ));
    }
    let node_id = body.node_id.trim();
    let ts = now();
    sqlx::query(
        "INSERT INTO node_lot_balances (node_id, svod_code, qty, reserved, updated_at)
         VALUES ($1, $2, $3, 0, $4)
         ON CONFLICT (node_id, svod_code) DO UPDATE
         SET qty = EXCLUDED.qty, updated_at = EXCLUDED.updated_at,
             reserved = LEAST(node_lot_balances.reserved, EXCLUDED.qty)",
    )
    .bind(node_id)
    .bind(&service.code)
    .bind(body.quantity)
    .bind(ts)
    .execute(&state.db)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let listing_id = upsert_local_city_listing(
        &state.db,
        node_id,
        &service.code,
        body.quantity,
        service.base_price,
        body.shelf_life_secs,
    )
    .await?;

    let actor = auth.resolve_account_id(&state.db).await;
    emit_event(
        &state,
        "ExchangeInventoryAdjusted",
        &auth.citizen_name,
        "Node lot balance set",
        format!("{} @ {} = {}", service.code, node_id, body.quantity),
        &format!("evt_nodelot_{}", Uuid::new_v4()),
        json!({
            "citizen_id": actor,
            "node_id": node_id,
            "svod_code": service.code,
            "quantity": body.quantity,
            "reason": body.reason,
            "lot_scope": "local",
            "listing_id": listing_id,
            "expires_at": resolve_expires_at(body.shelf_life_secs, ts),
        }),
    )
    .await?;

    Ok(Json(ApiResponse::success(json!({
        "node_id": node_id,
        "svod_code": service.code,
        "quantity": body.quantity,
        "unit_svod": service.base_price,
        "listing_id": listing_id,
        "expires_at": resolve_expires_at(body.shelf_life_secs, ts),
        "shelf_life_secs": body.shelf_life_secs.unwrap_or_else(default_shelf_life_secs),
    }))))
}

/// GET /exchange/node-lots?node_id=&svod_code=
pub async fn list_node_lots(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> ExchangeResult<Json<ApiResponse>> {
    let node_id = q.node_id.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let code = q.svod_code.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let rows = sqlx::query_as::<_, (String, String, i64, i64, i64)>(
        "SELECT node_id, svod_code, qty, reserved, updated_at
         FROM node_lot_balances
         WHERE ($1::text IS NULL OR node_id = $1)
           AND ($2::text IS NULL OR svod_code = $2)
         ORDER BY svod_code, node_id LIMIT 500",
    )
    .bind(node_id)
    .bind(code)
    .fetch_all(&state.db)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    let items: Vec<_> = rows
        .into_iter()
        .map(|r| {
            json!({
                "node_id": r.0,
                "svod_code": r.1,
                "qty": r.2,
                "reserved": r.3,
                "available": r.2 - r.3,
                "updated_at": r.4,
            })
        })
        .collect();
    Ok(Json(ApiResponse::success(items)))
}

/// Body for sell-to-state / offer: `item_code` required; `svod_code` = действие (опц.).
#[derive(Debug, Deserialize)]
pub struct SellToStateBody {
    /// Предмет из Свода предметов (обязателен для канона).
    pub item_code: Option<String>,
    /// Действие из Свода действий (как взаимодействует гражданин).
    pub svod_code: Option<String>,
    pub quantity: u64,
    pub shelf_life_secs: Option<i64>,
}

/// POST /exchange/sell-to-state — предложение предмета республике (сделка A → СБ → лот).
pub async fn sell_to_state(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<SellToStateBody>,
) -> ExchangeResult<Json<ApiResponse>> {
    let item_code = body
        .item_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            ExchangeError::BadRequest(
                "Укажите item_code (предмет из Свода предметов). Инвентарь — только предметы."
                    .into(),
            )
        })?;
    crate::exchange_market::offer_item_to_republic(
        State(state),
        Extension(auth),
        Json(crate::exchange_market::OfferItemBody {
            item_code,
            quantity: body.quantity,
            svod_code: body.svod_code,
            shelf_life_secs: body.shelf_life_secs,
        }),
    )
    .await
}
