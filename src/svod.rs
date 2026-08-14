use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Extension, Path, Query, State},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::auth::{AuthContext, Role};
use crate::response::ApiResponse;
use crate::AppState;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ServiceCategory {
    pub id: i32,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
}

/// Part of the unified Свод: `development` (Разработка) | `implementation` (Реализация).
pub const WORK_KIND_DEVELOPMENT: &str = "development";
pub const WORK_KIND_IMPLEMENTATION: &str = "implementation";

/// Official display name — one registry for exchange positions and action rates.
pub const SVOD_TITLE: &str = "Республиканский Свод Вознаграждений";

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ServiceItem {
    pub id: i32,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub category_id: Option<i32>,
    pub category_code: Option<String>,
    pub category_name: Option<String>,
    pub base_price: i64,
    pub min_quantity: i64,
    pub max_quantity: i64,
    pub is_active: bool,
    /// `development` | `implementation`
    pub work_kind: String,
    /// `global` (Доступ содержит Г) | `local` (только Ст/Сд/М)
    pub lot_scope: String,
    pub access_roles: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct SvodQuery {
    /// Filter: `development` | `implementation`
    pub work_kind: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateServiceRequest {
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub category_id: Option<i32>,
    pub category_code: Option<String>,
    pub base_price: i64,
    pub min_quantity: Option<i64>,
    pub max_quantity: Option<i64>,
    /// `development` | `implementation` (default: implementation)
    pub work_kind: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateServiceRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub category_id: Option<i32>,
    pub category_code: Option<String>,
    pub base_price: Option<i64>,
    pub min_quantity: Option<i64>,
    pub max_quantity: Option<i64>,
    pub is_active: Option<bool>,
    pub work_kind: Option<String>,
}

#[derive(Debug)]
pub enum SvodError {
    NotFound(String),
    Conflict(String),
    BadRequest(String),
    Forbidden(String),
    Database(String),
}

impl SvodError {
    fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            SvodError::NotFound(_) => StatusCode::NOT_FOUND,
            SvodError::Conflict(_) => StatusCode::CONFLICT,
            SvodError::BadRequest(_) => StatusCode::BAD_REQUEST,
            SvodError::Forbidden(_) => StatusCode::FORBIDDEN,
            SvodError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl std::fmt::Display for SvodError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SvodError::NotFound(msg) => write!(f, "{msg}"),
            SvodError::Conflict(msg) => write!(f, "{msg}"),
            SvodError::BadRequest(msg) => write!(f, "{msg}"),
            SvodError::Forbidden(msg) => write!(f, "{msg}"),
            SvodError::Database(msg) => write!(f, "Database error: {msg}"),
        }
    }
}

impl IntoResponse for SvodError {
    fn into_response(self) -> axum::response::Response {
        (self.status(), Json(ApiResponse::error(self.to_string()))).into_response()
    }
}

type SvodResult<T> = Result<T, SvodError>;

// Table `service_catalog` stores Свод exchange positions (items). API never exposes "catalog".
const SERVICE_SELECT: &str = "SELECT s.id, s.code, s.name, s.description, s.category_id,
    c.code AS category_code, c.name AS category_name,
    s.base_price, s.min_quantity, s.max_quantity, s.is_active, s.work_kind,
    s.lot_scope, s.access_roles
    FROM service_catalog s
    LEFT JOIN service_categories c ON s.category_id = c.id";

/// Lot scope from SVOD «Доступ»: Г → global (Агент СБ); иначе local (живой Страж).
/// Scope лота из доступа: Г ∈ доступ → global, иначе local.
/// Confirm-ось — не scope, а иерархия ролей (`access_level_from_access`).
pub fn lot_scope_from_access(access: &[String]) -> &'static str {
    if access.iter().any(|a| a == "Гражданин") {
        "global"
    } else {
        "local"
    }
}

/// Русская метка доступа Свода → Role.
pub fn role_from_access_label(label: &str) -> Option<Role> {
    match label.trim() {
        "Гражданин" | "Г" => Some(Role::Citizen),
        "Страж" | "Ст" => Some(Role::Guardian),
        "Судья" | "Сд" => Some(Role::Judge),
        "Мэр" | "М" => Some(Role::Mayor),
        "Айя" => Some(Role::Aiya),
        other => Role::from_str(other),
    }
}

/// Минимальный уровень доступа лота = наименьший ранг среди ролей в «Доступ».
/// Пустой список → Citizen (fallback).
pub fn access_level_from_access(access: &[String]) -> Role {
    let mut min_rank = u8::MAX;
    let mut min_role = Role::Citizen;
    for label in access {
        if let Some(r) = role_from_access_label(label) {
            let rank = r.access_rank();
            if rank < min_rank {
                min_rank = rank;
                min_role = r;
            }
        }
    }
    min_role
}

/// Активная роль видит/может заявлять лот с данным доступом.
pub fn role_can_access_lot(role: &Role, access: &[String]) -> bool {
    if access.is_empty() {
        return matches!(role, Role::Citizen | Role::Guardian | Role::Judge | Role::Mayor | Role::Aiya | Role::Founder);
    }
    // Лот доступен, если роль ∈ списка ИЛИ ранг роли ≥ мин. уровня (выше видит ниже).
    let level = access_level_from_access(access);
    if role.meets_access_level(&level) {
        // Дополнительно: для local-only (без Г) гражданин не видит.
        if !access.iter().any(|a| a == "Гражданин") && matches!(role, Role::Citizen) {
            return false;
        }
        return true;
    }
    false
}

/// Normalize and validate work_kind query/body values.
pub fn normalize_work_kind(raw: &str) -> Result<String, SvodError> {
    match raw.trim().to_lowercase().as_str() {
        WORK_KIND_DEVELOPMENT | "разработка" => Ok(WORK_KIND_DEVELOPMENT.to_string()),
        WORK_KIND_IMPLEMENTATION | "реализация" => Ok(WORK_KIND_IMPLEMENTATION.to_string()),
        other if other.is_empty() => Err(SvodError::BadRequest(
            "work_kind is required when provided".to_string(),
        )),
        other => Err(SvodError::BadRequest(format!(
            "work_kind must be '{WORK_KIND_DEVELOPMENT}' or '{WORK_KIND_IMPLEMENTATION}', got {other}"
        ))),
    }
}

pub fn work_kind_label(kind: &str) -> &'static str {
    match kind {
        WORK_KIND_DEVELOPMENT => "Разработка",
        WORK_KIND_IMPLEMENTATION => "Реализация",
        _ => "?",
    }
}

fn normalize_code(code: &str) -> Result<String, SvodError> {
    let trimmed = code.trim();
    if trimmed.is_empty() {
        return Err(SvodError::BadRequest("Service code is required".to_string()));
    }
    // Position may be a human label («Промт шортс»): collapse whitespace → `_`.
    let collapsed: String = trimmed.split_whitespace().collect::<Vec<_>>().join("_");
    if collapsed.is_empty() {
        return Err(SvodError::BadRequest("Service code is required".to_string()));
    }
    if !collapsed
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return Err(SvodError::BadRequest(
            "Service code may contain only letters, digits, underscore and hyphen".to_string(),
        ));
    }
    Ok(collapsed)
}

