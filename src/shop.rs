//! Node shop: Svod of Items + per-node warehouse + purchases.
//!
//! Exchange (Svod of Actions) = procurement channel to fill the warehouse.
//! Shop = storefront of **this** node's warehouse stock.
//! Interim settlement: KVAZI with 10% → QZ-1, 90% → active mayor (ЭК fiat rails = later).

use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use sqlx::{PgConnection, PgPool};
use std::sync::Arc;
use uuid::Uuid;

use crate::auth::AuthContext;
use crate::chain_events;
use crate::exchange::STATE_RESERVE_ID;
use crate::offices::{active_mayor_of, is_active_mayor};
use crate::response::ApiResponse;
use crate::AppState;

fn node_id() -> String {
    std::env::var("QUAZAR_NODE_ID").unwrap_or_else(|_| "QZ-NODE-1".into())
}

fn now_ts() -> chrono::DateTime<Utc> {
    Utc::now()
}

#[derive(Debug)]
pub enum ShopError {
    DatabaseError(String),
    NotFound(String),
    InsufficientBalance,
    InsufficientStock,
    Unauthorized(String),
    BadRequest(String),
}

impl ShopError {
    fn status_code(&self) -> StatusCode {
        match self {
            ShopError::DatabaseError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ShopError::NotFound(_) => StatusCode::NOT_FOUND,
            ShopError::InsufficientBalance | ShopError::InsufficientStock => StatusCode::CONFLICT,
            ShopError::Unauthorized(_) => StatusCode::FORBIDDEN,
            ShopError::BadRequest(_) => StatusCode::BAD_REQUEST,
        }
    }
}

impl std::fmt::Display for ShopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShopError::DatabaseError(s)
            | ShopError::NotFound(s)
            | ShopError::Unauthorized(s)
            | ShopError::BadRequest(s) => write!(f, "{s}"),
            ShopError::InsufficientBalance => write!(f, "Недостаточно КВАЗИ"),
            ShopError::InsufficientStock => write!(f, "Недостаточно остатка на складе узла"),
        }
    }
}

impl IntoResponse for ShopError {
    fn into_response(self) -> axum::response::Response {
        (self.status_code(), Json(ApiResponse::error(self.to_string()))).into_response()
    }
}

type ShopResult<T> = Result<T, ShopError>;

async fn ensure_active_citizen(pool: &PgPool, citizen_id: &str) -> ShopResult<()> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM citizens WHERE id = $1")
        .bind(citizen_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| ShopError::DatabaseError(e.to_string()))?;
    match status.as_deref() {
        Some("active") => Ok(()),
        Some(other) => Err(ShopError::BadRequest(format!(
            "Магазин недоступен: статус гражданина {other} (нужен active + VPN)"
        ))),
        None => Err(ShopError::NotFound("Citizen not found".into())),
    }
}

async fn lock_balance(tx: &mut PgConnection, citizen_id: &str) -> ShopResult<i64> {
    sqlx::query(
        "INSERT INTO balances (citizen_id, amount) VALUES ($1, 0)
         ON CONFLICT (citizen_id) DO NOTHING",
    )
    .bind(citizen_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| ShopError::DatabaseError(e.to_string()))?;
    sqlx::query_scalar("SELECT amount FROM balances WHERE citizen_id = $1 FOR UPDATE")
        .bind(citizen_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| ShopError::DatabaseError(e.to_string()))
}

async fn set_balance(tx: &mut PgConnection, citizen_id: &str, amount: i64) -> ShopResult<()> {
    sqlx::query("UPDATE balances SET amount = $1 WHERE citizen_id = $2")
        .bind(amount)
        .bind(citizen_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| ShopError::DatabaseError(e.to_string()))?;
    Ok(())
}

async fn lock_warehouse_qty(
    tx: &mut PgConnection,
    node: &str,
    item_id: Uuid,
) -> ShopResult<i64> {
    let row: Option<i64> = sqlx::query_scalar(
        "SELECT quantity FROM shop_warehouse
         WHERE node_id = $1 AND item_id = $2 FOR UPDATE",
    )
    .bind(node)
    .bind(item_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ShopError::DatabaseError(e.to_string()))?;
    Ok(row.unwrap_or(0))
}

