//! State-mediated exchange (citizen ↔ Quazar treasury via Security).
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
use crate::response::ApiResponse;
use crate::svod;
use crate::AppState;

pub const TREASURY_ID: &str = "quazar";

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

type ExchangeResult<T> = Result<T, ExchangeError>;

fn buyer_unit_price(svod: i64) -> i64 {
    (svod * 11 + 9) / 10
}

fn seller_unit_price(svod: i64) -> i64 {
    (svod * 9) / 10
}

fn now() -> i64 {
    Utc::now().timestamp()
}

async fn ensure_active_citizen(pool: &PgPool, citizen_id: &str) -> ExchangeResult<()> {
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

async fn get_balance(pool: &PgPool, citizen_id: &str) -> ExchangeResult<i64> {
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

async fn lock_balance(
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

async fn set_balance(
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

async fn emit_event(
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
    /// Optional position code from Государственный Свод (`parts.*.items`)
    pub svod_code: Option<String>,
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
          unit_buyer_price, unit_seller_price, status, created_at, updated_at)
         VALUES ($1,$2,$3,$4,$5,$5,$6,$7,$8,'active',$9,$9)",
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
/// Optional `?work_kind=` and `?svod_code=` filter via Свод positions (`service_catalog`).
pub async fn list_listings(
    State(state): State<Arc<AppState>>,
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

    // Join Свод positions for work_kind; listings are sell-side supply after SB verify.
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
        ),
    >(
        "SELECT l.id, l.request_id, l.seller_id, l.svod_code, l.qty_total, l.qty_remaining,
                l.unit_svod, l.unit_buyer_price, l.unit_seller_price, l.status, l.created_at,
                COALESCE(s.work_kind, 'implementation') AS work_kind
         FROM exchange_listings l
         LEFT JOIN service_catalog s ON s.code = l.svod_code
         WHERE l.status = $1
           AND ($2::text IS NULL OR COALESCE(s.work_kind, 'implementation') = $2)
           AND ($3::text IS NULL OR l.svod_code = $3)
         ORDER BY l.created_at DESC LIMIT 200",
    )
    .bind(&status)
    .bind(work_kind.as_deref())
    .bind(svod_code.as_deref())
    .fetch_all(&state.db)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let listings = rows
        .into_iter()
        .map(|r| {
            let work_kind = r.11;
            json!({
                "id": r.0,
                "request_id": r.1,
                "seller_id": r.2,
                "svod_code": r.3,
                "qty_total": r.4,
                "qty_remaining": r.5,
                "unit_svod": r.6,
                "unit_buyer_price": r.7,
                "unit_seller_price": r.8,
                "status": r.9,
                "created_at": r.10,
                "work_kind": work_kind,
                "work_kind_label": svod::work_kind_label(&work_kind),
                // Listings open from verified sell requests (citizen → treasury supply).
                "side": "sell",
                "side_semantics": "state_mediated_listing",
                "mode": "offers",
                "role_hint": "offers",
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(ApiResponse::success(listings)))
}

/// POST /exchange/listings/:id/buy — partial buy: debit buyer immediately + event.
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

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let listing: Option<(String, String, String, String, i64, i64, i64, i64, String)> =
        sqlx::query_as(
            "SELECT id, request_id, seller_id, svod_code, qty_remaining, unit_svod,
                    unit_buyer_price, unit_seller_price, status
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
        svod_code,
        qty_remaining,
        unit_svod,
        unit_buyer,
        unit_seller,
        status,
    )) = listing
    else {
        return Err(ExchangeError::NotFound("listing not found".into()));
    };
    if status != "active" {
        return Err(ExchangeError::BadRequest(format!(
            "listing status is {status}"
        )));
    }
    if seller_id == buyer_id {
        return Err(ExchangeError::BadRequest("cannot buy own listing".into()));
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
          security_task_id, created_at, updated_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,'escrowed',$12,$13,$13)",
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
            "Only Security (Guardian) can confirm delivery".into(),
        ));
    }

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let task: Option<(String, String, String, String, String, i64, String)> = sqlx::query_as(
        "SELECT id, deal_id, from_citizen, to_citizen, svod_code, qty, status
         FROM exchange_security_tasks WHERE id = $1 FOR UPDATE",
    )
    .bind(&task_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let Some((tid, deal_id, from_c, to_c, svod_code, qty, tstatus)) = task else {
        return Err(ExchangeError::NotFound("security task not found".into()));
    };
    if tstatus != "open" {
        return Err(ExchangeError::BadRequest(format!(
            "task status is {tstatus}"
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
    let treasury_bal = lock_balance(&mut tx, TREASURY_ID).await?;
    if treasury_bal < pay_seller {
        return Err(ExchangeError::BadRequest(
            "treasury insufficient for settlement".into(),
        ));
    }
    let seller_bal = lock_balance(&mut tx, &seller_id).await?;
    set_balance(&mut tx, TREASURY_ID, treasury_bal - pay_seller).await?;
    set_balance(&mut tx, &seller_id, seller_bal + pay_seller).await?;
    // Spread escrow - pay_seller remains in treasury

    // Credit buyer inventory
    let buyer_inv = lock_inventory(&mut tx, &buyer_id, &svod_code).await?;
    set_inventory(&mut tx, &buyer_id, &svod_code, buyer_inv + qty).await?;

    let ts = now();
    sqlx::query(
        "UPDATE exchange_security_tasks SET status = 'confirmed', updated_at = $1 WHERE id = $2",
    )
    .bind(ts)
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
            "seller_paid": pay_seller,
            "escrow_amount": escrow,
            "treasury_spread": escrow.saturating_sub(pay_seller),
            "seller_id": seller_id,
            "buyer_id": buyer_id,
        }),
    )
    .await?;

    Ok(Json(ApiResponse::success(json!({
        "task_id": tid,
        "deal_id": did,
        "status": "completed",
        "seller_paid": pay_seller,
        "treasury_spread": escrow.saturating_sub(pay_seller),
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
    let rows = sqlx::query_as::<_, (String, String, i64, i64)>(
        "SELECT citizen_id, svod_code, quantity, updated_at FROM exchange_inventory
         WHERE citizen_id = $1 ORDER BY svod_code",
    )
    .bind(&citizen_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?
    .into_iter()
    .map(|r| json!({"citizen_id": r.0, "svod_code": r.1, "quantity": r.2, "updated_at": r.3}))
    .collect::<Vec<_>>();
    Ok(Json(ApiResponse::success(rows)))
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
            "Only Aiya/Founder can add balance".into(),
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

// --- Temporary compatibility for Passport apps still on P2P paths ---

#[derive(Debug, Deserialize)]
pub struct LegacyOffersQuery {
    pub status: Option<String>,
    pub service: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LegacyOrdersQuery {
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LegacyCreateOfferBody {
    pub svod_code: Option<String>,
    pub service: Option<String>,
    pub price: Option<u64>,
    pub quantity: u64,
}

#[derive(Debug, Deserialize)]
pub struct LegacyCreateOrderBody {
    pub offer_id: String,
    pub quantity: u64,
}

fn listing_as_legacy_offer(
    id: &str,
    seller_id: &str,
    svod_code: &str,
    qty_remaining: i64,
    unit_buyer_price: i64,
    status: &str,
    created_at: i64,
) -> serde_json::Value {
    json!({
        "id": id,
        "seller": seller_id,
        "service": svod_code,
        "svod_code": svod_code,
        "price": unit_buyer_price,
        "quantity": qty_remaining,
        "status": status,
        "created_at": created_at,
        "legacy_alias": true,
        "source": "exchange_listings",
    })
}

/// GET /exchange/offers — alias of active treasury listings in old offer JSON shape.
pub async fn legacy_get_offers(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LegacyOffersQuery>,
) -> ExchangeResult<Json<ApiResponse>> {
    let status = q.status.unwrap_or_else(|| "active".into());
    let rows = sqlx::query_as::<_, (String, String, String, i64, i64, String, i64)>(
        "SELECT id, seller_id, svod_code, qty_remaining, unit_buyer_price, status, created_at
         FROM exchange_listings WHERE status = $1 ORDER BY created_at DESC LIMIT 200",
    )
    .bind(&status)
    .fetch_all(&state.db)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let offers = rows
        .into_iter()
        .filter(|r| {
            q.service
                .as_ref()
                .map(|s| r.2.to_lowercase().contains(&s.to_lowercase()))
                .unwrap_or(true)
        })
        .map(|r| listing_as_legacy_offer(&r.0, &r.1, &r.2, r.3, r.4, &r.5, r.6))
        .collect::<Vec<_>>();
    Ok(Json(ApiResponse::success(offers)))
}

/// GET /exchange/offer/:id — listing as legacy offer.
pub async fn legacy_get_offer_by_id(
    State(state): State<Arc<AppState>>,
    Path(offer_id): Path<String>,
) -> ExchangeResult<Json<ApiResponse>> {
    let row = sqlx::query_as::<_, (String, String, String, i64, i64, String, i64)>(
        "SELECT id, seller_id, svod_code, qty_remaining, unit_buyer_price, status, created_at
         FROM exchange_listings WHERE id = $1",
    )
    .bind(&offer_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?
    .ok_or_else(|| ExchangeError::NotFound(format!("Offer/listing {offer_id} not found")))?;
    Ok(Json(ApiResponse::success(listing_as_legacy_offer(
        &row.0, &row.1, &row.2, row.3, row.4, &row.5, row.6,
    ))))
}

/// POST /exchange/offer — maps to sell request (price ignored; svod ±10% applies after SB).
pub async fn legacy_create_offer(
    state: State<Arc<AppState>>,
    auth: Extension<AuthContext>,
    Json(body): Json<LegacyCreateOfferBody>,
) -> ExchangeResult<Json<ApiResponse>> {
    let svod_code = body
        .svod_code
        .or(body.service)
        .ok_or_else(|| ExchangeError::BadRequest("svod_code is required".into()))?;
    let _ = body.price; // catalog rate is authoritative in state-mediated flow
    create_request(
        state,
        auth,
        Json(CreateRequestBody {
            side: "sell".into(),
            svod_code,
            quantity: body.quantity,
        }),
    )
    .await
}

/// DELETE /exchange/offer/:id — not supported on treasury listings; cancel deal path differs.
pub async fn legacy_cancel_offer(
    Path(offer_id): Path<String>,
) -> ExchangeResult<Json<ApiResponse>> {
    Err(ExchangeError::BadRequest(format!(
        "P2P cancel removed. Use POST /exchange/deals/:id/cancel (deal id), not offer {offer_id}"
    )))
}

/// POST /exchange/order — maps offer_id to listing buy.
pub async fn legacy_create_order(
    state: State<Arc<AppState>>,
    auth: Extension<AuthContext>,
    Json(body): Json<LegacyCreateOrderBody>,
) -> ExchangeResult<Json<ApiResponse>> {
    buy_listing(
        state,
        auth,
        Path(body.offer_id),
        Json(BuyBody {
            quantity: body.quantity,
        }),
    )
    .await
}

/// GET /exchange/orders — buyer's deals in old order JSON shape.
pub async fn legacy_get_orders(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<LegacyOrdersQuery>,
) -> ExchangeResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ExchangeError::Unauthorized(
            "Node credentials cannot list orders".into(),
        ));
    }
    let citizen_id = auth.resolve_account_id(&state.db).await;
    let rows = sqlx::query_as::<_, (String, String, String, i64, i64, String, i64)>(
        "SELECT id, buyer_id, listing_id, qty, escrow_amount, status, created_at
         FROM exchange_deals WHERE buyer_id = $1 ORDER BY created_at DESC LIMIT 200",
    )
    .bind(&citizen_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let orders = rows
        .into_iter()
        .filter(|r| q.status.as_ref().map(|s| s == &r.5).unwrap_or(true))
        .map(|r| {
            json!({
                "id": r.0,
                "buyer": r.1,
                "offer_id": r.2,
                "quantity": r.3,
                "total_price": r.4,
                "status": r.5,
                "created_at": r.6,
                "legacy_alias": true,
                "source": "exchange_deals",
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(ApiResponse::success(orders)))
}