async fn resolve_category_id(
    pool: &PgPool,
    category_id: Option<i32>,
    category_code: Option<&str>,
) -> SvodResult<Option<i32>> {
    if let Some(id) = category_id {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM service_categories WHERE id = $1)",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(|e| SvodError::Database(e.to_string()))?;
        if !exists {
            return Err(SvodError::BadRequest(format!(
                "Category id {id} not found"
            )));
        }
        return Ok(Some(id));
    }
    if let Some(code) = category_code.map(str::trim).filter(|c| !c.is_empty()) {
        let id: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM service_categories WHERE code = $1",
        )
        .bind(code)
        .fetch_optional(pool)
        .await
        .map_err(|e| SvodError::Database(e.to_string()))?;
        return id
            .map(Some)
            .ok_or_else(|| SvodError::BadRequest(format!("Category code {code} not found")));
    }
    Ok(None)
}

pub async fn get_categories(pool: &PgPool) -> SvodResult<Vec<ServiceCategory>> {
    sqlx::query_as::<_, ServiceCategory>(
        "SELECT id, code, name, description FROM service_categories ORDER BY code",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| SvodError::Database(e.to_string()))
}

/// Load Свод exchange positions (`parts.*.items`). Internal storage table: `service_catalog`.
pub async fn get_svod_items(
    pool: &PgPool,
    active_only: bool,
    work_kind: Option<&str>,
) -> SvodResult<Vec<ServiceItem>> {
    get_positions(pool, active_only, work_kind).await
}

/// Alias kept for older internal call sites; prefer [`get_svod_items`].
pub async fn get_positions(
    pool: &PgPool,
    active_only: bool,
    work_kind: Option<&str>,
) -> SvodResult<Vec<ServiceItem>> {
    let kind = match work_kind {
        Some(raw) => Some(normalize_work_kind(raw)?),
        None => None,
    };
    let sql = match (active_only, kind.is_some()) {
        (true, true) => format!(
            "{SERVICE_SELECT} WHERE s.is_active = TRUE AND s.work_kind = $1 ORDER BY s.code"
        ),
        (true, false) => format!("{SERVICE_SELECT} WHERE s.is_active = TRUE ORDER BY s.code"),
        (false, true) => format!("{SERVICE_SELECT} WHERE s.work_kind = $1 ORDER BY s.code"),
        (false, false) => format!("{SERVICE_SELECT} ORDER BY s.code"),
    };
    let mut q = sqlx::query_as::<_, ServiceItem>(sqlx::AssertSqlSafe(sql));
    if let Some(ref k) = kind {
        q = q.bind(k);
    }
    q.fetch_all(pool)
        .await
        .map_err(|e| SvodError::Database(e.to_string()))
}

pub async fn get_service_by_code(
    pool: &PgPool,
    code: &str,
    active_only: bool,
) -> SvodResult<ServiceItem> {
    let code = normalize_code(code)?;
    let sql = if active_only {
        format!("{SERVICE_SELECT} WHERE s.code = $1 AND s.is_active = TRUE")
    } else {
        format!("{SERVICE_SELECT} WHERE s.code = $1")
    };
    sqlx::query_as::<_, ServiceItem>(sqlx::AssertSqlSafe(sql))
        .bind(&code)
        .fetch_optional(pool)
        .await
        .map_err(|e| SvodError::Database(e.to_string()))?
        .ok_or_else(|| SvodError::NotFound(format!("Service {code} not found")))
}

pub async fn create_service(pool: &PgPool, req: CreateServiceRequest) -> SvodResult<ServiceItem> {
    let code = normalize_code(&req.code)?;
    if req.name.trim().is_empty() {
        return Err(SvodError::BadRequest("Service name is required".to_string()));
    }
    if req.base_price <= 0 {
        return Err(SvodError::BadRequest(
            "base_price must be greater than 0 (КВАЗИ)".to_string(),
        ));
    }
    let min_quantity = req.min_quantity.unwrap_or(1);
    let max_quantity = req.max_quantity.unwrap_or(100);
    if min_quantity <= 0 || max_quantity <= 0 || min_quantity > max_quantity {
        return Err(SvodError::BadRequest(
            "Invalid min_quantity/max_quantity range".to_string(),
        ));
    }

    let category_id =
        resolve_category_id(pool, req.category_id, req.category_code.as_deref()).await?;
    let work_kind = match req.work_kind.as_deref() {
        Some(raw) => normalize_work_kind(raw)?,
        None => WORK_KIND_IMPLEMENTATION.to_string(),
    };

    sqlx::query(
        "INSERT INTO service_catalog (code, name, description, category_id, base_price, min_quantity, max_quantity, work_kind, is_active, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, TRUE, NOW())",
    )
    .bind(&code)
    .bind(req.name.trim())
    .bind(req.description.as_deref())
    .bind(category_id)
    .bind(req.base_price)
    .bind(min_quantity)
    .bind(max_quantity)
    .bind(&work_kind)
    .execute(pool)
    .await
    .map_err(|e| {
        if e.to_string().contains("duplicate key") {
            SvodError::Conflict(format!("Service code {code} already exists"))
        } else {
            SvodError::Database(e.to_string())
        }
    })?;

    tracing::info!(code = %code, base_price = req.base_price, work_kind = %work_kind, "svod service created");
    get_service_by_code(pool, &code, false).await
}

pub async fn update_service(
    pool: &PgPool,
    code: &str,
    req: UpdateServiceRequest,
) -> SvodResult<ServiceItem> {
    let code = normalize_code(code)?;
    let existing = get_service_by_code(pool, &code, false).await?;

    let name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&existing.name);
    let description = req.description.as_deref().or(existing.description.as_deref());
    let base_price = req.base_price.unwrap_or(existing.base_price);
    let min_quantity = req.min_quantity.unwrap_or(existing.min_quantity);
    let max_quantity = req.max_quantity.unwrap_or(existing.max_quantity);
    let is_active = req.is_active.unwrap_or(existing.is_active);
    let work_kind = match req.work_kind.as_deref() {
        Some(raw) => normalize_work_kind(raw)?,
        None => existing.work_kind.clone(),
    };

    if base_price <= 0 {
        return Err(SvodError::BadRequest(
            "base_price must be greater than 0 (КВАЗИ)".to_string(),
        ));
    }
    if min_quantity <= 0 || max_quantity <= 0 || min_quantity > max_quantity {
        return Err(SvodError::BadRequest(
            "Invalid min_quantity/max_quantity range".to_string(),
        ));
    }

    let category_id = if req.category_id.is_some() || req.category_code.is_some() {
        resolve_category_id(pool, req.category_id, req.category_code.as_deref()).await?
    } else {
        existing.category_id
    };

    sqlx::query(
        "UPDATE service_catalog SET name = $1, description = $2, category_id = $3,
         base_price = $4, min_quantity = $5, max_quantity = $6, is_active = $7,
         work_kind = $8, updated_at = NOW()
         WHERE code = $9",
    )
    .bind(name)
    .bind(description)
    .bind(category_id)
    .bind(base_price)
    .bind(min_quantity)
    .bind(max_quantity)
    .bind(is_active)
    .bind(&work_kind)
    .bind(&code)
    .execute(pool)
    .await
    .map_err(|e| SvodError::Database(e.to_string()))?;

    tracing::info!(code = %code, work_kind = %work_kind, "svod service updated");
    get_service_by_code(pool, &code, false).await
}