/// Resolve active shop item by code (Items svod).
pub async fn item_id_by_code(pool: &PgPool, code: &str) -> Result<(Uuid, i64), String> {
    let row: Option<(Uuid, i64)> = sqlx::query_as(
        "SELECT id, price_kvazi FROM shop_items WHERE code = $1 AND is_active = TRUE",
    )
    .bind(code)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;
    row.ok_or_else(|| format!("Unknown or inactive item_code: {code}"))
}

/// Same as [`item_id_by_code`] inside an open transaction.
pub async fn item_id_by_code_tx(
    tx: &mut PgConnection,
    code: &str,
) -> Result<(Uuid, i64), String> {
    let row: Option<(Uuid, i64)> = sqlx::query_as(
        "SELECT id, price_kvazi FROM shop_items WHERE code = $1 AND is_active = TRUE",
    )
    .bind(code)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    row.ok_or_else(|| format!("Unknown or inactive item_code: {code}"))
}

/// Credit node warehouse by delta (exchange procurement / mayor stock).
/// Returns (previous_qty, new_qty).
pub async fn credit_warehouse_delta(
    tx: &mut PgConnection,
    node: &str,
    item_id: Uuid,
    delta: i64,
) -> Result<(i64, i64), String> {
    if delta == 0 {
        return Err("delta must be non-zero".into());
    }
    sqlx::query(
        "INSERT INTO shop_warehouse (id, node_id, item_id, quantity, updated_at)
         VALUES ($1, $2, $3, 0, $4)
         ON CONFLICT (node_id, item_id) DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(node)
    .bind(item_id)
    .bind(now_ts())
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    let current = lock_warehouse_qty(tx, node, item_id)
        .await
        .map_err(|e| e.to_string())?;
    let new_qty = current + delta;
    if new_qty < 0 {
        return Err("stock would go negative".into());
    }
    sqlx::query(
        "UPDATE shop_warehouse SET quantity = $1, updated_at = $2
         WHERE node_id = $3 AND item_id = $4",
    )
    .bind(new_qty)
    .bind(now_ts())
    .bind(node)
    .bind(item_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    Ok((current, new_qty))
}

/// Node id for this process (same as shop handlers).
pub fn this_node_id() -> String {
    node_id()
}

/// GET /api/shop/items — warehouse storefront for this node (stock > 0 by default).
#[derive(Debug, Deserialize)]
pub struct ListItemsQuery {
    /// If true, include zero-stock rows.
    pub all: Option<bool>,
}

pub async fn list_items(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<ListItemsQuery>,
) -> ShopResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ShopError::Unauthorized(
            "Node credentials cannot browse shop".into(),
        ));
    }
    let node = node_id();
    let include_zero = q.all.unwrap_or(false);
    let rows: Vec<(
        Uuid,
        String,
        String,
        String,
        String,
        String,
        i64,
        i64,
    )> = if include_zero {
        sqlx::query_as(
            "SELECT i.id, i.code, i.title, i.category, i.unit, i.description,
                    i.price_kvazi, COALESCE(w.quantity, 0)
             FROM shop_items i
             LEFT JOIN shop_warehouse w ON w.item_id = i.id AND w.node_id = $1
             WHERE i.is_active = TRUE
             ORDER BY i.category, i.title",
        )
        .bind(&node)
        .fetch_all(&state.db)
        .await
        .map_err(|e| ShopError::DatabaseError(e.to_string()))?
    } else {
        sqlx::query_as(
            "SELECT i.id, i.code, i.title, i.category, i.unit, i.description,
                    i.price_kvazi, w.quantity
             FROM shop_items i
             INNER JOIN shop_warehouse w ON w.item_id = i.id AND w.node_id = $1
             WHERE i.is_active = TRUE AND w.quantity > 0
             ORDER BY i.category, i.title",
        )
        .bind(&node)
        .fetch_all(&state.db)
        .await
        .map_err(|e| ShopError::DatabaseError(e.to_string()))?
    };

    let items: Vec<_> = rows
        .into_iter()
        .map(
            |(id, code, title, category, unit, description, price_kvazi, quantity)| {
                json!({
                    "id": id,
                    "code": code,
                    "title": title,
                    "category": category,
                    "unit": unit,
                    "description": description,
                    "price_kvazi": price_kvazi,
                    "currency": "КВАЗИ",
                    "quantity": quantity,
                    "node_id": node,
                })
            },
        )
        .collect();

    Ok(Json(ApiResponse::success(json!({
        "node_id": node,
        "svod": "items",
        "items": items,
    }))))
}

