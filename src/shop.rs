//! Node warehouse APIs for the **separate** shop app + exchange procurement.
//!
//! - Exchange (`lot_kind=item`) fills `shop_warehouse` (КВАЗИ).
//! - Shop app sells leftovers for **external/fiat** payment, then calls debit here.
//! - No KVAZI settlement in the shop contour (ЭК гл. 4).

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
use crate::offices::is_active_mayor;
use crate::response::ApiResponse;
use crate::types::Role;
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
    InsufficientStock,
    Unauthorized(String),
    BadRequest(String),
    Gone(String),
}

impl ShopError {
    fn status_code(&self) -> StatusCode {
        match self {
            ShopError::DatabaseError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ShopError::NotFound(_) => StatusCode::NOT_FOUND,
            ShopError::InsufficientStock => StatusCode::CONFLICT,
            ShopError::Unauthorized(_) => StatusCode::FORBIDDEN,
            ShopError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ShopError::Gone(_) => StatusCode::GONE,
        }
    }
}

impl std::fmt::Display for ShopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShopError::DatabaseError(s)
            | ShopError::NotFound(s)
            | ShopError::Unauthorized(s)
            | ShopError::BadRequest(s)
            | ShopError::Gone(s) => write!(f, "{s}"),
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

async fn lock_warehouse_qty(
    tx: &mut PgConnection,
    node: &str,
    item_id: Uuid,
) -> ShopResult<i64> {
    let qty: Option<i64> = sqlx::query_scalar(
        "SELECT quantity FROM shop_warehouse
         WHERE node_id = $1 AND item_id = $2 FOR UPDATE",
    )
    .bind(node)
    .bind(item_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ShopError::DatabaseError(e.to_string()))?;
    Ok(qty.unwrap_or(0))
}