pub async fn toggle_service(pool: &PgPool, code: &str, active: bool) -> SvodResult<ServiceItem> {
    let code = normalize_code(code)?;
    let result = sqlx::query(
        "UPDATE service_catalog SET is_active = $1, updated_at = NOW() WHERE code = $2",
    )
    .bind(active)
    .bind(&code)
    .execute(pool)
    .await
    .map_err(|e| SvodError::Database(e.to_string()))?;

    if result.rows_affected() == 0 {
        return Err(SvodError::NotFound(format!("Service {code} not found")));
    }

    tracing::info!(code = %code, active, "svod service toggled");
    get_service_by_code(pool, &code, false).await
}

fn require_aiya(auth: &AuthContext) -> SvodResult<()> {
    if auth.can_manage_svod() {
        Ok(())
    } else if auth.is_node {
        Err(SvodError::Forbidden(
            "Node credentials cannot manage the Республиканский Свод Вознаграждений".to_string(),
        ))
    } else {
        Err(SvodError::Forbidden(
            "Only Aiya and Founder can manage the Республиканский Свод Вознаграждений (Agent: read-only)"
                .to_string(),
        ))
    }
}

fn position_with_label(s: &ServiceItem) -> serde_json::Value {
    let access_level = access_level_from_access(&s.access_roles);
    serde_json::json!({
        "kind": "position",
        "id": s.id,
        "code": s.code,
        "name": s.name,
        "description": s.description,
        "category_id": s.category_id,
        "category_code": s.category_code,
        "category_name": s.category_name,
        "base_price": s.base_price,
        "min_quantity": s.min_quantity,
        "max_quantity": s.max_quantity,
        "is_active": s.is_active,
        "lot_scope": s.lot_scope,
        "access": s.access_roles,
        "access_roles": s.access_roles,
        "access_level": access_level.as_str(),
        "access_level_ru": match access_level {
            Role::Citizen => "Гражданин",
            Role::Guardian => "Страж",
            Role::Judge => "Судья",
            Role::Mayor => "Мэр",
            Role::Aiya => "Айя",
            Role::Founder => "Основатель",
            Role::Agent => "Агент",
            Role::Candidate => "Кандидат",
        },
        "work_kind": s.work_kind,
        "work_kind_label": work_kind_label(&s.work_kind),
    })
}

fn rate_with_kind(r: &crate::reward_svod::RewardPublic) -> serde_json::Value {
    serde_json::json!({
        "kind": "rate",
        "action": r.action,
        "code": r.action,
        "unit": r.unit,
        "reward": r.reward,
        "work_kind": r.work_kind,
        "work_kind_label": r.work_kind_label,
    })
}

fn build_part(
    kind: &str,
    title: &str,
    hint: &str,
    items: Vec<serde_json::Value>,
    rates: Vec<serde_json::Value>,
) -> serde_json::Value {
    // Canonical fields: items (биржевые позиции), rates (ставки за действия).
    // Legacy aliases services/rewards kept for older Passport builds.
    serde_json::json!({
        "work_kind": kind,
        "title": title,
        "hint": hint,
        "items": items.clone(),
        "rates": rates.clone(),
        "services": items,
        "rewards": rates,
    })
}

/// Двучастная структура единого Республиканского Свода: Разработка + Реализация.
pub fn build_svod_parts(
    positions: &[ServiceItem],
    rates: &[crate::reward_svod::RewardPublic],
) -> serde_json::Value {
    let filter_pos = |kind: &str| {
        positions
            .iter()
            .filter(|s| s.work_kind == kind)
            .map(position_with_label)
            .collect::<Vec<_>>()
    };
    let filter_rates = |kind: &str| {
        rates
            .iter()
            .filter(|r| r.work_kind == kind)
            .map(rate_with_kind)
            .collect::<Vec<_>>()
    };
    serde_json::json!({
        "development": build_part(
            WORK_KIND_DEVELOPMENT,
            "Разработка",
            "Идеи, темы, креативный вклад",
            filter_pos(WORK_KIND_DEVELOPMENT),
            filter_rates(WORK_KIND_DEVELOPMENT),
        ),
        "implementation": build_part(
            WORK_KIND_IMPLEMENTATION,
            "Реализация",
            "Код, правки, ролики, просмотры, поставка",
            filter_pos(WORK_KIND_IMPLEMENTATION),
            filter_rates(WORK_KIND_IMPLEMENTATION),
        ),
    })
}

pub fn build_svod_sections(
    positions: &[ServiceItem],
    rates: &[crate::reward_svod::RewardPublic],
) -> Vec<serde_json::Value> {
    let parts = build_svod_parts(positions, rates);
    vec![
        parts["development"].clone(),
        parts["implementation"].clone(),
    ]
}

fn role_view_payload() -> serde_json::Value {
    serde_json::json!({
        "filter": {
            "ui": "button_near_balance",
            "when_off": "showcase_shows_all; params_chosen_on_submit",
            "mode": ["offers", "needs"],
            "svod_code": "optional position code from GET /svod → lots",
        },
        "offers": {
            "label": "Предложения",
            "mode": "offers",
            "showcase": "GET /exchange/listings (госостаток > 0, по статусу)",
            "submit_svod": "GET /svod → lots",
            "submit_side": "application",
            "hint": "Витрина: лоты с положительным госостатком. Заявка → Агент Биржи → СБ по иерархии доступа.",
        },
        "needs": {
            "label": "Заявка",
            "mode": "needs",
            "showcase": "GET /exchange/listings",
            "submit_svod": "GET /svod → lots",
            "submit": "POST /exchange/needs или заявка с витрины",
            "hint": "Секретарь выставляет только global. Confirm: роль ↔ access_level.",
        },
        "confirm": {
            "hierarchy": "Citizen < Guardian < Judge < Mayor < Aiya",
            "auto": "активная роль соответствует доступу лота → СБ сразу",
            "escalate": "роль ниже → подтверждение нужного уровня; выше подтверждает ниже",
        },
    })
}

/// GET /svod — единый цельный Республиканский Свод (один список лотов).
pub async fn get_svod_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SvodQuery>,
) -> Result<Json<ApiResponse>, SvodError> {
    let positions = get_svod_items(&state.db, true, q.work_kind.as_deref()).await?;
    let rates = crate::reward_svod::list_rewards_public_filtered(
        &state.db,
        q.work_kind.as_deref(),
    )
    .await
    .map_err(SvodError::Database)?;
    let parts = build_svod_parts(&positions, &rates);
    let sections = build_svod_sections(&positions, &rates);
    let items_json: Vec<_> = positions.iter().map(position_with_label).collect();
    let rates_json: Vec<_> = rates.iter().map(rate_with_kind).collect();
    Ok(Json(ApiResponse::success(serde_json::json!({
        "currency": "КВАЗИ",
        "title": SVOD_TITLE,
        "name": SVOD_TITLE,
        "rewards_title": SVOD_TITLE,
        "canon": "Единый Республиканский Свод = один список лотов (lots). Scope из Доступа (Г→global). Confirm — иерархия ролей (access_level).",
        "note": "parts/sections — legacy; клиентам использовать lots/items. services — legacy-алиас items.",
        "lots": items_json.clone(),
        "items": items_json.clone(),
        "rates": rates_json.clone(),
        "services": items_json,
        "rewards": rates_json,
        "parts": parts,
        "sections": sections,
        "role_view": role_view_payload(),
    }))))
}