/// GET /api/shop/items/:id
pub async fn get_item(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> ShopResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ShopError::Unauthorized(
            "Node credentials cannot browse shop".into(),
        ));
    }
    let node = node_id();
    let row: Option<(
        Uuid,
        String,
        String,
        String,
        String,
        String,
        i64,
        i64,
    )> = sqlx::query_as(
        "SELECT i.id, i.code, i.title, i.category, i.unit, i.description,
                i.price_kvazi, COALESCE(w.quantity, 0)
         FROM shop_items i
         LEFT JOIN shop_warehouse w ON w.item_id = i.id AND w.node_id = $1
         WHERE i.id = $2 AND i.is_active = TRUE",
    )
    .bind(&node)
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| ShopError::DatabaseError(e.to_string()))?;

    let Some((id, code, title, category, unit, description, price_kvazi, quantity)) = row else {
        return Err(ShopError::NotFound("Item not found".into()));
    };

    Ok(Json(ApiResponse::success(json!({
        "id": id,
        "code": code,
        "title": title,
        "category": category,
        "unit": unit,
        "description": description,
        "price_kvazi": price_kvazi,
        "currency": "КВАЗИ",
        "quantity": quantity,
        "node_id": node,
        "svod": "items",
    }))))
}

#[derive(Debug, Deserialize)]
pub struct BuyBody {
    pub item_id: Uuid,
    pub quantity: u64,
}

/// POST /api/shop/buy — debit KVAZI, 10% state / 90% mayor, reduce warehouse, emit event.
pub async fn buy_item(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<BuyBody>,
) -> ShopResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ShopError::Unauthorized(
            "Node credentials cannot buy".into(),
        ));
    }
    if body.quantity == 0 {
        return Err(ShopError::BadRequest("quantity must be > 0".into()));
    }
    let citizen_id = auth.resolve_account_id(&state.db).await;
    ensure_active_citizen(&state.db, &citizen_id).await?;
    let node = node_id();

    let item: Option<(String, String, i64)> = sqlx::query_as(
        "SELECT code, title, price_kvazi FROM shop_items WHERE id = $1 AND is_active = TRUE",
    )
    .bind(body.item_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| ShopError::DatabaseError(e.to_string()))?;
    let Some((code, title, unit_price)) = item else {
        return Err(ShopError::NotFound("Item not found".into()));
    };

    let qty = body.quantity as i64;
    let total = unit_price
        .checked_mul(qty)
        .ok_or_else(|| ShopError::BadRequest("total overflow".into()))?;
    let commission = total / 10; // 10% state
    let mayor_share = total - commission;

    let mayor = active_mayor_of(&state.db, &node)
        .await
        .map_err(|e| ShopError::DatabaseError(e.to_string()))?;
    let mayor_id = mayor.as_ref().map(|(id, _)| id.clone());

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ShopError::DatabaseError(e.to_string()))?;

    let stock = lock_warehouse_qty(&mut tx, &node, body.item_id).await?;
    if stock < qty {
        return Err(ShopError::InsufficientStock);
    }

    let bal = lock_balance(&mut tx, &citizen_id).await?;
    if bal < total {
        return Err(ShopError::InsufficientBalance);
    }
    set_balance(&mut tx, &citizen_id, bal - total).await?;

    let reserve = lock_balance(&mut tx, STATE_RESERVE_ID).await?;
    set_balance(&mut tx, STATE_RESERVE_ID, reserve + commission).await?;

    if let Some(ref mid) = mayor_id {
        if mid != &citizen_id {
            let mb = lock_balance(&mut tx, mid).await?;
            set_balance(&mut tx, mid, mb + mayor_share).await?;
        } else {
            // Buyer is mayor: keep mayor_share (already deducted total; add back share)
            let mb = lock_balance(&mut tx, mid).await?;
            set_balance(&mut tx, mid, mb + mayor_share).await?;
        }
    } else {
        // No mayor — entire remainder to state reserve
        let reserve2 = lock_balance(&mut tx, STATE_RESERVE_ID).await?;
        set_balance(&mut tx, STATE_RESERVE_ID, reserve2 + mayor_share).await?;
    }

    sqlx::query(
        "UPDATE shop_warehouse SET quantity = $1, updated_at = $2
         WHERE node_id = $3 AND item_id = $4",
    )
    .bind(stock - qty)
    .bind(now_ts())
    .bind(&node)
    .bind(body.item_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| ShopError::DatabaseError(e.to_string()))?;

    let purchase_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO shop_purchases
         (id, citizen_id, item_id, node_id, quantity, total_price, commission, mayor_share,
          mayor_citizen_id, currency, created_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,'КВАЗИ',$10)",
    )
    .bind(purchase_id)
    .bind(&citizen_id)
    .bind(body.item_id)
    .bind(&node)
    .bind(qty)
    .bind(total)
    .bind(commission)
    .bind(if mayor_id.is_some() {
        mayor_share
    } else {
        0
    })
    .bind(mayor_id.as_deref())
    .bind(now_ts())
    .execute(&mut *tx)
    .await
    .map_err(|e| ShopError::DatabaseError(e.to_string()))?;

    tx.commit()
        .await
        .map_err(|e| ShopError::DatabaseError(e.to_string()))?;

    let event_id = format!("shop_purchase_{purchase_id}");
    let _ = chain_events::emit_signed_event(
        &state,
        "ShopPurchase",
        &citizen_id,
        &format!("Shop purchase: {title}"),
        &format!("{code} × {qty} on {node}"),
        Some(&event_id),
        json!({
            "purchase_id": purchase_id,
            "citizen_id": citizen_id,
            "item_id": body.item_id,
            "code": code,
            "node_id": node,
            "quantity": qty,
            "total_price": total,
            "commission": commission,
            "mayor_share": mayor_share,
            "mayor_citizen_id": mayor_id,
            "currency": "КВАЗИ",
        }),
    )
    .await;

    Ok(Json(ApiResponse::success(json!({
        "purchase_id": purchase_id,
        "item_id": body.item_id,
        "code": code,
        "title": title,
        "quantity": qty,
        "total_price": total,
        "commission": commission,
        "mayor_share": mayor_share,
        "mayor_citizen_id": mayor_id,
        "node_id": node,
        "currency": "КВАЗИ",
        "remaining_stock": stock - qty,
    }))))
}

