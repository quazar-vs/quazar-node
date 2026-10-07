//! Canonical republican exchange market (A → lot → B → settle A).
//!
//! - Citizen inventory = items only (`citizen_item_inventory`).
//! - Republic warehouse ≠ shop_warehouse (mayor/local).
//! - ±10% vs item `price_kvazi` (Свод предметов).
//! - Action (`svod_code`) = how citizen interacts; optional metadata on the deal.

use axum::{
    extract::{Extension, State},
    response::Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use sqlx::{PgPool, Postgres, Transaction};
use std::sync::Arc;
use uuid::Uuid;

use crate::auth::AuthContext;
use crate::exchange::{
    buyer_unit_price, default_shelf_life_secs, emit_event, ensure_active_citizen, get_balance,
    lock_balance, seller_unit_price, set_balance, ExchangeError, ExchangeResult, STATE_RESERVE_ID,
};
use crate::response::ApiResponse;
use crate::AppState;

fn now() -> i64 {
    Utc::now().timestamp()
}

#[derive(Debug, Deserialize)]
pub struct OfferItemBody {
    /// Subject from ITEMS.md / shop_items.
    pub item_code: String,
    pub quantity: u64,
    /// Action from SVOD.md (how the citizen interacts); optional.
    pub svod_code: Option<String>,
    pub shelf_life_secs: Option<i64>,
}

#[derive(Debug, Clone)]
struct ItemRow {
    code: String,
    title: String,
    price_kvazi: i64,
}

async fn load_item(pool: &PgPool, code: &str) -> ExchangeResult<ItemRow> {
    let row: Option<(String, String, i64)> = sqlx::query_as(
        "SELECT code, title, price_kvazi FROM shop_items
         WHERE code = $1 AND is_active = TRUE",
    )
    .bind(code)
    .fetch_optional(pool)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    let Some((code, title, price)) = row else {
        return Err(ExchangeError::BadRequest(format!(
            "Неизвестный предмет свода: {code}"
        )));
    };
    if price <= 0 {
        return Err(ExchangeError::BadRequest(
            "У предмета нет положительной справочной цены (price_kvazi)".into(),
        ));
    }
    Ok(ItemRow {
        code,
        title,
        price_kvazi: price,
    })
}

pub async fn lock_citizen_item(
    tx: &mut Transaction<'_, Postgres>,
    citizen_id: &str,
    item_code: &str,
) -> ExchangeResult<i64> {
    sqlx::query(
        "INSERT INTO citizen_item_inventory (citizen_id, item_code, quantity, updated_at)
         VALUES ($1, $2, 0, $3)
         ON CONFLICT (citizen_id, item_code) DO NOTHING",
    )
    .bind(citizen_id)
    .bind(item_code)
    .bind(now())
    .execute(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    sqlx::query_scalar(
        "SELECT quantity FROM citizen_item_inventory
         WHERE citizen_id = $1 AND item_code = $2 FOR UPDATE",
    )
    .bind(citizen_id)
    .bind(item_code)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))
}

pub async fn set_citizen_item(
    tx: &mut Transaction<'_, Postgres>,
    citizen_id: &str,
    item_code: &str,
    qty: i64,
) -> ExchangeResult<()> {
    if qty < 0 {
        return Err(ExchangeError::BadRequest("quantity cannot be negative".into()));
    }
    sqlx::query(
        "UPDATE citizen_item_inventory SET quantity = $1, updated_at = $2
         WHERE citizen_id = $3 AND item_code = $4",
    )
    .bind(qty)
    .bind(now())
    .bind(citizen_id)
    .bind(item_code)
    .execute(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    Ok(())
}

async fn lock_republic_wh(
    tx: &mut Transaction<'_, Postgres>,
    item_code: &str,
) -> ExchangeResult<i64> {
    sqlx::query(
        "INSERT INTO republic_exchange_warehouse (item_code, quantity, updated_at)
         VALUES ($1, 0, $2)
         ON CONFLICT (item_code) DO NOTHING",
    )
    .bind(item_code)
    .bind(now())
    .execute(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    sqlx::query_scalar(
        "SELECT quantity FROM republic_exchange_warehouse WHERE item_code = $1 FOR UPDATE",
    )
    .bind(item_code)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))
}