/// GET /svod/rewards — тот же Свод, акцент на ставках за действия (две части).
pub async fn get_rewards_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<crate::reward_svod::RewardsQuery>,
) -> Result<Json<ApiResponse>, SvodError> {
    let positions = get_svod_items(&state.db, true, q.work_kind.as_deref()).await?;
    let rates = crate::reward_svod::list_rewards_public_filtered(
        &state.db,
        q.work_kind.as_deref(),
    )
    .await
    .map_err(SvodError::Database)?;
    // Same two-part registry as GET /svod (positions + rates), one title.
    let parts = build_svod_parts(&positions, &rates);
    let sections = build_svod_sections(&positions, &rates);
    let rates_json: Vec<_> = rates.iter().map(rate_with_kind).collect();
    let items_json: Vec<_> = positions.iter().map(position_with_label).collect();
    Ok(Json(ApiResponse::success(serde_json::json!({
        "currency": "КВАЗИ",
        "title": SVOD_TITLE,
        "name": SVOD_TITLE,
        "rewards_title": SVOD_TITLE,
        "canon": "Тот же Свод: lots/items — позиции биржи, rates — ставки за действия.",
        "lots": items_json.clone(),
        "items": items_json.clone(),
        "rates": rates_json.clone(),
        "services": items_json,
        "rewards": rates_json,
        "parts": parts,
        "sections": sections,
        "role_view": role_view_payload(),
    }))))
}

pub async fn get_categories_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiResponse>, SvodError> {
    let categories = get_categories(&state.db).await?;
    Ok(Json(ApiResponse::success(categories)))
}

pub async fn get_service_handler(
    State(state): State<Arc<AppState>>,
    Path(code): Path<String>,
) -> Result<Json<ApiResponse>, SvodError> {
    let item = get_service_by_code(&state.db, &code, true).await?;
    Ok(Json(ApiResponse::success(item)))
}

pub async fn create_service_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateServiceRequest>,
) -> Result<Json<ApiResponse>, SvodError> {
    require_aiya(&auth)?;
    let item = create_service(&state.db, req).await?;
    let _ = crate::chain_events::emit_signed_event(
        &state,
        "SvodServiceCreated",
        &auth.citizen_name,
        "Svod service created",
        &format!("created {}", item.code),
        Some(&format!("svod_create_{}", item.code)),
        serde_json::json!({
            "code": item.code,
            "name": item.name,
            "base_price": item.base_price,
            "min_quantity": item.min_quantity,
            "max_quantity": item.max_quantity,
        }),
    )
    .await;
    Ok(Json(ApiResponse::success(item)))
}

pub async fn update_service_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(code): Path<String>,
    Json(req): Json<UpdateServiceRequest>,
) -> Result<Json<ApiResponse>, SvodError> {
    require_aiya(&auth)?;
    let item = update_service(&state.db, &code, req).await?;
    let _ = crate::chain_events::emit_signed_event(
        &state,
        "SvodServiceUpdated",
        &auth.citizen_name,
        "Svod service updated",
        &format!("updated {}", item.code),
        Some(&format!(
            "svod_update_{}_{}",
            item.code,
            chrono::Utc::now().timestamp()
        )),
        serde_json::json!({
            "code": item.code,
            "name": item.name,
            "base_price": item.base_price,
            "is_active": item.is_active,
        }),
    )
    .await;
    Ok(Json(ApiResponse::success(item)))
}

pub async fn disable_service_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(code): Path<String>,
) -> Result<Json<ApiResponse>, SvodError> {
    require_aiya(&auth)?;
    let item = toggle_service(&state.db, &code, false).await?;
    let _ = crate::chain_events::emit_signed_event(
        &state,
        "SvodServiceDeactivated",
        &auth.citizen_name,
        "Svod service deactivated",
        &format!("deactivated {}", item.code),
        Some(&format!(
            "svod_disable_{}_{}",
            item.code,
            chrono::Utc::now().timestamp()
        )),
        serde_json::json!({
            "code": item.code,
            "is_active": item.is_active,
        }),
    )
    .await;
    Ok(Json(ApiResponse::success(serde_json::json!({
        "message": format!("Service {} disabled", code),
        "service": item
    }))))
}

/// Parsed etalon from `SVOD.md` (4-column legacy or 7-column canon tables).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SvodParsedFile {
    pub items: Vec<SvodParsedItem>,
    pub rates: Vec<SvodParsedRate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SvodParsedItem {
    pub code: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub number: Option<u32>,
    pub base_price: i64,
    pub unit: String,
    pub work_kind: String,
    pub access: Vec<String>,
    pub auto_verify: bool,
    #[serde(default)]
    pub shelf_life_secs: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SvodParsedRate {
    pub action: String,
    pub reward: i64,
    pub unit: String,
    pub work_kind: String,
    pub access: Vec<String>,
    pub auto_verify: bool,
}

fn norm_header(h: &str) -> String {
    h.trim()
        .to_lowercase()
        .replace('ё', "е")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn split_md_row(line: &str) -> Vec<String> {
    let line = line.trim().trim_matches('|');
    line.split('|')
        .map(|c| c.trim().to_string())
        .collect()
}

fn is_md_separator_row(cells: &[String]) -> bool {
    !cells.is_empty()
        && cells.iter().all(|c| {
            let t = c.trim();
            !t.is_empty()
                && t.chars()
                    .all(|ch| ch == '-' || ch == ':' || ch == ' ')
        })
}

fn parse_amount_cell(raw: &str) -> Option<i64> {
    let mut num = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
        } else if !num.is_empty() {
            break;
        }
    }
    if num.is_empty() {
        None
    } else {
        num.parse().ok()
    }
}

fn work_kind_from_heading(heading: &str) -> Option<&'static str> {
    let h = norm_header(heading);
    if h.contains("разработ") || h.contains("development") {
        Some(WORK_KIND_DEVELOPMENT)
    } else if h.contains("реализ") || h.contains("implementation") {
        Some(WORK_KIND_IMPLEMENTATION)
    } else {
        None
    }
}

fn row_is_item(unit: &str, code: &str) -> bool {
    let u = norm_header(unit);
    if u.contains("позиц") || u.contains("бирж") {
        return true;
    }
    // Infra lots may use descriptive units (e.g. «1 месяц работы узла»).
    matches!(
        code.trim().to_uppercase().as_str(),
        "NODE_MAINTENANCE"
    )
}