/// GET /api/shop/history
#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub limit: Option<i64>,
}

pub async fn history(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<HistoryQuery>,
) -> ShopResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ShopError::Unauthorized(
            "Node credentials cannot query history".into(),
        ));
    }
    let citizen_id = auth.resolve_account_id(&state.db).await;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let rows: Vec<(
        Uuid,
        Uuid,
        String,
        String,
        String,
        i64,
        i64,
        i64,
        chrono::DateTime<Utc>,
    )> = sqlx::query_as(
        "SELECT p.id, p.item_id, i.code, i.title, p.node_id, p.quantity,
                p.total_price, p.commission, p.created_at
         FROM shop_purchases p
         JOIN shop_items i ON i.id = p.item_id
         WHERE p.citizen_id = $1
         ORDER BY p.created_at DESC
         LIMIT $2",
    )
    .bind(&citizen_id)
    .bind(limit)
    .fetch_all(&state.db)
    .await
    .map_err(|e| ShopError::DatabaseError(e.to_string()))?;

    let purchases: Vec<_> = rows
        .into_iter()
        .map(
            |(id, item_id, code, title, node, quantity, total_price, commission, created_at)| {
                json!({
                    "id": id,
                    "item_id": item_id,
                    "code": code,
                    "title": title,
                    "node_id": node,
                    "quantity": quantity,
                    "total_price": total_price,
                    "commission": commission,
                    "currency": "КВАЗИ",
                    "created_at": created_at,
                })
            },
        )
        .collect();

    Ok(Json(ApiResponse::success(json!({
        "citizen_id": citizen_id,
        "purchases": purchases,
    }))))
}

/// GET /api/shop/balance — same KVAZI balance as exchange.
pub async fn balance(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> ShopResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ShopError::Unauthorized(
            "Node credentials cannot query balance".into(),
        ));
    }
    let citizen_id = auth.resolve_account_id(&state.db).await;
    let amount: i64 = sqlx::query_scalar("SELECT COALESCE(amount, 0) FROM balances WHERE citizen_id = $1")
        .bind(&citizen_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| ShopError::DatabaseError(e.to_string()))?
        .unwrap_or(0);
    Ok(Json(ApiResponse::success(json!({
        "citizen_id": citizen_id,
        "amount": amount,
        "currency": "КВАЗИ",
        "qz_id": citizen_id,
    }))))
}

#[derive(Debug, Deserialize)]
pub struct StockBody {
    pub item_id: Uuid,
    /// Absolute quantity to set, OR use delta.
    pub quantity: Option<i64>,
    pub delta: Option<i64>,
}