pub fn this_node_id() -> String {
    node_id()
}

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
pub async fn credit_warehouse_delta(
    tx: &mut PgConnection,
    node: &str,
    item_id: Uuid,
    delta: i64,
) -> Result<(i64, i64), String> {
    if delta == 0 {
        let q = lock_warehouse_qty(tx, node, item_id)
            .await
            .map_err(|e| e.to_string())?;
        return Ok((q, q));
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
        return Err("warehouse stock would go negative".into());
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

async fn can_manage_warehouse(state: &AppState, auth: &AuthContext) -> ShopResult<bool> {
    if auth.is_node {
        return Ok(true);
    }
    if auth.can_add_balance() || auth.can_submit_need_for_citizen() {
        return Ok(true);
    }
    if matches!(auth.role, Role::Aiya | Role::Founder | Role::Agent) {
        return Ok(true);
    }
    let citizen_id = auth.resolve_account_id(&state.db).await;
    let is_mayor = is_active_mayor(&state.db, &citizen_id)
        .await
        .map_err(|e| ShopError::DatabaseError(e.to_string()))?;
    Ok(is_mayor)
}

fn item_json(
    id: Uuid,
    code: String,
    title: String,
    category: String,
    unit: String,
    description: String,
    catalog_ref_kvazi: i64,
    quantity: i64,
    node: &str,
) -> serde_json::Value {
    json!({
        "id": id,
        "code": code,
        "title": title,
        "category": category,
        "unit": unit,
        "description": description,
        // Catalog reference only — shop payment is external/fiat (not KVAZI).
        "catalog_ref_kvazi": catalog_ref_kvazi,
        "price_kvazi": catalog_ref_kvazi,
        "payment": "external",
        "quantity": quantity,
        "node_id": node,
    })
}

/// GET /api/shop/items — warehouse catalog for this node (shop-app / citizens).
#[derive(Debug, Deserialize)]
pub struct ListItemsQuery {
    pub all: Option<bool>,
}

pub async fn list_items(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    Query(q): Query<ListItemsQuery>,
) -> ShopResult<Json<ApiResponse>> {
    let node = node_id();
    let include_zero = q.all.unwrap_or(false);
    let rows: Vec<(Uuid, String, String, String, String, String, i64, i64)> = if include_zero {
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
        .map(|(id, code, title, category, unit, description, price, quantity)| {
            item_json(
                id, code, title, category, unit, description, price, quantity, &node,
            )
        })
        .collect();

    Ok(Json(ApiResponse::success(json!({
        "node_id": node,
        "svod": "items",
        "payment": "external",
        "items": items,
    }))))
}

/// GET /api/shop/items/:id
pub async fn get_item(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> ShopResult<Json<ApiResponse>> {
    let node = node_id();
    let row: Option<(Uuid, String, String, String, String, String, i64, i64)> = sqlx::query_as(
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

    let Some((id, code, title, category, unit, description, price, quantity)) = row else {
        return Err(ShopError::NotFound("Item not found".into()));
    };

    Ok(Json(ApiResponse::success(item_json(
        id, code, title, category, unit, description, price, quantity, &node,
    ))))
}

/// POST /api/shop/buy — removed (shop has no KVAZI).
pub async fn buy_item() -> ShopResult<Json<ApiResponse>> {
    Err(ShopError::Gone(
        "Магазин не принимает КВАЗИ. Оплата — во внешнем shop-приложении; \
         утилизация склада: POST /api/shop/warehouse/debit"
            .into(),
    ))
}

/// GET /api/shop/balance — removed from shop contour.
pub async fn balance() -> ShopResult<Json<ApiResponse>> {
    Err(ShopError::Gone(
        "Баланс КВАЗИ — только биржа: GET /exchange/balance (или аналог). У магазина нет КВАЗИ."
            .into(),
    ))
}

#[derive(Debug, Deserialize)]
pub struct DebitBody {
    pub item_id: Uuid,
    pub quantity: u64,
    /// Optional republic citizen (if buyer is a citizen).
    pub citizen_id: Option<String>,
    /// External buyer id / email / order ref when not a citizen.
    pub buyer_external: Option<String>,
    /// Idempotency / payment provider reference.
    pub external_payment_id: Option<String>,
    /// Fiat amount in minor units (cents) or shop-app units; 0 if unknown.
    pub total_price: Option<i64>,
    pub currency: Option<String>,
}

/// POST /api/shop/warehouse/debit — utilize stock after external shop payment.
/// Auth: node secret (shop-app), mayor of node, Aiya/Founder/Secretary.
pub async fn debit_warehouse(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<DebitBody>,
) -> ShopResult<Json<ApiResponse>> {
    if !can_manage_warehouse(&state, &auth).await? {
        return Err(ShopError::Unauthorized(
            "Утилизация склада: мэр узла, Айя/Секретарь или credentials магазина (node secret)"
                .into(),
        ));
    }
    if body.quantity == 0 {
        return Err(ShopError::BadRequest("quantity must be > 0".into()));
    }

    let citizen_id = body
        .citizen_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let buyer_external = body
        .buyer_external
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if citizen_id.is_none() && buyer_external.is_none() {
        return Err(ShopError::BadRequest(
            "укажите citizen_id или buyer_external".into(),
        ));
    }

    if let Some(ref cid) = citizen_id {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM citizens WHERE id = $1)")
            .bind(cid)
            .fetch_one(&state.db)
            .await
            .map_err(|e| ShopError::DatabaseError(e.to_string()))?;
        if !exists {
            return Err(ShopError::NotFound("citizen_id not found".into()));
        }
    }

    let currency = body
        .currency
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("EXTERNAL")
        .to_string();
    if currency.eq_ignore_ascii_case("КВАЗИ") || currency.eq_ignore_ascii_case("KVAZI") {
        return Err(ShopError::BadRequest(
            "Магазин не принимает КВАЗИ — укажите EXTERNAL / фиатную валюту".into(),
        ));
    }

    let node = node_id();
    let item: Option<(String, String)> = sqlx::query_as(
        "SELECT code, title FROM shop_items WHERE id = $1 AND is_active = TRUE",
    )
    .bind(body.item_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| ShopError::DatabaseError(e.to_string()))?;
    let Some((code, title)) = item else {
        return Err(ShopError::NotFound("Item not found".into()));
    };

    if let Some(ref pay_id) = body.external_payment_id {
        let dup: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM shop_purchases WHERE external_payment_id = $1 LIMIT 1",
        )
        .bind(pay_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| ShopError::DatabaseError(e.to_string()))?;
        if let Some(existing) = dup {
            return Ok(Json(ApiResponse::success(json!({
                "purchase_id": existing,
                "idempotent": true,
                "node_id": node,
                "item_id": body.item_id,
                "code": code,
            }))));
        }
    }

    let qty = body.quantity as i64;
    let total = body.total_price.unwrap_or(0).max(0);

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ShopError::DatabaseError(e.to_string()))?;

    let stock = lock_warehouse_qty(&mut tx, &node, body.item_id).await?;
    if stock < qty {
        return Err(ShopError::InsufficientStock);
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
          mayor_citizen_id, currency, buyer_external, external_payment_id, created_at)
         VALUES ($1,$2,$3,$4,$5,$6,0,0,NULL,$7,$8,$9,$10)",
    )
    .bind(purchase_id)
    .bind(citizen_id.as_deref())
    .bind(body.item_id)
    .bind(&node)
    .bind(qty)
    .bind(total)
    .bind(&currency)
    .bind(buyer_external.as_deref())
    .bind(body.external_payment_id.as_deref())
    .bind(now_ts())
    .execute(&mut *tx)
    .await
    .map_err(|e| ShopError::DatabaseError(e.to_string()))?;

    tx.commit()
        .await
        .map_err(|e| ShopError::DatabaseError(e.to_string()))?;

    let actor = if auth.is_node {
        format!("shop@{}", node)
    } else {
        auth.resolve_account_id(&state.db).await
    };

    let event_id = format!("shop_purchase_{purchase_id}");
    let _ = chain_events::emit_signed_event(
        &state,
        "ShopPurchase",
        &actor,
        &format!("Shop utilize: {title}"),
        &format!("{code} × {qty} on {node} ({currency})"),
        Some(&event_id),
        json!({
            "purchase_id": purchase_id,
            "citizen_id": citizen_id,
            "buyer_external": buyer_external,
            "external_payment_id": body.external_payment_id,
            "item_id": body.item_id,
            "code": code,
            "node_id": node,
            "quantity": qty,
            "total_price": total,
            "currency": currency,
            "utilized": true,
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
        "currency": currency,
        "citizen_id": citizen_id,
        "buyer_external": buyer_external,
        "external_payment_id": body.external_payment_id,
        "node_id": node,
        "remaining_stock": stock - qty,
        "payment": "external",
    }))))
}