/// Case-insensitive remove of a normalized needle; returns (remainder, found).
fn remove_norm_phrase(hay: &str, needle_norm: &str) -> (String, bool) {
    let hay_chars: Vec<char> = hay.chars().collect();
    let hay_norm: Vec<char> = norm_header(hay).chars().collect();
    let needle: Vec<char> = needle_norm.chars().collect();
    if needle.is_empty() || hay_norm.len() < needle.len() {
        return (hay.to_string(), false);
    }
    if let Some(start) = hay_norm
        .windows(needle.len())
        .position(|w| w == needle.as_slice())
    {
        let mut out = String::new();
        out.extend(&hay_chars[..start]);
        out.push(' ');
        out.extend(&hay_chars[start + needle.len()..]);
        (out, true)
    } else {
        (hay.to_string(), false)
    }
}

/// Expand access cell (`Г, Ст, Сд, М` / `Г, Агент СБ`) into status names + `auto_verify`.
/// `auto_verify` is set only when the cell contains the marker «Агент СБ».
fn parse_access_cell(raw: &str) -> Result<(Vec<String>, bool), String> {
    let mut rest = raw.trim().to_string();
    let mut auto_verify = false;
    for marker in ["агент сб", "агентсб"] {
        let (next, found) = remove_norm_phrase(&rest, marker);
        if found {
            auto_verify = true;
            rest = next;
        }
    }

    let mut access: Vec<String> = Vec::new();
    for tok in rest.split(|c: char| c == ',' || c == ';' || c == '/' || c.is_whitespace()) {
        let t = tok.trim().trim_matches('*');
        if t.is_empty() {
            continue;
        }
        let n = norm_header(t);
        let status = if n == "г" || n.starts_with("гражданин") {
            "Гражданин"
        } else if n == "ст" || n.starts_with("страж") {
            "Страж"
        } else if n == "сд" || n.starts_with("судь") {
            "Судья"
        } else if n == "м" || n.starts_with("мэр") || n.starts_with("мер") {
            "Мэр"
        } else {
            return Err(format!(
                "SVOD.md Доступ: unknown token `{t}` (expected Г/Ст/Сд/М or «Агент СБ»)"
            ));
        };
        if !access.iter().any(|s| s == status) {
            access.push(status.to_string());
        }
    }
    if access.is_empty() {
        return Err(format!(
            "SVOD.md Доступ: empty status list in `{raw}` (need Г/Ст/Сд/М; «Агент СБ» only sets auto_verify)"
        ));
    }
    Ok((access, auto_verify))
}

/// Column layout of a SVOD.md table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SvodColLayout {
    /// Позиция | Ставка (КВАЗИ) | Единица | Доступ
    Classic,
    /// Лот / Код | Единица | Цена (КВАЗИ) | Доступ
    FlatLot,
    /// № | Название | Описание | Единица | Цена (КВАЗИ) | Доступ | Срок жизни
    Canon7,
}

/// Validate header (classic / flat-lot / 7-column canon).
fn validate_svod_header(cells: &[String]) -> Result<SvodColLayout, String> {
    if cells.len() == 7 {
        let c0 = norm_header(&cells[0]);
        let c1 = norm_header(&cells[1]);
        let c2 = norm_header(&cells[2]);
        let c3 = norm_header(&cells[3]);
        let c4 = norm_header(&cells[4]);
        let c5 = norm_header(&cells[5]);
        let c6 = norm_header(&cells[6]);
        let ok0 = c0 == "№" || c0.contains("номер") || c0 == "n";
        let ok1 = c1.contains("назван");
        let ok2 = c2.contains("описан");
        let ok3 = c3.contains("единиц") || c3 == "unit";
        let ok4 = c4.contains("цена") || c4.contains("квази") || c4.contains("price");
        let ok5 = c5.contains("доступ") || c5.contains("access");
        let ok6 = c6.contains("срок") || c6.contains("жизн") || c6.contains("ttl");
        if ok0 && ok1 && ok2 && ok3 && ok4 && ok5 && ok6 {
            return Ok(SvodColLayout::Canon7);
        }
    }

    if cells.len() != 4 {
        return Err(format!(
            "SVOD.md table must have 4 columns (legacy) or 7 columns (canon), got {}: {}",
            cells.len(),
            cells.join(" | ")
        ));
    }
    let c0 = norm_header(&cells[0]);
    let c1 = norm_header(&cells[1]);
    let c2 = norm_header(&cells[2]);
    let c3 = norm_header(&cells[3]);
    let ok_access =
        c3.contains("доступ") || c3.contains("access") || c3.contains("статус") || c3.contains("роль");

    // Flat: Лот/Код | Единица | Цена (КВАЗИ) | Доступ
    let flat0 = c0.contains("лот") || (c0.contains("код") && !c0.contains("позиц"));
    let flat1 = c1.contains("единиц") || c1 == "unit";
    let flat2 = c2.contains("цена") || c2.contains("ставк") || c2.contains("квази") || c2.contains("price");
    if flat0 && flat1 && flat2 && ok_access {
        return Ok(SvodColLayout::FlatLot);
    }

    // Classic: Позиция | Ставка | Единица | Доступ
    let ok0 = c0.contains("позиц") || c0.contains("код") || c0 == "code" || c0.contains("действи");
    let ok1 = c1.contains("ставк") || c1.contains("квази") || c1.contains("price") || c1.contains("reward");
    let ok2 = c2.contains("единиц") || c2 == "unit";
    if ok0 && ok1 && ok2 && ok_access {
        return Ok(SvodColLayout::Classic);
    }

    Err(format!(
        "SVOD.md expected 7 columns «№ | Название | Описание | Единица | Цена (КВАЗИ) | Доступ | Срок жизни», \
         or 4 columns «Лот/Код | Единица | Цена (КВАЗИ) | Доступ» \
         or «Позиция | Ставка (КВАЗИ) | Единица | Доступ», got: {}",
        cells.join(" | ")
    ))
}

/// Extract machine code from description (`Код: \`idea_submit\``) or fall back to name.
fn extract_lot_code(description: &str, name: &str) -> Result<String, String> {
    let desc = description.trim();
    if let Some(idx) = desc.to_lowercase().find("код:") {
        let rest = desc[idx + 4..].trim();
        if let Some(inner) = rest.strip_prefix('`').and_then(|s| s.split('`').next()) {
            let c = inner.trim();
            if !c.is_empty() {
                return Ok(c.to_string());
            }
        }
        let token = rest
            .split(|c: char| c.is_whitespace() || c == '.' || c == ',')
            .next()
            .unwrap_or("")
            .trim_matches('`')
            .trim();
        if !token.is_empty() {
            return Ok(token.to_string());
        }
    }
    let name = name.trim();
    if !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Ok(name.to_string());
    }
    normalize_code(name).map_err(|e| e.to_string())
}