/// POST /api/shop/warehouse/stock — mayor fills warehouse (exchange = procurement narrative).
pub async fn stock_warehouse(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<StockBody>,
) -> ShopResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ShopError::Unauthorized("Node credentials cannot stock".into()));
    }
    let citizen_id = auth.resolve_account_id(&state.db).await;
    let is_mayor = is_active_mayor(&state.db, &citizen_id)
        .await
        .map_err(|e| ShopError::DatabaseError(e.to_string()))?;
    let is_elevated = matches!(
        auth.role,
        crate::types::Role::Aiya | crate::types::Role::Founder | crate::types::Role::Agent
    );
    if !is_mayor && !is_elevated {
        return Err(ShopError::Unauthorized(
            "Только мэр узла (или Айя/Секретарь) наполняет склад магазина".into(),
        ));
    }

    let node = node_id();
    let exists: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM shop_items WHERE id = $1 AND is_active = TRUE")
            .bind(body.item_id)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| ShopError::DatabaseError(e.to_string()))?;
    if exists.is_none() {
        return Err(ShopError::NotFound("Item not found".into()));
    }

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ShopError::DatabaseError(e.to_string()))?;

    sqlx::query(
        "INSERT INTO shop_warehouse (id, node_id, item_id, quantity, updated_at)
         VALUES ($1, $2, $3, 0, $4)
         ON CONFLICT (node_id, item_id) DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(&node)
    .bind(body.item_id)
    .bind(now_ts())
    .execute(&mut *tx)
    .await
    .map_err(|e| ShopError::DatabaseError(e.to_string()))?;

    let current = lock_warehouse_qty(&mut tx, &node, body.item_id).await?;
    let new_qty = if let Some(abs) = body.quantity {
        if abs < 0 {
            return Err(ShopError::BadRequest("quantity must be >= 0".into()));
        }
        abs
    } else if let Some(delta) = body.delta {
        let n = current + delta;
        if n < 0 {
            return Err(ShopError::BadRequest("stock would go negative".into()));
        }
        n
    } else {
        return Err(ShopError::BadRequest(
            "provide quantity (absolute) or delta".into(),
        ));
    };

    sqlx::query(
        "UPDATE shop_warehouse SET quantity = $1, updated_at = $2
         WHERE node_id = $3 AND item_id = $4",
    )
    .bind(new_qty)
    .bind(now_ts())
    .bind(&node)
    .bind(body.item_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| ShopError::DatabaseError(e.to_string()))?;

    tx.commit()
        .await
        .map_err(|e| ShopError::DatabaseError(e.to_string()))?;

    let event_id = format!("shop_stock_{}_{}", node, body.item_id);
    let _ = chain_events::emit_signed_event(
        &state,
        "ShopWarehouseStocked",
        &citizen_id,
        "Shop warehouse stocked",
        &format!("node={node} item={} qty={new_qty}", body.item_id),
        Some(&event_id),
        json!({
            "node_id": node,
            "item_id": body.item_id,
            "quantity": new_qty,
            "previous": current,
            "by": citizen_id,
        }),
    )
    .await;

    Ok(Json(ApiResponse::success(json!({
        "node_id": node,
        "item_id": body.item_id,
        "quantity": new_qty,
        "previous": current,
    }))))
}

/// GET /api/shop/warehouse — stock overview for this node.
pub async fn list_warehouse(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> ShopResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ShopError::Unauthorized(
            "Node credentials cannot list warehouse".into(),
        ));
    }
    let node = node_id();
    let rows: Vec<(Uuid, String, String, String, i64, i64)> = sqlx::query_as(
        "SELECT i.id, i.code, i.title, i.category, i.price_kvazi, COALESCE(w.quantity, 0)
         FROM shop_items i
         LEFT JOIN shop_warehouse w ON w.item_id = i.id AND w.node_id = $1
         WHERE i.is_active = TRUE
         ORDER BY i.title",
    )
    .bind(&node)
    .fetch_all(&state.db)
    .await
    .map_err(|e| ShopError::DatabaseError(e.to_string()))?;

    let stock: Vec<_> = rows
        .into_iter()
        .map(|(id, code, title, category, price_kvazi, quantity)| {
            json!({
                "item_id": id,
                "code": code,
                "title": title,
                "category": category,
                "price_kvazi": price_kvazi,
                "quantity": quantity,
            })
        })
        .collect();

    Ok(Json(ApiResponse::success(json!({
        "node_id": node,
        "stock": stock,
    }))))
}