async fn set_republic_wh(
    tx: &mut Transaction<'_, Postgres>,
    item_code: &str,
    qty: i64,
) -> ExchangeResult<()> {
    if qty < 0 {
        return Err(ExchangeError::BadRequest(
            "republic warehouse quantity cannot be negative".into(),
        ));
    }
    sqlx::query(
        "UPDATE republic_exchange_warehouse SET quantity = $1, updated_at = $2
         WHERE item_code = $3",
    )
    .bind(qty)
    .bind(now())
    .bind(item_code)
    .execute(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    Ok(())
}

/// POST /exchange/sell-to-state — offer item to republic (deal A pending SB).
pub async fn offer_item_to_republic(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<OfferItemBody>,
) -> ExchangeResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ExchangeError::Unauthorized("Node cannot sell".into()));
    }
    if !auth.can_open_state_deal() {
        return Err(ExchangeError::Unauthorized(
            "Продажа республике доступна гражданам и выше".into(),
        ));
    }
    if body.quantity == 0 {
        return Err(ExchangeError::BadRequest("quantity must be > 0".into()));
    }

    let seller_id = auth.resolve_account_id(&state.db).await;
    ensure_active_citizen(&state.db, &seller_id).await?;

    let item_code = body.item_code.trim().to_string();
    let item = load_item(&state.db, &item_code).await?;
    let qty = body.quantity as i64;
    let unit_svod = item.price_kvazi;
    let unit_seller = seller_unit_price(unit_svod);
    let unit_buyer = buyer_unit_price(unit_svod);
    let payout = unit_seller.saturating_mul(qty);
    let action = body
        .svod_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // Optional: validate action exists if provided.
    if let Some(ref code) = action {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM service_catalog WHERE code = $1 AND is_active)",
        )
        .bind(code)
        .fetch_one(&state.db)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
        if !ok {
            return Err(ExchangeError::BadRequest(format!(
                "Неизвестное действие свода: {code}"
            )));
        }
    }

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let inv = lock_citizen_item(&mut tx, &seller_id, &item.code).await?;
    if inv < qty {
        return Err(ExchangeError::InsufficientInventory);
    }
    // Do not move inventory yet — wait for SB on offer_a.

    let request_id = format!("exreq_{}", Uuid::new_v4());
    let deal_id = format!("exdeal_{}", Uuid::new_v4());
    let task_id = format!("extask_{}", Uuid::new_v4());
    let ts = now();
    let svod_for_row = action.clone().unwrap_or_else(|| item.code.clone());

    sqlx::query(
        "INSERT INTO exchange_requests
         (id, citizen_id, side, svod_code, qty_total, qty_remaining, unit_svod, status,
          created_at, updated_at, lot_kind, item_code)
         VALUES ($1,$2,'sell',$3,$4,$4,$5,'pending_sb',$6,$6,'item',$7)",
    )
    .bind(&request_id)
    .bind(&seller_id)
    .bind(&svod_for_row)
    .bind(qty)
    .bind(unit_svod)
    .bind(ts)
    .bind(&item.code)
    .execute(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    // Placeholder listing id until SB accepts (required NOT NULL? check)
    // listing_id on deals may be required — create a pending listing shell or use request id.
    let listing_placeholder = format!("exlist_pending_{}", Uuid::new_v4());

    sqlx::query(
        "INSERT INTO exchange_listings
         (id, request_id, seller_id, svod_code, qty_total, qty_remaining,
          unit_svod, unit_buyer_price, unit_seller_price, status, created_at, updated_at,
          is_state_agent, lot_scope, lot_kind, item_code, offer_deal_id)
         VALUES ($1,$2,$3,$4,$5,0,$6,$7,$8,'pending_sb',$9,$9,TRUE,'global','item',$10,$11)",
    )
    .bind(&listing_placeholder)
    .bind(&request_id)
    .bind(&seller_id)
    .bind(&svod_for_row)
    .bind(qty)
    .bind(unit_svod)
    .bind(unit_buyer)
    .bind(unit_seller)
    .bind(ts)
    .bind(&item.code)
    .bind(&deal_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    sqlx::query(
        "INSERT INTO exchange_deals
         (id, listing_id, request_id, buyer_id, seller_id, svod_code, qty,
          unit_svod, unit_buyer_price, unit_seller_price, escrow_amount, status,
          security_task_id, created_at, updated_at, lot_scope, confirm_mode,
          lot_kind, item_code, deal_role)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,'pending_intake',$12,$13,$13,'global',
                 'live_guardian','item',$14,'offer_a')",
    )
    .bind(&deal_id)
    .bind(&listing_placeholder)
    .bind(&request_id)
    .bind(STATE_RESERVE_ID)
    .bind(&seller_id)
    .bind(&svod_for_row)
    .bind(qty)
    .bind(unit_svod)
    .bind(unit_buyer)
    .bind(unit_seller)
    .bind(payout)
    .bind(&task_id)
    .bind(ts)
    .bind(&item.code)
    .execute(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    sqlx::query(
        "INSERT INTO exchange_security_tasks
         (id, deal_id, from_citizen, to_citizen, svod_code, qty, status, created_at, updated_at,
          confirm_mode)
         VALUES ($1,$2,$3,$4,$5,$6,'open',$7,$7,'live_guardian')",
    )
    .bind(&task_id)
    .bind(&deal_id)
    .bind(&seller_id)
    .bind(STATE_RESERVE_ID)
    .bind(&svod_for_row)
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
        "ExchangeOfferOpened",
        &auth.citizen_name,
        "Citizen offered item to republic (awaiting SB)",
        format!(
            "offer {qty} x {} ({}) @ seller {unit_seller}",
            item.code, item.title
        ),
        &format!("evt_offer_{deal_id}"),
        json!({
            "deal_id": deal_id,
            "security_task_id": task_id,
            "request_id": request_id,
            "item_code": item.code,
            "item_title": item.title,
            "svod_code": action,
            "quantity": qty,
            "unit_svod": unit_svod,
            "unit_seller_price": unit_seller,
            "unit_buyer_price": unit_buyer,
            "payout": payout,
            "deal_role": "offer_a",
            "pending_sb": true,
        }),
    )
    .await?;

    Ok(Json(ApiResponse::success(json!({
        "deal_id": deal_id,
        "security_task_id": task_id,
        "request_id": request_id,
        "status": "pending_intake",
        "deal_role": "offer_a",
        "item_code": item.code,
        "item_title": item.title,
        "svod_code": action,
        "quantity": qty,
        "unit_svod": unit_svod,
        "unit_seller_price": unit_seller,
        "unit_buyer_price": unit_buyer,
        "payout": payout,
        "pending_sb": true,
        "note": "После СБ предмет → склад республиканской биржи, появится лот (+10%). Выплата продавцу (−10%) — после покупки лота другим гражданином и СБ.",
    }))))
}