/// Parse «30 дней», «24 часа», «7d», «—» → seconds (None = default shelf life).
fn parse_shelf_life_cell(raw: &str) -> Result<Option<i64>, String> {
    let t = raw.trim();
    if t.is_empty() || t == "—" || t == "-" || t == "∞" || norm_header(t) == "бессрочно" {
        return Ok(None);
    }
    let lower = norm_header(t);
    let mut num = String::new();
    for ch in lower.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
        } else if !num.is_empty() {
            break;
        }
    }
    let n: i64 = num
        .parse()
        .map_err(|_| format!("SVOD.md Срок жизни: cannot parse `{raw}`"))?;
    if n <= 0 {
        return Err(format!("SVOD.md Срок жизни: must be positive in `{raw}`"));
    }
    let secs = if lower.contains("час") || lower.ends_with(" ч") || lower.ends_with("ч") {
        n.saturating_mul(3600)
    } else if lower.contains("мин") {
        n.saturating_mul(60)
    } else if lower.contains("нед") {
        n.saturating_mul(604_800)
    } else if lower.contains("мес") {
        n.saturating_mul(2_592_000)
    } else if lower.contains("дн") || lower.ends_with('d') {
        n.saturating_mul(86_400)
    } else if lower.ends_with('h') {
        n.saturating_mul(3600)
    } else {
        // Bare number → days (canon tables use «30 дней»; fallback)
        n.saturating_mul(86_400)
    };
    Ok(Some(secs))
}

/// Path for parsed SVOD JSON cache on the node.
pub fn svod_parsed_cache_path() -> PathBuf {
    PathBuf::from("/var/lib/quazar/algonism/SVOD.parsed.json")
}

/// Write parsed etalon JSON into node cache.
pub fn write_svod_parsed_cache(json: &str) -> Result<(), String> {
    let path = svod_parsed_cache_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("SVOD cache mkdir: {e}"))?;
    }
    std::fs::write(&path, json).map_err(|e| format!("SVOD cache write {}: {e}", path.display()))
}

fn default_work_kind_for_code(code: &str) -> &'static str {
    let n = code.to_lowercase();
    if n.contains("creative") || n.contains("idea") || n.contains("промт") || n.contains("concept")
    {
        WORK_KIND_DEVELOPMENT
    } else {
        WORK_KIND_IMPLEMENTATION
    }
}

/// Parse `SVOD.md` (7-column canon or legacy 4-column tables) into lean items/rates JSON.
pub fn parse_svod_markdown(md: &str) -> Result<String, String> {
    let mut items: Vec<SvodParsedItem> = Vec::new();
    let mut rates: Vec<SvodParsedRate> = Vec::new();
    let mut current_kind = WORK_KIND_IMPLEMENTATION.to_string();
    let mut section_sets_kind = false;
    let mut in_table = false;
    let mut layout = SvodColLayout::Classic;

    for raw_line in md.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix('#') {
            let heading = rest.trim_start_matches('#').trim();
            if let Some(k) = work_kind_from_heading(heading) {
                current_kind = k.to_string();
                section_sets_kind = true;
            }
            in_table = false;
            continue;
        }

        if !line.starts_with('|') {
            in_table = false;
            continue;
        }

        let cells = split_md_row(line);
        if cells.is_empty() {
            continue;
        }
        if is_md_separator_row(&cells) {
            continue;
        }

        if !in_table {
            layout = validate_svod_header(&cells)?;
            in_table = true;
            continue;
        }

        let expected_cols = if layout == SvodColLayout::Canon7 { 7 } else { 4 };
        if cells.len() != expected_cols {
            return Err(format!(
                "SVOD.md data row must have exactly {expected_cols} columns, got {}: {}",
                cells.len(),
                cells.join(" | ")
            ));
        }

        if layout == SvodColLayout::Canon7 {
            let name = cells[1].trim().to_string();
            let description = cells[2].trim().to_string();
            if name.is_empty() {
                continue;
            }
            let code = extract_lot_code(&description, &name)?;
            let unit = cells[3].trim().to_string();
            if unit.is_empty() {
                return Err(format!("SVOD.md row `{code}`: empty Единица"));
            }
            let amount = parse_amount_cell(&cells[4]).unwrap_or(0);
            let (access, auto_verify) = parse_access_cell(&cells[5])
                .map_err(|e| format!("SVOD.md row `{code}`: {e}"))?;
            let shelf_life_secs = parse_shelf_life_cell(&cells[6])
                .map_err(|e| format!("SVOD.md row `{code}`: {e}"))?;
            let number = cells[0]
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|&n| n > 0);
            let work_kind = default_work_kind_for_code(&code).to_string();
            items.push(SvodParsedItem {
                code,
                name,
                description,
                number,
                base_price: amount,
                unit,
                work_kind,
                access,
                auto_verify,
                shelf_life_secs,
            });
            continue;
        }

        let code = cells[0].trim().to_string();
        if code.is_empty() {
            continue;
        }

        let (amount, unit) = match layout {
            SvodColLayout::Classic => {
                let amount = parse_amount_cell(&cells[1]).ok_or_else(|| {
                    format!("SVOD.md row `{code}`: missing numeric amount in column Ставка (КВАЗИ)")
                })?;
                let unit = cells[2].trim().to_string();
                (amount, unit)
            }
            SvodColLayout::FlatLot => {
                let unit = cells[1].trim().to_string();
                let amount = parse_amount_cell(&cells[2]).ok_or_else(|| {
                    format!("SVOD.md row `{code}`: missing numeric amount in column Цена (КВАЗИ)")
                })?;
                (amount, unit)
            }
            SvodColLayout::Canon7 => unreachable!(),
        };
        if unit.is_empty() {
            return Err(format!("SVOD.md row `{code}`: empty Единица"));
        }
        let (access, auto_verify) = parse_access_cell(&cells[3])
            .map_err(|e| format!("SVOD.md row `{code}`: {e}"))?;

        let work_kind = if section_sets_kind {
            current_kind.clone()
        } else {
            default_work_kind_for_code(&code).to_string()
        };

        if row_is_item(&unit, &code) {
            items.push(SvodParsedItem {
                code: code.clone(),
                name: code.clone(),
                description: String::new(),
                number: None,
                base_price: amount,
                unit,
                work_kind,
                access,
                auto_verify,
                shelf_life_secs: None,
            });
        } else {
            rates.push(SvodParsedRate {
                action: code,
                reward: amount,
                unit,
                work_kind,
                access,
                auto_verify,
            });
        }
    }

    if items.is_empty() && rates.is_empty() {
        return Err("SVOD.md tables produced no items and no rates".to_string());
    }

    let etalon = SvodParsedFile { items, rates };
    serde_json::to_string_pretty(&etalon).map_err(|e| format!("SVOD serialize: {e}"))
}