/// GET /api/shop/history — purchases involving this citizen (external shop utilizations).
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
            "Node credentials cannot query personal history".into(),
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
        String,
        chrono::DateTime<Utc>,
    )> = sqlx::query_as(
        "SELECT p.id, p.item_id, i.code, i.title, p.node_id, p.quantity,
                p.total_price, p.currency, p.created_at
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
            |(id, item_id, code, title, node, quantity, total_price, currency, created_at)| {
                json!({
                    "id": id,
                    "item_id": item_id,
                    "code": code,
                    "title": title,
                    "node_id": node,
                    "quantity": quantity,
                    "total_price": total_price,
                    "currency": currency,
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

#[derive(Debug, Deserialize)]
pub struct StockBody {
    pub item_id: Uuid,
    pub quantity: Option<i64>,
    pub delta: Option<i64>,
}

/// POST /api/shop/warehouse/stock — mayor / elevated manual fill (prefer exchange procurement).
pub async fn stock_warehouse(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<StockBody>,
) -> ShopResult<Json<ApiResponse>> {
    if !can_manage_warehouse(&state, &auth).await? {
        return Err(ShopError::Unauthorized(
            "Только мэр узла (или Айя/Секретарь/shop credentials) наполняет склад".into(),
        ));
    }

    let actor = if auth.is_node {
        format!("shop@{}", node_id())
    } else {
        auth.resolve_account_id(&state.db).await
    };

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
        &actor,
        "Shop warehouse stocked",
        &format!("node={node} item={} qty={new_qty}", body.item_id),
        Some(&event_id),
        json!({
            "node_id": node,
            "item_id": body.item_id,
            "quantity": new_qty,
            "previous": current,
            "by": actor,
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
    Extension(_auth): Extension<AuthContext>,
) -> ShopResult<Json<ApiResponse>> {
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
        .map(|(id, code, title, category, catalog_ref_kvazi, quantity)| {
            json!({
                "item_id": id,
                "code": code,
                "title": title,
                "category": category,
                "catalog_ref_kvazi": catalog_ref_kvazi,
                "price_kvazi": catalog_ref_kvazi,
                "quantity": quantity,
                "payment": "external",
            })
        })
        .collect();

    Ok(Json(ApiResponse::success(json!({
        "node_id": node,
        "stock": stock,
        "payment": "external",
    }))))
}