/// SB confirms offer_a: move item to republic warehouse, activate lot.
pub async fn confirm_offer_a(
    _state: &Arc<AppState>,
    auth: &AuthContext,
    tx: &mut Transaction<'_, Postgres>,
    task_id: &str,
    deal_id: &str,
) -> ExchangeResult<serde_json::Value> {
    let deal: Option<(
        String,
        String,
        String,
        String,
        i64,
        i64,
        i64,
        i64,
        String,
        Option<String>,
        String,
    )> = sqlx::query_as(
        "SELECT id, listing_id, request_id, seller_id, qty, unit_svod,
                unit_buyer_price, unit_seller_price, status, item_code, svod_code
         FROM exchange_deals WHERE id = $1 FOR UPDATE",
    )
    .bind(deal_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let Some((
        did,
        listing_id,
        request_id,
        seller_id,
        qty,
        unit_svod,
        unit_buyer,
        unit_seller,
        dstatus,
        item_code_opt,
        svod_code,
    )) = deal
    else {
        return Err(ExchangeError::NotFound("deal not found".into()));
    };
    if dstatus != "pending_intake" {
        return Err(ExchangeError::BadRequest(format!(
            "offer_a status is {dstatus}, expected pending_intake"
        )));
    }
    let item_code = item_code_opt.ok_or_else(|| {
        ExchangeError::BadRequest("offer_a missing item_code".into())
    })?;

    let inv = lock_citizen_item(tx, &seller_id, &item_code).await?;
    if inv < qty {
        return Err(ExchangeError::InsufficientInventory);
    }
    set_citizen_item(tx, &seller_id, &item_code, inv - qty).await?;

    let wh = lock_republic_wh(tx, &item_code).await?;
    set_republic_wh(tx, &item_code, wh + qty).await?;

    let ts = now();
    let shelf = default_shelf_life_secs();
    let expires_at = ts.saturating_add(shelf);

    // Activate listing as public lot (republic sells +10%).
    sqlx::query(
        "UPDATE exchange_listings SET
            status = 'active',
            qty_remaining = $1,
            expires_at = $2,
            updated_at = $3,
            is_state_agent = TRUE,
            offer_deal_id = $4
         WHERE id = $5",
    )
    .bind(qty)
    .bind(expires_at)
    .bind(ts)
    .bind(&did)
    .bind(&listing_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    sqlx::query(
        "UPDATE exchange_requests SET status = 'accepted', listing_id = $1, updated_at = $2 WHERE id = $3",
    )
    .bind(&listing_id)
    .bind(ts)
    .bind(&request_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    sqlx::query(
        "UPDATE exchange_deals SET status = 'awaiting_buyer', updated_at = $1 WHERE id = $2",
    )
    .bind(ts)
    .bind(&did)
    .execute(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    sqlx::query(
        "UPDATE exchange_security_tasks SET status = 'confirmed', updated_at = $1,
         confirmed_by = COALESCE(confirmed_by, $2) WHERE id = $3",
    )
    .bind(ts)
    .bind(&auth.citizen_name)
    .bind(task_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    Ok(json!({
        "deal_id": did,
        "listing_id": listing_id,
        "deal_role": "offer_a",
        "status": "awaiting_buyer",
        "item_code": item_code,
        "svod_code": svod_code,
        "quantity": qty,
        "unit_svod": unit_svod,
        "unit_buyer_price": unit_buyer,
        "unit_seller_price": unit_seller,
        "lot_active": true,
        "note": "Предмет на складе республиканской биржи; лот виден покупателям.",
    }))
}

/// Citizen buys republic lot backed by offer_a → deal B pending SB.
pub async fn buy_republic_lot(
    state: &Arc<AppState>,
    auth: &AuthContext,
    listing_id: &str,
    qty: i64,
) -> ExchangeResult<Json<ApiResponse>> {
    if !auth.can_open_state_deal() {
        return Err(ExchangeError::Unauthorized(
            "Покупка на бирже доступна гражданам и выше".into(),
        ));
    }
    let buyer_id = auth.resolve_account_id(&state.db).await;
    ensure_active_citizen(&state.db, &buyer_id).await?;

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let listing: Option<(
        String,
        String,
        String,
        i64,
        i64,
        i64,
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT id, request_id, seller_id, qty_remaining, unit_svod,
                unit_buyer_price, unit_seller_price, status, item_code, offer_deal_id, svod_code
         FROM exchange_listings WHERE id = $1 FOR UPDATE",
    )
    .bind(listing_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let Some((
        lid,
        request_id,
        original_seller,
        qty_remaining,
        unit_svod,
        unit_buyer,
        _unit_seller,
        status,
        item_code_opt,
        offer_deal_id,
        svod_code_opt,
    )) = listing
    else {
        return Err(ExchangeError::NotFound("listing not found".into()));
    };
    if status != "active" {
        return Err(ExchangeError::BadRequest(format!(
            "listing status is {status}"
        )));
    }
    let offer_deal_id = offer_deal_id.ok_or_else(|| {
        ExchangeError::BadRequest("listing is not a republic market lot".into())
    })?;
    let item_code = item_code_opt.ok_or_else(|| {
        ExchangeError::BadRequest("listing missing item_code".into())
    })?;
    if qty > qty_remaining {
        return Err(ExchangeError::BadRequest("quantity exceeds remaining".into()));
    }
    if original_seller == buyer_id {
        return Err(ExchangeError::BadRequest(
            "нельзя купить свой предложенный лот".into(),
        ));
    }

    // Ensure offer_a still awaiting.
    let offer_status: Option<String> =
        sqlx::query_scalar("SELECT status FROM exchange_deals WHERE id = $1 FOR UPDATE")
            .bind(&offer_deal_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    if offer_status.as_deref() != Some("awaiting_buyer") {
        return Err(ExchangeError::BadRequest(format!(
            "offer deal not awaiting buyer (status={:?})",
            offer_status
        )));
    }

    // Seller price for deal B row comes from listing (payout to original seller on settle).
    let unit_seller = seller_unit_price(unit_svod);
    let debit = unit_buyer.saturating_mul(qty);
    let buyer_bal = lock_balance(&mut tx, &buyer_id).await?;
    if buyer_bal < debit {
        return Err(ExchangeError::InsufficientBalance);
    }
    let reserve = lock_balance(&mut tx, STATE_RESERVE_ID).await?;
    set_balance(&mut tx, &buyer_id, buyer_bal - debit).await?;
    set_balance(&mut tx, STATE_RESERVE_ID, reserve + debit).await?;

    let new_rem = qty_remaining - qty;
    let list_status = if new_rem == 0 { "filled" } else { "active" };
    let ts = now();
    sqlx::query(
        "UPDATE exchange_listings SET qty_remaining = $1, status = $2, updated_at = $3 WHERE id = $4",
    )
    .bind(new_rem)
    .bind(list_status)
    .bind(ts)
    .bind(&lid)
    .execute(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let deal_b = format!("exdeal_{}", Uuid::new_v4());
    let task_id = format!("extask_{}", Uuid::new_v4());
    let svod_code = svod_code_opt.unwrap_or_else(|| item_code.clone());

    sqlx::query(
        "INSERT INTO exchange_deals
         (id, listing_id, request_id, buyer_id, seller_id, svod_code, qty,
          unit_svod, unit_buyer_price, unit_seller_price, escrow_amount, status,
          security_task_id, created_at, updated_at, lot_scope, confirm_mode,
          lot_kind, item_code, deal_role, paired_deal_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,'escrowed',$12,$13,$13,'global',
                 'live_guardian','item',$14,'buy_b',$15)",
    )
    .bind(&deal_b)
    .bind(&lid)
    .bind(&request_id)
    .bind(&buyer_id)
    .bind(STATE_RESERVE_ID) // republic sells from warehouse
    .bind(&svod_code)
    .bind(qty)
    .bind(unit_svod)
    .bind(unit_buyer)
    .bind(unit_seller)
    .bind(debit)
    .bind(&task_id)
    .bind(ts)
    .bind(&item_code)
    .bind(&offer_deal_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    sqlx::query(
        "INSERT INTO exchange_security_tasks
         (id, deal_id, from_citizen, to_citizen, svod_code, qty, status, created_at, updated_at,
          confirm_mode)
         VALUES ($1,$2,$3,$4,$5,$6,'open',$7,$7,'live_guardian')",
    )
    .bind(&task_id)
    .bind(&deal_b)
    .bind(STATE_RESERVE_ID)
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
        state,
        "ExchangeBuyOpened",
        &auth.citizen_name,
        "Citizen buy republic lot (awaiting SB)",
        format!("buy {qty} x {item_code} @ {unit_buyer}"),
        &format!("evt_buy_{deal_b}"),
        json!({
            "deal_id": deal_b,
            "paired_deal_id": offer_deal_id,
            "listing_id": lid,
            "security_task_id": task_id,
            "item_code": item_code,
            "quantity": qty,
            "unit_buyer_price": unit_buyer,
            "escrow_amount": debit,
            "deal_role": "buy_b",
            "pending_sb": true,
        }),
    )
    .await?;

    Ok(Json(ApiResponse::success(json!({
        "deal_id": deal_b,
        "paired_deal_id": offer_deal_id,
        "listing_id": lid,
        "security_task_id": task_id,
        "status": "escrowed",
        "deal_role": "buy_b",
        "item_code": item_code,
        "quantity": qty,
        "unit_buyer_price": unit_buyer,
        "escrow_amount": debit,
        "pending_sb": true,
        "note": "После СБ предмет со склада республики → ваш инвентарь; продавцу выплатят −10%.",
    }))))
}

/// SB confirms buy_b: item → buyer; pay offer_a seller; close both.
pub async fn confirm_buy_b(
    state: &Arc<AppState>,
    auth: &AuthContext,
    tx: &mut Transaction<'_, Postgres>,
    task_id: &str,
    deal_id: &str,
) -> ExchangeResult<serde_json::Value> {
    let deal: Option<(
        String,
        String,
        String,
        String,
        i64,
        i64,
        i64,
        String,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT id, listing_id, buyer_id, seller_id, qty, unit_seller_price, escrow_amount,
                status, item_code, paired_deal_id
         FROM exchange_deals WHERE id = $1 FOR UPDATE",
    )
    .bind(deal_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let Some((
        did,
        listing_id,
        buyer_id,
        _seller_state,
        qty,
        _unit_seller,
        escrow,
        dstatus,
        item_code_opt,
        paired_opt,
    )) = deal
    else {
        return Err(ExchangeError::NotFound("deal not found".into()));
    };
    if dstatus != "escrowed" && dstatus != "in_delivery" {
        return Err(ExchangeError::BadRequest(format!(
            "buy_b status is {dstatus}"
        )));
    }
    let item_code = item_code_opt.ok_or_else(|| {
        ExchangeError::BadRequest("buy_b missing item_code".into())
    })?;
    let offer_deal_id = paired_opt.ok_or_else(|| {
        ExchangeError::BadRequest("buy_b missing paired offer_a".into())
    })?;

    // Warehouse → buyer inventory
    let wh = lock_republic_wh(tx, &item_code).await?;
    if wh < qty {
        return Err(ExchangeError::BadRequest(
            "Недостаточно предметов на складе республиканской биржи".into(),
        ));
    }
    set_republic_wh(tx, &item_code, wh - qty).await?;
    let bin = lock_citizen_item(tx, &buyer_id, &item_code).await?;
    set_citizen_item(tx, &buyer_id, &item_code, bin + qty).await?;

    let ts = now();
    sqlx::query(
        "UPDATE exchange_security_tasks SET status = 'confirmed', updated_at = $1,
         confirmed_by = COALESCE(confirmed_by, $2) WHERE id = $3",
    )
    .bind(ts)
    .bind(&auth.citizen_name)
    .bind(task_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    sqlx::query("UPDATE exchange_deals SET status = 'completed', updated_at = $1 WHERE id = $2")
        .bind(ts)
        .bind(&did)
        .execute(&mut **tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    // Close offer_a: pay original seller (−10% already in unit_seller of offer / listing).
    let offer: Option<(String, String, i64, i64, String)> = sqlx::query_as(
        "SELECT id, seller_id, qty, unit_seller_price, status
         FROM exchange_deals WHERE id = $1 FOR UPDATE",
    )
    .bind(&offer_deal_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let Some((oid, seller_id, oqty, o_unit_seller, ostatus)) = offer else {
        return Err(ExchangeError::NotFound("paired offer_a not found".into()));
    };
    if ostatus != "awaiting_buyer" {
        return Err(ExchangeError::BadRequest(format!(
            "offer_a status is {ostatus}, expected awaiting_buyer"
        )));
    }

    // Pay for the purchased qty (partial buys: pay proportionally).
    let pay = o_unit_seller.saturating_mul(qty.min(oqty));
    let reserve = lock_balance(tx, STATE_RESERVE_ID).await?;
    if reserve < pay {
        return Err(ExchangeError::BadRequest(
            "Недостаточный резерв республики для выплаты продавцу".into(),
        ));
    }
    let seller_bal = lock_balance(tx, &seller_id).await?;
    set_balance(tx, STATE_RESERVE_ID, reserve - pay).await?;
    set_balance(tx, &seller_id, seller_bal + pay).await?;

    // If full offer qty bought (listing filled / remaining 0), complete offer_a;
    // for partial, keep awaiting if listing still has remainder linked to same offer.
    let rem: i64 = sqlx::query_scalar(
        "SELECT COALESCE(qty_remaining, 0) FROM exchange_listings WHERE id = $1",
    )
    .bind(&listing_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?
    .unwrap_or(0);

    let offer_done = rem == 0;
    if offer_done {
        sqlx::query(
            "UPDATE exchange_deals SET status = 'completed', updated_at = $1 WHERE id = $2",
        )
        .bind(ts)
        .bind(&oid)
        .execute(&mut **tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    }

    let _ = state;
    Ok(json!({
        "deal_id": did,
        "deal_role": "buy_b",
        "status": "completed",
        "item_code": item_code,
        "quantity": qty,
        "buyer_id": buyer_id,
        "escrow_amount": escrow,
        "offer_deal_id": oid,
        "seller_id": seller_id,
        "seller_payout": pay,
        "offer_status": if offer_done { "completed" } else { "awaiting_buyer" },
        "spread_stays_in_reserve": true,
    }))
}

/// GET /exchange/inventory — citizen item inventory (ITEMS).
pub async fn list_item_inventory(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> ExchangeResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ExchangeError::Unauthorized("Node cannot read inventory".into()));
    }
    let citizen_id = auth.resolve_account_id(&state.db).await;
    let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT i.item_code, COALESCE(s.title, i.item_code), i.quantity, i.updated_at
         FROM citizen_item_inventory i
         LEFT JOIN shop_items s ON s.code = i.item_code
         WHERE i.citizen_id = $1 AND i.quantity > 0
         ORDER BY i.item_code",
    )
    .bind(&citizen_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let items: Vec<_> = rows
        .into_iter()
        .map(|(code, title, qty, updated)| {
            json!({
                "item_code": code,
                "title": title,
                "quantity": qty,
                "updated_at": updated,
            })
        })
        .collect();

    let bal = get_balance(&state.db, &citizen_id).await.unwrap_or(0);
    Ok(Json(ApiResponse::success(json!({
        "citizen_id": citizen_id,
        "balance_kvazi": bal,
        "inventory": items,
        "note": "Инвентарь — только предметы Свода предметов.",
    }))))
}

/// GET /exchange/republic-warehouse — SB/Aiya view of republican exchange stock.
pub async fn list_republic_warehouse(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> ExchangeResult<Json<ApiResponse>> {
    if !auth.can_exchange_security() && !auth.is_admin() {
        return Err(ExchangeError::Unauthorized(
            "Склад республиканской биржи: Страж+ / админ".into(),
        ));
    }
    let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT w.item_code, COALESCE(s.title, w.item_code), w.quantity, w.updated_at
         FROM republic_exchange_warehouse w
         LEFT JOIN shop_items s ON s.code = w.item_code
         WHERE w.quantity > 0
         ORDER BY w.item_code",
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let stock: Vec<_> = rows
        .into_iter()
        .map(|(code, title, qty, updated)| {
            json!({
                "item_code": code,
                "title": title,
                "quantity": qty,
                "updated_at": updated,
            })
        })
        .collect();
    Ok(Json(ApiResponse::success(json!({ "warehouse": stock }))))
}

#[derive(Debug, Deserialize)]
pub struct AdjustItemInventoryBody {
    pub citizen_id: String,
    pub item_code: String,
    pub quantity: i64,
}

/// POST /exchange/inventory/items — Aiya/SB seed citizen item inventory.
pub async fn adjust_item_inventory(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<AdjustItemInventoryBody>,
) -> ExchangeResult<Json<ApiResponse>> {
    if !auth.can_exchange_security() && !auth.is_admin() {
        return Err(ExchangeError::Unauthorized(
            "Недостаточно прав для изменения инвентаря предметов".into(),
        ));
    }
    if body.quantity < 0 {
        return Err(ExchangeError::BadRequest("quantity must be >= 0".into()));
    }
    let _ = load_item(&state.db, body.item_code.trim()).await?;
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    let _ = lock_citizen_item(&mut tx, &body.citizen_id, body.item_code.trim()).await?;
    set_citizen_item(
        &mut tx,
        &body.citizen_id,
        body.item_code.trim(),
        body.quantity,
    )
    .await?;
    tx.commit()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    Ok(Json(ApiResponse::success(json!({
        "citizen_id": body.citizen_id,
        "item_code": body.item_code,
        "quantity": body.quantity,
    }))))
}

/// Lookup deal_role for confirm_delivery branching.
pub async fn deal_role_of(
    tx: &mut Transaction<'_, Postgres>,
    deal_id: &str,
) -> ExchangeResult<Option<String>> {
    sqlx::query_scalar("SELECT deal_role FROM exchange_deals WHERE id = $1")
        .bind(deal_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))
}

pub async fn listing_has_offer_deal(pool: &PgPool, listing_id: &str) -> ExchangeResult<bool> {
    let v: Option<String> = sqlx::query_scalar(
        "SELECT offer_deal_id FROM exchange_listings WHERE id = $1 AND status = 'active'",
    )
    .bind(listing_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?
    .flatten();
    Ok(v.is_some())
}

// --- Эмиссия КВАЗИ (Агент Биржи) ---

/// Агент Биржи: выпуск КВАЗИ за **действие** Свода (ставки `svod_rewards`).
pub async fn agent_emit_for_action(
    state: &Arc<AppState>,
    citizen_id: &str,
    action: &str,
    units: u64,
    ref_id: Option<&str>,
) -> Result<(u64, u64), String> {
    let amount = crate::reward_svod::compute_action_reward(&state.db, action, units).await?;
    if amount == 0 {
        return Ok((0, 0));
    }
    let new_balance = crate::reward_svod::credit_reward_ledger(
        &state.db,
        citizen_id,
        action,
        units,
        amount,
        ref_id,
    )
    .await?;

    let event_id = format!(
        "ex_emit_{}_{}",
        action,
        ref_id.unwrap_or(&Uuid::new_v4().to_string())
    );
    if let Err(e) = emit_event(
        state,
        "ExchangeEmission",
        "Агент Биржи",
        "Эмиссия КВАЗИ за действие Свода",
        format!("{units}×{action} → {amount} КВАЗИ → {citizen_id}"),
        &event_id,
        json!({
            "citizen_id": citizen_id,
            "action": action,
            "item_code": null,
            "units": units,
            "amount": amount as i64,
            "balance": new_balance as i64,
            "ref_id": ref_id,
            "emitter": "exchange_agent",
            "kind": "action",
        }),
    )
    .await
    {
        tracing::warn!(error = %e, event_id = %event_id, "exchange_agent: ExchangeEmission (action) not recorded");
    }

    tracing::info!(
        citizen_id = %citizen_id,
        action = %action,
        units,
        amount,
        new_balance,
        "exchange_agent: emitted QUAZI for action"
    );
    Ok((amount, new_balance))
}

/// Агент Биржи: выпуск КВАЗИ за **создание предмета** (ставка = `price_kvazi` × qty).
pub async fn agent_emit_for_item(
    state: &Arc<AppState>,
    citizen_id: &str,
    item_code: &str,
    quantity: u64,
    ref_id: Option<&str>,
) -> Result<(u64, u64), String> {
    if quantity == 0 {
        return Ok((0, 0));
    }
    let item = load_item(&state.db, item_code)
        .await
        .map_err(|e| e.to_string())?;
    let amount = (item.price_kvazi.max(0) as u64).saturating_mul(quantity);
    if amount == 0 {
        return Ok((0, 0));
    }
    let ledger_action = format!("item_create:{item_code}");
    let new_balance = crate::reward_svod::credit_reward_ledger(
        &state.db,
        citizen_id,
        &ledger_action,
        quantity,
        amount,
        ref_id,
    )
    .await?;

    let event_id = format!(
        "ex_emit_item_{}_{}",
        item_code,
        ref_id.unwrap_or(&Uuid::new_v4().to_string())
    );
    if let Err(e) = emit_event(
        state,
        "ExchangeEmission",
        "Агент Биржи",
        "Эмиссия КВАЗИ за создание предмета Свода",
        format!("{quantity}×{item_code} → {amount} КВАЗИ → {citizen_id}"),
        &event_id,
        json!({
            "citizen_id": citizen_id,
            "action": null,
            "item_code": item_code,
            "item_title": item.title,
            "units": quantity,
            "amount": amount as i64,
            "balance": new_balance as i64,
            "unit_price_kvazi": item.price_kvazi,
            "ref_id": ref_id,
            "emitter": "exchange_agent",
            "kind": "item",
        }),
    )
    .await
    {
        tracing::warn!(error = %e, event_id = %event_id, "exchange_agent: ExchangeEmission (item) not recorded");
    }

    tracing::info!(
        citizen_id = %citizen_id,
        item_code = %item_code,
        quantity,
        amount,
        new_balance,
        "exchange_agent: emitted QUAZI for item create"
    );
    Ok((amount, new_balance))
}

#[derive(Debug, Deserialize)]
pub struct CreateItemBody {
    pub item_code: String,
    pub quantity: u64,
}

/// POST /exchange/create-item — гражданин создаёт предмет в инвентарь + эмиссия Агента Биржи.
pub async fn create_item_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<CreateItemBody>,
) -> ExchangeResult<Json<ApiResponse>> {
    if auth.is_node {
        return Err(ExchangeError::Unauthorized("Node cannot create items".into()));
    }
    if !auth.can_open_state_deal() {
        return Err(ExchangeError::Unauthorized(
            "Создание предметов: гражданин и выше".into(),
        ));
    }
    if body.quantity == 0 {
        return Err(ExchangeError::BadRequest("quantity must be > 0".into()));
    }
    let citizen_id = auth.resolve_account_id(&state.db).await;
    ensure_active_citizen(&state.db, &citizen_id).await?;
    let item_code = body.item_code.trim().to_string();
    let item = load_item(&state.db, &item_code).await?;
    let qty = body.quantity as i64;

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    let inv = lock_citizen_item(&mut tx, &citizen_id, &item.code).await?;
    set_citizen_item(&mut tx, &citizen_id, &item.code, inv + qty).await?;
    // Track created qty on board.
    sqlx::query(
        "INSERT INTO exchange_item_created (item_code, created_qty, updated_at)
         VALUES ($1, $2, $3)
         ON CONFLICT (item_code) DO UPDATE
         SET created_qty = exchange_item_created.created_qty + EXCLUDED.created_qty,
             updated_at = EXCLUDED.updated_at",
    )
    .bind(&item.code)
    .bind(qty)
    .bind(now())
    .execute(&mut *tx)
    .await
    .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;
    tx.commit()
        .await
        .map_err(|e| ExchangeError::DatabaseError(e.to_string()))?;

    let ref_id = format!("create_{}_{}", item.code, Uuid::new_v4());
    let (amount, balance) = agent_emit_for_item(
        &state,
        &citizen_id,
        &item.code,
        body.quantity,
        Some(&ref_id),
    )
    .await
    .map_err(ExchangeError::BadRequest)?;

    Ok(Json(ApiResponse::success(json!({
        "citizen_id": citizen_id,
        "item_code": item.code,
        "item_title": item.title,
        "quantity": body.quantity,
        "inventory_quantity": inv + qty,
        "emission": {
            "amount": amount,
            "balance": balance,
            "emitter": "exchange_agent",
        },
        "note": "Предмет в инвентаре; КВАЗИ выпустил Агент Биржи по price_kvazi Свода предметов.",
    }))))
}