/// Upsert items/rates from parsed etalon JSON into local DB.
/// Positions absent from the etalon are deactivated (not deleted — FK safety).
pub async fn apply_svod_etalon_json(pool: &PgPool, raw: &str) -> Result<(), String> {
    let etalon: SvodParsedFile =
        serde_json::from_str(raw).map_err(|e| format!("SVOD etalon JSON parse: {e}"))?;
    if etalon.items.is_empty() && etalon.rates.is_empty() {
        return Err("SVOD etalon has no items and no rates".to_string());
    }

    // Ensure default IT category exists for catalog FK.
    sqlx::query(
        "INSERT INTO service_categories (code, name, description)
         VALUES ('IT', 'Information Technology', 'IT-услуги и разработка')
         ON CONFLICT (code) DO UPDATE
         SET name = EXCLUDED.name,
             description = EXCLUDED.description",
    )
    .execute(pool)
    .await
    .map_err(|e| format!("svod category IT: {e}"))?;

    let mut active_codes: Vec<String> = Vec::new();
    for item in &etalon.items {
        let code = normalize_code(item.code.trim()).map_err(|e| e.to_string())?;
        let display_name = if item.name.trim().is_empty() {
            code.as_str()
        } else {
            item.name.trim()
        };
        let work_kind = normalize_work_kind(&item.work_kind).map_err(|e| e.to_string())?;
        if item.base_price < 0 {
            return Err(format!("SVOD item {code}: base_price must be >= 0"));
        }
        let category_id = resolve_category_id(pool, None, Some("IT"))
            .await
            .map_err(|e| e.to_string())?;

        let lot_scope = lot_scope_from_access(&item.access);
        let desc = if item.description.trim().is_empty() {
            None
        } else {
            Some(item.description.as_str())
        };
        sqlx::query(
            "INSERT INTO service_catalog
                (code, name, description, category_id, base_price, min_quantity, max_quantity,
                 work_kind, lot_scope, access_roles, is_active, updated_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,TRUE,NOW())
             ON CONFLICT (code) DO UPDATE SET
                name = EXCLUDED.name,
                description = EXCLUDED.description,
                category_id = EXCLUDED.category_id,
                base_price = EXCLUDED.base_price,
                min_quantity = EXCLUDED.min_quantity,
                max_quantity = EXCLUDED.max_quantity,
                work_kind = EXCLUDED.work_kind,
                lot_scope = EXCLUDED.lot_scope,
                access_roles = EXCLUDED.access_roles,
                is_active = EXCLUDED.is_active,
                updated_at = NOW()",
        )
        .bind(&code)
        .bind(display_name)
        .bind(desc)
        .bind(category_id)
        .bind(item.base_price)
        .bind(1_i64)
        .bind(100_i64)
        .bind(&work_kind)
        .bind(lot_scope)
        .bind(&item.access)
        .execute(pool)
        .await
        .map_err(|e| format!("svod item {code}: {e}"))?;

        // Mirror canon lots into svod_rewards for action-based payout paths.
        sqlx::query(
            "INSERT INTO svod_rewards (action, unit, reward, work_kind, updated_at)
             VALUES ($1, $2, $3, $4, NOW())
             ON CONFLICT (action) DO UPDATE SET
                unit = EXCLUDED.unit,
                reward = EXCLUDED.reward,
                work_kind = EXCLUDED.work_kind,
                updated_at = NOW()",
        )
        .bind(&code)
        .bind(item.unit.trim())
        .bind(item.base_price)
        .bind(&work_kind)
        .execute(pool)
        .await
        .map_err(|e| format!("svod item→reward {code}: {e}"))?;

        active_codes.push(code);
    }

    for rate in &etalon.rates {
        let action = rate.action.trim();
        if action.is_empty() {
            continue;
        }
        let work_kind = normalize_work_kind(&rate.work_kind).map_err(|e| e.to_string())?;
        sqlx::query(
            "INSERT INTO svod_rewards (action, unit, reward, work_kind, updated_at)
             VALUES ($1, $2, $3, $4, NOW())
             ON CONFLICT (action) DO UPDATE SET
                unit = EXCLUDED.unit,
                reward = EXCLUDED.reward,
                work_kind = EXCLUDED.work_kind,
                updated_at = NOW()",
        )
        .bind(action)
        .bind(rate.unit.trim())
        .bind(rate.reward)
        .bind(&work_kind)
        .execute(pool)
        .await
        .map_err(|e| format!("svod rate {action}: {e}"))?;

        // Also mirror rates into service_catalog so local/export board works after DNA reload.
        let code = normalize_code(action).map_err(|e| e.to_string())?;
        let lot_scope = lot_scope_from_access(&rate.access);
        let category_id = resolve_category_id(pool, None, Some("IT"))
            .await
            .map_err(|e| e.to_string())?;
        if rate.reward >= 0 {
            sqlx::query(
                "INSERT INTO service_catalog
                    (code, name, description, category_id, base_price, min_quantity, max_quantity,
                     work_kind, lot_scope, access_roles, is_active, updated_at)
                 VALUES ($1,$2,$3,$4,$5,1,1000,$6,$7,$8,TRUE,NOW())
                 ON CONFLICT (code) DO UPDATE SET
                    name = EXCLUDED.name,
                    description = EXCLUDED.description,
                    category_id = EXCLUDED.category_id,
                    base_price = EXCLUDED.base_price,
                    work_kind = EXCLUDED.work_kind,
                    lot_scope = EXCLUDED.lot_scope,
                    access_roles = EXCLUDED.access_roles,
                    is_active = TRUE,
                    updated_at = NOW()",
            )
            .bind(&code)
            .bind(action)
            .bind(Some(rate.unit.trim()))
            .bind(category_id)
            .bind(rate.reward)
            .bind(&work_kind)
            .bind(lot_scope)
            .bind(&rate.access)
            .execute(pool)
            .await
            .map_err(|e| format!("svod rate→catalog {code}: {e}"))?;
            if !active_codes.contains(&code) {
                active_codes.push(code);
            }
        }
    }

    if !active_codes.is_empty() {
        sqlx::query(
            "UPDATE service_catalog
             SET is_active = FALSE, updated_at = NOW()
             WHERE is_active = TRUE AND NOT (code = ANY($1))",
        )
        .bind(&active_codes)
        .execute(pool)
        .await
        .map_err(|e| format!("svod deactivate missing (post-rates): {e}"))?;
    }

    tracing::info!(
        items = etalon.items.len(),
        rates = etalon.rates.len(),
        "SVOD etalon applied from markdown tables into Postgres"
    );
    Ok(())
}

