//! Reward rates that belong to the single Svod (`/svod`): КВАЗИ for citizen actions.
//! Stored in `svod_rewards` (not a second product named «Свод»).

use std::sync::Arc;

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::AuthContext;
use crate::response::{self, ApiResponse};
use crate::AppState;

pub const ACTION_VIDEO_CREATE: &str = "video_create";
pub const ACTION_VIDEO_VIEW: &str = "video_view";

pub const UNIT_VIDEO_CREATE: &str = "1 ролик (15–30 секунд)";
pub const UNIT_VIDEO_VIEW: &str = "10 просмотров";

/// Default batch size for view rewards (10 views → one payout unit).
pub const VIDEO_VIEW_BATCH: u64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Reward {
    pub id: Uuid,
    pub action: String,
    pub unit: String,
    pub reward: i64,
    pub created_at: chrono::DateTime<Utc>,
    pub updated_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RewardPublic {
    pub action: String,
    pub unit: String,
    pub reward: i64,
}

impl From<Reward> for RewardPublic {
    fn from(r: Reward) -> Self {
        Self {
            action: r.action,
            unit: r.unit,
            reward: r.reward,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateRewardRequest {
    pub action: String,
    pub reward: i64,
    pub unit: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateVideoRequest {
    pub title: Option<String>,
    /// Video length in seconds (must be 15–30).
    pub duration_secs: i32,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ChannelVideo {
    pub id: Uuid,
    pub citizen_id: String,
    pub title: String,
    pub duration_secs: i32,
    pub view_count: i64,
    pub created_at: chrono::DateTime<Utc>,
}

fn default_video_create_reward() -> u64 {
    std::env::var("SVOD_VIDEO_CREATE_REWARD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50)
}

fn default_video_view_reward() -> u64 {
    std::env::var("SVOD_VIDEO_VIEW_REWARD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10)
}

/// Pure calculation from reward-per-unit and completed units.
///
/// - `video_create`: `units` = number of videos → `units * reward`
/// - `video_view`: `units` = number of views → `(units / 10) * reward`
pub fn get_reward(action: &str, units: u64) -> u64 {
    let per = match action {
        ACTION_VIDEO_CREATE => default_video_create_reward(),
        ACTION_VIDEO_VIEW => default_video_view_reward(),
        _ => return 0,
    };
    match action {
        ACTION_VIDEO_CREATE => units.saturating_mul(per),
        ACTION_VIDEO_VIEW => (units / VIDEO_VIEW_BATCH).saturating_mul(per),
        _ => 0,
    }
}

/// Same as [`get_reward`] but uses DB row reward when provided.
pub fn get_reward_with_rate(action: &str, units: u64, reward_per_unit: u64) -> u64 {
    match action {
        ACTION_VIDEO_CREATE => units.saturating_mul(reward_per_unit),
        ACTION_VIDEO_VIEW => (units / VIDEO_VIEW_BATCH).saturating_mul(reward_per_unit),
        _ => 0,
    }
}

pub async fn ensure_rewards_seeded(pool: &PgPool) -> Result<(), sqlx::Error> {
    let create_reward = default_video_create_reward() as i64;
    let view_reward = default_video_view_reward() as i64;

    sqlx::query(
        r#"
        INSERT INTO svod_rewards (action, unit, reward)
        VALUES
            ($1, $2, $3),
            ($4, $5, $6)
        ON CONFLICT (action) DO NOTHING
        "#,
    )
    .bind(ACTION_VIDEO_CREATE)
    .bind(UNIT_VIDEO_CREATE)
    .bind(create_reward)
    .bind(ACTION_VIDEO_VIEW)
    .bind(UNIT_VIDEO_VIEW)
    .bind(view_reward)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_rewards(pool: &PgPool) -> Result<Vec<Reward>, sqlx::Error> {
    sqlx::query_as::<_, Reward>(
        "SELECT id, action, unit, reward, created_at, updated_at FROM svod_rewards ORDER BY action ASC",
    )
    .fetch_all(pool)
    .await
}

pub async fn list_rewards_public(pool: &PgPool) -> Result<Vec<RewardPublic>, sqlx::Error> {
    Ok(list_rewards(pool)
        .await?
        .into_iter()
        .map(RewardPublic::from)
        .collect())
}

pub async fn get_reward_row(pool: &PgPool, action: &str) -> Result<Option<Reward>, sqlx::Error> {
    sqlx::query_as::<_, Reward>(
        "SELECT id, action, unit, reward, created_at, updated_at FROM svod_rewards WHERE action = $1",
    )
    .bind(action)
    .fetch_optional(pool)
    .await
}

async fn credit_balance(pool: &PgPool, citizen_id: &str, amount: u64) -> Result<u64, String> {
    if amount == 0 {
        return Ok(0);
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM balances WHERE citizen_id = $1)")
        .bind(citizen_id)
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?;
    if !exists {
        sqlx::query("INSERT INTO balances (citizen_id, amount) VALUES ($1, 0)")
            .bind(citizen_id)
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
    }
    sqlx::query("UPDATE balances SET amount = amount + $1 WHERE citizen_id = $2")
        .bind(amount as i64)
        .bind(citizen_id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    let bal: i64 = sqlx::query_scalar("SELECT amount FROM balances WHERE citizen_id = $1")
        .bind(citizen_id)
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(bal as u64)
}

async fn record_ledger(
    pool: &PgPool,
    citizen_id: &str,
    action: &str,
    units: u64,
    amount: u64,
    ref_id: Option<&str>,
) -> Result<(), String> {
    sqlx::query(
        r#"
        INSERT INTO svod_ledger (citizen_id, action, units, amount, ref_id)
        VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(citizen_id)
    .bind(action)
    .bind(units as i64)
    .bind(amount as i64)
    .bind(ref_id)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Award КВАЗИ for an action using current Svod reward rates.
pub async fn award_for_action(
    pool: &PgPool,
    citizen_id: &str,
    action: &str,
    units: u64,
    ref_id: Option<&str>,
) -> Result<(u64, u64), String> {
    let row = get_reward_row(pool, action)
        .await
        .map_err(|e| e.to_string())?;
    let rate = row
        .as_ref()
        .map(|r| r.reward.max(0) as u64)
        .unwrap_or_else(|| match action {
            ACTION_VIDEO_CREATE => default_video_create_reward(),
            ACTION_VIDEO_VIEW => default_video_view_reward(),
            _ => 0,
        });
    let amount = get_reward_with_rate(action, units, rate);
    if amount == 0 {
        return Ok((0, 0));
    }
    let new_balance = credit_balance(pool, citizen_id, amount).await?;
    record_ledger(pool, citizen_id, action, units, amount, ref_id).await?;
    tracing::info!(
        citizen_id = %citizen_id,
        action = %action,
        units,
        amount,
        ref_id = ref_id.unwrap_or(""),
        new_balance,
        "svod: awarded QUAZI reward"
    );
    Ok((amount, new_balance))
}

/// POST /svod/admin/reward — Aiya/Founder updates a reward rate inside the Svod.
pub async fn update_reward_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<UpdateRewardRequest>,
) -> impl IntoResponse {
    if auth.is_node {
        return response::forbidden("Node credentials cannot update svod rewards");
    }
    if !auth.can_manage_svod() {
        return response::forbidden("Only Aiya / Founder can update svod rewards");
    }
    let action = req.action.trim();
    if action != ACTION_VIDEO_CREATE && action != ACTION_VIDEO_VIEW {
        return response::bad_request(format!(
            "Unknown action '{action}'. Allowed: {ACTION_VIDEO_CREATE}, {ACTION_VIDEO_VIEW}"
        ));
    }
    if req.reward < 0 {
        return response::bad_request("reward must be >= 0");
    }
    let unit = req
        .unit
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if action == ACTION_VIDEO_CREATE {
                UNIT_VIDEO_CREATE.to_string()
            } else {
                UNIT_VIDEO_VIEW.to_string()
            }
        });

    let updated = sqlx::query_as::<_, Reward>(
        r#"
        INSERT INTO svod_rewards (action, unit, reward, created_at, updated_at)
        VALUES ($1, $2, $3, NOW(), NOW())
        ON CONFLICT (action) DO UPDATE SET
            unit = EXCLUDED.unit,
            reward = EXCLUDED.reward,
            updated_at = NOW()
        RETURNING id, action, unit, reward, created_at, updated_at
        "#,
    )
    .bind(action)
    .bind(&unit)
    .bind(req.reward)
    .fetch_one(&state.db)
    .await;

    match updated {
        Ok(row) => {
            tracing::info!(
                action = %row.action,
                reward = row.reward,
                by = %auth.citizen_name,
                "svod: reward rate updated"
            );
            Json(ApiResponse::success(RewardPublic::from(row))).into_response()
        }
        Err(e) => response::internal_error(format!("Database error: {e}")),
    }
}

async fn resolve_auth_citizen_id(pool: &PgPool, auth: &AuthContext) -> Result<String, String> {
    if auth.is_node {
        return Err("Node credentials cannot use video rewards".into());
    }
    let id: Option<String> = sqlx::query_scalar("SELECT id FROM citizens WHERE name = $1")
        .bind(&auth.citizen_name)
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())?;
    id.ok_or_else(|| "Citizen not found".into())
}

/// POST /api/channel/videos — register a short video → award create reward.
pub async fn create_video_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<CreateVideoRequest>,
) -> impl IntoResponse {
    let citizen_id = match resolve_auth_citizen_id(&state.db, &auth).await {
        Ok(id) => id,
        Err(e) => return response::forbidden(e),
    };
    if req.duration_secs < 15 || req.duration_secs > 30 {
        return response::bad_request("duration_secs must be between 15 and 30");
    }
    let title = req
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Без названия")
        .to_string();

    let video = sqlx::query_as::<_, ChannelVideo>(
        r#"
        INSERT INTO channel_videos (citizen_id, title, duration_secs)
        VALUES ($1, $2, $3)
        RETURNING id, citizen_id, title, duration_secs, view_count, created_at
        "#,
    )
    .bind(&citizen_id)
    .bind(&title)
    .bind(req.duration_secs)
    .fetch_one(&state.db)
    .await;

    let video = match video {
        Ok(v) => v,
        Err(e) => return response::internal_error(format!("Database error: {e}")),
    };

    let award = award_for_action(
        &state.db,
        &citizen_id,
        ACTION_VIDEO_CREATE,
        1,
        Some(&video.id.to_string()),
    )
    .await;

    let (awarded, balance) = match award {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "svod: failed to award video_create");
            (0, 0)
        }
    };

    (
        StatusCode::CREATED,
        Json(ApiResponse::success(serde_json::json!({
            "video": video,
            "svod_award": {
                "action": ACTION_VIDEO_CREATE,
                "amount": awarded,
                "balance": balance,
            }
        }))),
    )
        .into_response()
}

/// POST /api/channel/videos/:id/view — record a view; every 10 views awards creator.
pub async fn view_video_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(video_id): Path<Uuid>,
) -> impl IntoResponse {
    if auth.is_node {
        return response::forbidden("Node credentials cannot record views");
    }

    let updated = sqlx::query_as::<_, ChannelVideo>(
        r#"
        UPDATE channel_videos
        SET view_count = view_count + 1
        WHERE id = $1
        RETURNING id, citizen_id, title, duration_secs, view_count, created_at
        "#,
    )
    .bind(video_id)
    .fetch_optional(&state.db)
    .await;

    let video = match updated {
        Ok(Some(v)) => v,
        Ok(None) => return response::not_found("Video not found"),
        Err(e) => return response::internal_error(format!("Database error: {e}")),
    };

    let new_count = video.view_count as u64;
    let old_count = new_count.saturating_sub(1);
    let new_batches = new_count / VIDEO_VIEW_BATCH;
    let old_batches = old_count / VIDEO_VIEW_BATCH;
    let batches_earned = new_batches.saturating_sub(old_batches);

    let mut awarded = 0u64;
    let mut balance = 0u64;
    if batches_earned > 0 {
        match award_for_action(
            &state.db,
            &video.citizen_id,
            ACTION_VIDEO_VIEW,
            batches_earned * VIDEO_VIEW_BATCH,
            Some(&video.id.to_string()),
        )
        .await
        {
            Ok((a, b)) => {
                awarded = a;
                balance = b;
            }
            Err(e) => tracing::error!(error = %e, "svod: failed to award video_view"),
        }
    }

    Json(ApiResponse::success(serde_json::json!({
        "video_id": video.id,
        "view_count": video.view_count,
        "svod_award": {
            "action": ACTION_VIDEO_VIEW,
            "batches": batches_earned,
            "amount": awarded,
            "creator_balance": balance,
        }
    })))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reward_video_create_one_clip() {
        std::env::set_var("SVOD_VIDEO_CREATE_REWARD", "50");
        assert_eq!(get_reward(ACTION_VIDEO_CREATE, 1), 50);
        assert_eq!(get_reward(ACTION_VIDEO_CREATE, 2), 100);
    }

    #[test]
    fn reward_video_view_batches() {
        std::env::set_var("SVOD_VIDEO_VIEW_REWARD", "10");
        assert_eq!(get_reward(ACTION_VIDEO_VIEW, 0), 0);
        assert_eq!(get_reward(ACTION_VIDEO_VIEW, 9), 0);
        assert_eq!(get_reward(ACTION_VIDEO_VIEW, 10), 10);
        assert_eq!(get_reward(ACTION_VIDEO_VIEW, 19), 10);
        assert_eq!(get_reward(ACTION_VIDEO_VIEW, 20), 20);
        assert_eq!(get_reward(ACTION_VIDEO_VIEW, 25), 20);
    }

    #[test]
    fn reward_unknown_action_is_zero() {
        assert_eq!(get_reward("unknown", 100), 0);
    }

    #[test]
    fn reward_with_custom_rate() {
        assert_eq!(get_reward_with_rate(ACTION_VIDEO_CREATE, 1, 75), 75);
        assert_eq!(get_reward_with_rate(ACTION_VIDEO_VIEW, 30, 5), 15);
    }
}