/// Parse `SVOD.md` tables, cache lean JSON, apply into Postgres.
pub async fn apply_svod_etalon_markdown(pool: &PgPool, md: &str) -> Result<(), String> {
    let json = parse_svod_markdown(md)?;
    if let Err(e) = write_svod_parsed_cache(&json) {
        tracing::warn!(error = %e, "failed to write SVOD.parsed.json cache");
    }
    apply_svod_etalon_json(pool, &json).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_code_accepts_alphanumeric() {
        assert_eq!(normalize_code("WEB_DEV-1").unwrap(), "WEB_DEV-1");
    }

    #[test]
    fn normalize_code_accepts_unicode_and_spaces() {
        assert_eq!(normalize_code("Промт шортс").unwrap(), "Промт_шортс");
    }

    #[test]
    fn normalize_code_rejects_empty() {
        assert!(normalize_code("  ").is_err());
    }

    #[test]
    fn normalize_work_kind_accepts_aliases() {
        assert_eq!(normalize_work_kind("development").unwrap(), "development");
        assert_eq!(normalize_work_kind("Реализация").unwrap(), "implementation");
        assert!(normalize_work_kind("other").is_err());
    }

    #[test]
    fn parse_svod_markdown_four_columns_access_and_auto_verify() {
        let md = r#"
# Свод

## Часть 1. Разработка

| Позиция | Ставка (КВАЗИ) | Единица | Доступ |
|---------|----------------|---------|--------|
| CREATIVE_CONCEPT | 80 | позиция биржи | Г, Ст, Сд, М |
| idea_submit | 30 | 1 идея / тема | Г, Ст, Сд, М |

## Часть 2. Реализация

| Позиция | Ставка (КВАЗИ) | Единица | Доступ |
|---------|----------------|---------|--------|
| WEB_DEV | 100 | позиция биржи | Г, Ст, Сд, М |
| video_create | 50 | 1 ролик | Г, Ст, Сд, М |
| video_view | 10 | 10 просмотров | Г, Агент СБ |
"#;
        let json = parse_svod_markdown(md).expect("parse");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let items = v["items"].as_array().unwrap();
        let rates = v["rates"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(rates.len(), 3);
        assert_eq!(items[0]["code"], "CREATIVE_CONCEPT");
        assert_eq!(items[0]["base_price"], 80);
        assert_eq!(items[0]["unit"], "позиция биржи");
        assert_eq!(items[0]["work_kind"], "development");
        assert_eq!(
            items[0]["access"],
            serde_json::json!(["Гражданин", "Страж", "Судья", "Мэр"])
        );
        assert_eq!(items[0]["auto_verify"], false);
        assert_eq!(items[1]["code"], "WEB_DEV");
        assert_eq!(items[1]["work_kind"], "implementation");
        assert_eq!(rates[0]["action"], "idea_submit");
        assert_eq!(rates[0]["reward"], 30);
        assert_eq!(rates[0]["auto_verify"], false);
        assert_eq!(rates[1]["action"], "video_create");
        assert_eq!(rates[2]["action"], "video_view");
        assert_eq!(rates[2]["reward"], 10);
        assert_eq!(rates[2]["access"], serde_json::json!(["Гражданин"]));
        assert_eq!(rates[2]["auto_verify"], true);
    }

    #[test]
    fn parse_svod_flat_lot_table() {
        let md = r#"
# РЕСПУБЛИКАНСКИЙ СВОД ВОЗНАГРАЖДЕНИЙ КВАЗАРА

## 2. ТАБЛИЦА ЛОТОВ

| Лот / Код | Единица | Цена (КВАЗИ) | Доступ |
|-----------|---------|--------------|--------|
| CREATIVE_CONCEPT | позиция биржи | 80 | Г, Ст, Сд, М |
| WEB_DEV | позиция биржи | 100 | Г, Ст, Сд, М |
| idea_submit | 1 идея / тема | 30 | Г, Ст, Сд, М |
| video_view | 10 просмотров | 10 | Г |
| node_maintenance | 1 месяц работы узла | 50 | М |
"#;
        let json = parse_svod_markdown(md).expect("parse flat");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let items = v["items"].as_array().unwrap();
        let rates = v["rates"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(rates.len(), 2);
        assert_eq!(items[0]["code"], "CREATIVE_CONCEPT");
        assert_eq!(items[0]["base_price"], 80);
        assert_eq!(items[0]["work_kind"], "development");
        assert_eq!(items[1]["code"], "WEB_DEV");
        assert_eq!(items[1]["base_price"], 100);
        assert_eq!(items[2]["code"], "node_maintenance");
        assert_eq!(items[2]["base_price"], 50);
        assert_eq!(rates[0]["action"], "idea_submit");
        assert_eq!(rates[0]["reward"], 30);
        assert_eq!(rates[1]["action"], "video_view");
        assert_eq!(rates[1]["auto_verify"], false);
    }

    #[test]
    fn parse_svod_rejects_wrong_column_count() {
        let md = r#"
## Разработка

| Позиция | Ставка (КВАЗИ) | Единица |
|---------|----------------|---------|
| X | 1 | позиция биржи |
"#;
        assert!(parse_svod_markdown(md).is_err());
    }

    #[test]
    fn parse_svod_rejects_empty_tables() {
        let md = "# Empty\n\nno tables here\n";
        assert!(parse_svod_markdown(md).is_err());
    }

    #[test]
    fn parse_svod_canon_seven_columns() {
        let md = include_str!("../docs/algonism/SVOD.md");
        let json = parse_svod_markdown(md).expect("parse canon SVOD.md");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let items = v["items"].as_array().unwrap();
        assert_eq!(items.len(), 12);
        assert!(v["rates"].as_array().unwrap().is_empty());

        let nums: Vec<u32> = items
            .iter()
            .filter_map(|i| i["number"].as_u64().map(|n| n as u32))
            .collect();
        assert_eq!(nums.len(), 12);
        assert_eq!(nums, (1..=12).collect::<Vec<_>>());

        let order = items.iter().find(|i| i["code"] == "Order").unwrap();
        assert_eq!(order["base_price"], 0);
        assert_eq!(order["name"], "Заявка на бирже");
        assert_eq!(order["shelf_life_secs"], 2_592_000);

        let mayor = items.iter().find(|i| i["code"] == "MayorApplication").unwrap();
        assert_eq!(mayor["base_price"], 0);
        assert_eq!(mayor["shelf_life_secs"], 86_400);

        let idea = items.iter().find(|i| i["code"] == "idea_submit").unwrap();
        assert_eq!(idea["name"], "Подача идеи");
        assert_eq!(idea["base_price"], 30);
    }

    #[test]
    fn parse_shelf_life_variants() {
        assert_eq!(parse_shelf_life_cell("30 дней").unwrap(), Some(2_592_000));
        assert_eq!(parse_shelf_life_cell("24 часа").unwrap(), Some(86_400));
        assert_eq!(parse_shelf_life_cell("—").unwrap(), None);
    }

    #[test]
    fn extract_lot_code_from_description() {
        assert_eq!(
            extract_lot_code("Код: `idea_submit`. Текст", "Подача идеи").unwrap(),
            "idea_submit"
        );
        assert_eq!(extract_lot_code("", "Order").unwrap(), "Order");
    }

    #[test]
    fn lot_scope_from_access_citizen_is_global() {
        assert_eq!(
            lot_scope_from_access(&["Гражданин".into(), "Страж".into()]),
            "global"
        );
        assert_eq!(lot_scope_from_access(&["Страж".into(), "Мэр".into()]), "local");
        assert_eq!(lot_scope_from_access(&["Мэр".into()]), "local");
    }

    #[test]
    fn access_level_from_access_min_rank() {
        assert_eq!(
            access_level_from_access(&["Страж".into(), "Судья".into()]),
            Role::Guardian
        );
        assert_eq!(
            access_level_from_access(&["Судья".into(), "Мэр".into()]),
            Role::Judge
        );
        assert_eq!(
            access_level_from_access(&["Гражданин".into(), "Страж".into()]),
            Role::Citizen
        );
    }

    #[test]
    fn role_can_access_lot_citizen_local_denied() {
        assert!(!role_can_access_lot(
            &Role::Citizen,
            &["Страж".into(), "Судья".into()]
        ));
        assert!(role_can_access_lot(
            &Role::Guardian,
            &["Страж".into(), "Судья".into()]
        ));
        assert!(role_can_access_lot(
            &Role::Judge,
            &["Страж".into()]
        ));
    }
}
