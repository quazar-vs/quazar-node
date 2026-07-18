use std::sync::Arc;

use axum::{
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tokio::time;
use uuid::Uuid;

use crate::auth::{AuthContext, KeyStore};
use crate::block_producer;
use crate::blockchain::{
    build_signed_mayor_application_timed_out_event, build_signed_mayor_confirmed_event,
    build_signed_mayor_election_announced_event, build_signed_mayor_election_candidate_event,
    build_signed_mayor_election_endorse_event, build_signed_mayor_election_vote_cast_event,
    build_signed_mayor_election_vote_result_event, build_signed_mayor_election_vote_started_event,
    build_signed_node_started_event,
};
use crate::gossip;
use crate::models::Event;
use crate::node_generator::{
    allocate_mayor_node_id, format_secret_for_package, generate_node_secret_hex,
    peer_url_from_sync_peer, resolve_network_id, resolve_network_version, MayorNodePackageParams,
};
use crate::pending;
use crate::nodes::{Node, NodeStatus};
use crate::response::{self, ApiResponse};
use crate::types::Role;
use crate::validator::EventValidator;
use crate::AppState;

/// Absolute majority: floor(n/2)+1 == 50%+1 of cast votes.
pub fn absolute_majority_threshold(total_votes: i64) -> i64 {
    if total_votes <= 0 {
        return 1;
    }
    total_votes / 2 + 1
}

pub fn has_first_round_majority(votes_for_leader: i64, total_votes: i64) -> bool {
    total_votes > 0 && votes_for_leader >= absolute_majority_threshold(total_votes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElectionStatus {
    Nominating,
    Round1,
    Round2,
    Completed,
    Cancelled,
}

impl ElectionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ElectionStatus::Nominating => "Nominating",
            ElectionStatus::Round1 => "Round1",
            ElectionStatus::Round2 => "Round2",
            ElectionStatus::Completed => "Completed",
            ElectionStatus::Cancelled => "Cancelled",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "Nominating" => Some(ElectionStatus::Nominating),
            "Round1" => Some(ElectionStatus::Round1),
            "Round2" => Some(ElectionStatus::Round2),
            "Completed" => Some(ElectionStatus::Completed),
            "Cancelled" => Some(ElectionStatus::Cancelled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateStatus {
    Nominated,
    Qualified,
    Advanced,
    Eliminated,
    Winner,
}

impl CandidateStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            CandidateStatus::Nominated => "Nominated",
            CandidateStatus::Qualified => "Qualified",
            CandidateStatus::Advanced => "Advanced",
            CandidateStatus::Eliminated => "Eliminated",
            CandidateStatus::Winner => "Winner",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct MayorApplyRequest {
    pub ip: String,
}

#[derive(Debug, Serialize)]
pub struct MayorApplyResponse {
    pub application_id: String,
    /// Assigned P2P sync peer (`id@url`, same format as `QUAZAR_BOOTSTRAP_PEERS`).
    pub sync_peer: String,
}

#[derive(Debug, Serialize)]
pub struct MayorApplicationStatusResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct MayorSetupPackageQuery {
    pub ip: String,
    pub api_key: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MayorSetupPackageResponse {
    pub node_id: String,
    pub node_secret: String,
    /// Full bootstrap peer entry: `id@url` (preferred for QUAZAR_BOOTSTRAP_PEERS).
    pub sync_peer: String,
    /// Base URL extracted from sync_peer (no node id).
    pub peer_url: String,
    pub qz_id: String,
    pub citizen_name: String,
    pub network_id: String,
    pub version: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateElectionRequest {
    pub city_name: String,
}

#[derive(Debug, Deserialize)]
pub struct NominateRequest {
    pub election_id: String,
    pub candidate_id: String,
}

#[derive(Debug, Deserialize)]
pub struct EndorseRequest {
    pub candidate_id: String,
}

#[derive(Debug, Deserialize)]
pub struct VoteRequest {
    pub candidate_id: String,
}

#[derive(Debug, Deserialize)]
pub struct ListElectionsQuery {
    pub status: Option<String>,
    pub city_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow)]
pub struct MayorElection {
    pub id: String,
    pub city_name: String,
    pub status: String,
    pub current_round: i32,
    pub winner_id: Option<String>,
    pub winner_name: Option<String>,
    pub created_by: String,
    pub created_at: chrono::DateTime<Utc>,
    pub round1_started_at: Option<chrono::DateTime<Utc>>,
    pub round2_started_at: Option<chrono::DateTime<Utc>>,
    pub completed_at: Option<chrono::DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow)]
pub struct MayorCandidate {
    pub id: String,
    pub election_id: String,
    pub citizen_id: String,
    pub citizen_name: String,
    pub nominated_by: String,
    pub status: String,
    pub endorsements: i32,
    pub created_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow)]
pub struct MayorVote {
    pub id: String,
    pub election_id: String,
    pub round: i32,
    pub voter_id: String,
    pub candidate_id: String,
    pub created_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct CandidateTally {
    pub candidate_id: String,
    pub citizen_id: String,
    pub citizen_name: String,
    pub status: String,
    pub endorsements: i32,
    pub votes: i64,
}

#[derive(Debug, Serialize)]
pub struct ElectionDetail {
    pub election: MayorElection,
    pub candidates: Vec<CandidateTally>,
    pub total_votes_current_round: i64,
    pub majority_threshold: i64,
}

#[derive(Debug)]
pub enum MayorError {
    Message(String),
    Forbidden(String),
    NotFound(String),
    Database(String),
}

impl MayorError {
    fn bad_request(msg: impl Into<String>) -> Self {
        Self::Message(msg.into())
    }

    fn forbidden(msg: impl Into<String>) -> Self {
        Self::Forbidden(msg.into())
    }

    fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }

    fn to_response(&self) -> (StatusCode, Json<ApiResponse>) {
        let (status, message) = match self {
            MayorError::Message(m) => (StatusCode::BAD_REQUEST, m),
            MayorError::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            MayorError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            MayorError::Database(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (status, Json(ApiResponse::error(message)))
    }
}

async fn resolve_auth_citizen(
    auth: &AuthContext,
    pool: &PgPool,
) -> Result<(String, String, String), MayorError> {
    if auth.is_node {
        return Err(MayorError::forbidden(
            "Node credentials cannot participate in mayor elections",
        ));
    }
    if auth.role.is_agent() {
        return Err(MayorError::forbidden(
            auth.insufficient_permissions_message().to_string(),
        ));
    }

    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT id, name, status FROM citizens WHERE name = $1",
    )
    .bind(&auth.citizen_name)
    .fetch_optional(pool)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    let (id, name, status) =
        row.ok_or_else(|| MayorError::bad_request("Гражданин-инициатор не найден в реестре"))?;
    if status != "active" {
        return Err(MayorError::bad_request(format!(
            "Гражданин не может участвовать: статус {status}"
        )));
    }
    Ok((id, name, status))
}

async fn load_active_citizen(
    pool: &PgPool,
    citizen_id: &str,
) -> Result<(String, String), MayorError> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT id, name, status FROM citizens WHERE id = $1",
    )
    .bind(citizen_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    let (id, name, status) =
        row.ok_or_else(|| MayorError::not_found("Гражданин-кандидат не найден"))?;
    if status != "active" {
        return Err(MayorError::bad_request(format!(
            "Кандидат не активен: статус {status}"
        )));
    }
    Ok((id, name))
}

async fn fetch_election(pool: &PgPool, id: &str) -> Result<MayorElection, MayorError> {
    sqlx::query_as::<_, MayorElection>(
        r#"
        SELECT id, city_name, status, current_round, winner_id, winner_name,
               created_by, created_at, round1_started_at, round2_started_at, completed_at
        FROM mayor_elections WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?
    .ok_or_else(|| MayorError::not_found("Выборы мэра не найдены"))
}

async fn refresh_endorsement_count(pool: &PgPool, candidate_id: &str) -> Result<(), MayorError> {
    sqlx::query(
        r#"
        UPDATE mayor_candidates
        SET endorsements = (
            SELECT COUNT(*)::int FROM mayor_endorsements WHERE candidate_id = $1
        )
        WHERE id = $1
        "#,
    )
    .bind(candidate_id)
    .execute(pool)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;
    Ok(())
}

async fn count_votes_for_candidate(
    pool: &PgPool,
    election_id: &str,
    round: i32,
    candidate_id: &str,
) -> Result<i64, MayorError> {
    sqlx::query_scalar(
        r#"
        SELECT COUNT(*)::bigint FROM mayor_votes
        WHERE election_id = $1 AND round = $2 AND candidate_id = $3
        "#,
    )
    .bind(election_id)
    .bind(round)
    .bind(candidate_id)
    .fetch_one(pool)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))
}

async fn count_votes_round(pool: &PgPool, election_id: &str, round: i32) -> Result<i64, MayorError> {
    sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM mayor_votes WHERE election_id = $1 AND round = $2",
    )
    .bind(election_id)
    .bind(round)
    .fetch_one(pool)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))
}

async fn list_candidates(
    pool: &PgPool,
    election_id: &str,
) -> Result<Vec<MayorCandidate>, MayorError> {
    sqlx::query_as::<_, MayorCandidate>(
        r#"
        SELECT id, election_id, citizen_id, citizen_name, nominated_by, status, endorsements, created_at
        FROM mayor_candidates
        WHERE election_id = $1
        ORDER BY endorsements DESC, created_at ASC
        "#,
    )
    .bind(election_id)
    .fetch_all(pool)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))
}

async fn build_election_detail(pool: &PgPool, election_id: &str) -> Result<ElectionDetail, MayorError> {
    let election = fetch_election(pool, election_id).await?;
    let round = election.current_round;
    let candidates = list_candidates(pool, election_id).await?;
    let mut tallies = Vec::with_capacity(candidates.len());
    for c in candidates {
        let votes = if round > 0 {
            count_votes_for_candidate(pool, election_id, round, &c.id).await?
        } else {
            0
        };
        tallies.push(CandidateTally {
            candidate_id: c.id,
            citizen_id: c.citizen_id,
            citizen_name: c.citizen_name,
            status: c.status,
            endorsements: c.endorsements,
            votes,
        });
    }
    if round > 0 {
        tallies.sort_by(|a, b| b.votes.cmp(&a.votes).then(a.citizen_name.cmp(&b.citizen_name)));
    }
    let total_votes = if round > 0 {
        count_votes_round(pool, election_id, round).await?
    } else {
        0
    };
    Ok(ElectionDetail {
        election,
        candidates: tallies,
        total_votes_current_round: total_votes,
        majority_threshold: absolute_majority_threshold(total_votes),
    })
}

async fn appoint_mayor(
    state: Arc<AppState>,
    auth_name: &str,
    election_id: &str,
    candidate: &MayorCandidate,
    round: i32,
) -> Result<MayorElection, MayorError> {
    let now = Utc::now();

    sqlx::query(
        r#"
        UPDATE mayor_candidates SET status = $1 WHERE id = $2
        "#,
    )
    .bind(CandidateStatus::Winner.as_str())
    .bind(&candidate.id)
    .execute(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    sqlx::query(
        r#"
        UPDATE mayor_candidates
        SET status = $1
        WHERE election_id = $2 AND id <> $3 AND status <> $1
        "#,
    )
    .bind(CandidateStatus::Eliminated.as_str())
    .bind(election_id)
    .bind(&candidate.id)
    .execute(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    // Demote previous mayors of this city-election winner slot (global single Mayor role for now).
    sqlx::query(
        r#"
        UPDATE citizens SET role = $1
        WHERE role = $2 AND id <> $3
        "#,
    )
    .bind(Role::Citizen.as_str())
    .bind(Role::Mayor.as_str())
    .bind(&candidate.citizen_id)
    .execute(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    sqlx::query("UPDATE citizens SET role = $1 WHERE id = $2")
        .bind(Role::Mayor.as_str())
        .bind(&candidate.citizen_id)
        .execute(&state.db)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))?;

    sqlx::query(
        r#"
        UPDATE api_keys SET role = $1
        WHERE citizen_id = $2 AND is_active = TRUE
        "#,
    )
    .bind(Role::Mayor.as_str())
    .bind(&candidate.citizen_id)
    .execute(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    sqlx::query(
        r#"
        UPDATE mayor_elections
        SET status = $1, winner_id = $2, winner_name = $3, completed_at = $4
        WHERE id = $5
        "#,
    )
    .bind(ElectionStatus::Completed.as_str())
    .bind(&candidate.citizen_id)
    .bind(&candidate.citizen_name)
    .bind(now)
    .bind(election_id)
    .execute(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    let vote_id = format!("{election_id}_r{round}_result");
    let event_id = format!("election_vote_result_{vote_id}");
    let event = build_signed_mayor_election_vote_result_event(
        &event_id,
        election_id,
        &vote_id,
        round,
        "winner",
        Some(&candidate.id),
        Some(&candidate.citizen_id),
        Some(&candidate.citizen_name),
        &[],
        auth_name,
        now.timestamp(),
    );
    submit_mayor_pending_event(state.clone(), event).await?;

    tracing::info!(
        election = %election_id,
        mayor = %candidate.citizen_name,
        "mayor elected"
    );

    fetch_election(&state.db, election_id).await
}

pub async fn create_election(
    state: Arc<AppState>,
    auth: &AuthContext,
    req: CreateElectionRequest,
) -> Result<MayorElection, MayorError> {
    if !auth.can_manage_mayor_elections() {
        return Err(MayorError::forbidden(
            "Only Founder, Aiya, Mayor or Guardian can open mayor elections",
        ));
    }

    let city_name = req.city_name.trim().to_string();
    if city_name.is_empty() {
        return Err(MayorError::bad_request("city_name is required"));
    }

    let (creator_id, _, _) = resolve_auth_citizen(auth, &state.db).await?;

    let open: Option<String> = sqlx::query_scalar(
        r#"
        SELECT id FROM mayor_elections
        WHERE city_name = $1 AND status IN ('Nominating', 'Round1', 'Round2')
        LIMIT 1
        "#,
    )
    .bind(&city_name)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    if open.is_some() {
        return Err(MayorError::bad_request(
            "Для этого города уже идут выборы мэра",
        ));
    }

    let id = Uuid::new_v4().to_string();
    let now = Utc::now();
    sqlx::query(
        r#"
        INSERT INTO mayor_elections (id, city_name, status, current_round, created_by, created_at)
        VALUES ($1, $2, $3, 0, $4, $5)
        "#,
    )
    .bind(&id)
    .bind(&city_name)
    .bind(ElectionStatus::Nominating.as_str())
    .bind(&creator_id)
    .bind(now)
    .execute(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    let event_id = format!("election_announced_{id}");
    let event = build_signed_mayor_election_announced_event(
        &event_id,
        &id,
        &city_name,
        &creator_id,
        &auth.citizen_name,
        now.timestamp(),
    );
    submit_mayor_pending_event(state.clone(), event).await?;

    fetch_election(&state.db, &id).await
}

pub async fn nominate_candidate(
    state: Arc<AppState>,
    auth: &AuthContext,
    req: NominateRequest,
) -> Result<MayorCandidate, MayorError> {
    let (nominator_id, _, _) = resolve_auth_citizen(auth, &state.db).await?;
    let election = fetch_election(&state.db, &req.election_id).await?;
    if election.status != ElectionStatus::Nominating.as_str() {
        return Err(MayorError::bad_request(
            "Номинации принимаются только в статусе Nominating",
        ));
    }

    let (citizen_id, citizen_name) = load_active_citizen(&state.db, &req.candidate_id).await?;
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();

    let inserted = sqlx::query(
        r#"
        INSERT INTO mayor_candidates (
            id, election_id, citizen_id, citizen_name, nominated_by, status, endorsements, created_at
        ) VALUES ($1, $2, $3, $4, $5, $6, 0, $7)
        ON CONFLICT (election_id, citizen_id) DO NOTHING
        "#,
    )
    .bind(&id)
    .bind(&req.election_id)
    .bind(&citizen_id)
    .bind(&citizen_name)
    .bind(&nominator_id)
    .bind(CandidateStatus::Nominated.as_str())
    .bind(now)
    .execute(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    if inserted.rows_affected() == 0 {
        return Err(MayorError::bad_request(
            "Этот гражданин уже номинирован на этих выборах",
        ));
    }

    let event_id = format!("election_candidate_{id}");
    let event = build_signed_mayor_election_candidate_event(
        &event_id,
        &req.election_id,
        &id,
        &citizen_id,
        &citizen_name,
        &nominator_id,
        &auth.citizen_name,
        now.timestamp(),
    );
    submit_mayor_pending_event(state.clone(), event).await?;

    sqlx::query_as::<_, MayorCandidate>(
        r#"
        SELECT id, election_id, citizen_id, citizen_name, nominated_by, status, endorsements, created_at
        FROM mayor_candidates WHERE id = $1
        "#,
    )
    .bind(&id)
    .fetch_one(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))
}

pub async fn endorse_candidate(
    state: Arc<AppState>,
    auth: &AuthContext,
    req: EndorseRequest,
) -> Result<MayorCandidate, MayorError> {
    let (endorser_id, _, _) = resolve_auth_citizen(auth, &state.db).await?;

    let candidate = sqlx::query_as::<_, MayorCandidate>(
        r#"
        SELECT id, election_id, citizen_id, citizen_name, nominated_by, status, endorsements, created_at
        FROM mayor_candidates WHERE id = $1
        "#,
    )
    .bind(&req.candidate_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?
    .ok_or_else(|| MayorError::not_found("Кандидат не найден"))?;

    let election = fetch_election(&state.db, &candidate.election_id).await?;
    if election.status != ElectionStatus::Nominating.as_str() {
        return Err(MayorError::bad_request(
            "Поддержка принимается только на этапе Nominating",
        ));
    }

    if candidate.citizen_id == endorser_id {
        return Err(MayorError::bad_request(
            "Нельзя поддерживать собственную кандидатуру",
        ));
    }

    let endorsement_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let inserted = sqlx::query(
        r#"
        INSERT INTO mayor_endorsements (id, candidate_id, citizen_id, created_at)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (candidate_id, citizen_id) DO NOTHING
        "#,
    )
    .bind(&endorsement_id)
    .bind(&req.candidate_id)
    .bind(&endorser_id)
    .bind(now)
    .execute(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    if inserted.rows_affected() == 0 {
        return Err(MayorError::bad_request("Вы уже поддержали этого кандидата"));
    }

    refresh_endorsement_count(&state.db, &req.candidate_id).await?;

    let event_id = format!("election_endorse_{endorsement_id}");
    let event = build_signed_mayor_election_endorse_event(
        &event_id,
        &candidate.election_id,
        &req.candidate_id,
        &endorsement_id,
        &endorser_id,
        &auth.citizen_name,
        now.timestamp(),
    );
    submit_mayor_pending_event(state.clone(), event).await?;

    sqlx::query_as::<_, MayorCandidate>(
        r#"
        SELECT id, election_id, citizen_id, citizen_name, nominated_by, status, endorsements, created_at
        FROM mayor_candidates WHERE id = $1
        "#,
    )
    .bind(&req.candidate_id)
    .fetch_one(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))
}

pub async fn start_round1(
    state: Arc<AppState>,
    auth: &AuthContext,
    election_id: &str,
) -> Result<MayorElection, MayorError> {
    if !auth.can_manage_mayor_elections() {
        return Err(MayorError::forbidden(
            "Only Founder, Aiya, Mayor or Guardian can start voting",
        ));
    }
    let _ = resolve_auth_citizen(auth, &state.db).await?;
    let election = fetch_election(&state.db, election_id).await?;
    if election.status != ElectionStatus::Nominating.as_str() {
        return Err(MayorError::bad_request(
            "Первый тур запускается только из статуса Nominating",
        ));
    }

    let candidates = list_candidates(&state.db, election_id).await?;
    if candidates.len() < 2 {
        return Err(MayorError::bad_request(
            "Для старта выборов нужно минимум 2 кандидата",
        ));
    }

    let now = Utc::now();
    sqlx::query(
        r#"
        UPDATE mayor_candidates SET status = $1 WHERE election_id = $2
        "#,
    )
    .bind(CandidateStatus::Qualified.as_str())
    .bind(election_id)
    .execute(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    sqlx::query(
        r#"
        UPDATE mayor_elections
        SET status = $1, current_round = 1, round1_started_at = $2
        WHERE id = $3
        "#,
    )
    .bind(ElectionStatus::Round1.as_str())
    .bind(now)
    .bind(election_id)
    .execute(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    let vote_id = format!("{election_id}_r1");
    let event_id = format!("election_vote_started_{vote_id}");
    let event = build_signed_mayor_election_vote_started_event(
        &event_id,
        election_id,
        &vote_id,
        1,
        &auth.citizen_name,
        now.timestamp(),
    );
    submit_mayor_pending_event(state.clone(), event).await?;

    fetch_election(&state.db, election_id).await
}

pub async fn cast_vote(
    state: Arc<AppState>,
    auth: &AuthContext,
    election_id: &str,
    req: VoteRequest,
) -> Result<MayorVote, MayorError> {
    if !auth.can_vote_in_election() {
        return Err(MayorError::forbidden(
            "Agent cannot vote in elections; only Citizen, Judge, Guardian, Mayor, Aiya, Founder may vote",
        ));
    }
    let (voter_id, _, _) = resolve_auth_citizen(auth, &state.db).await?;
    let election = fetch_election(&state.db, election_id).await?;
    let round = election.current_round;
    if election.status != ElectionStatus::Round1.as_str()
        && election.status != ElectionStatus::Round2.as_str()
    {
        return Err(MayorError::bad_request(
            "Голосование доступно только в Round1 или Round2",
        ));
    }

    let candidate = sqlx::query_as::<_, MayorCandidate>(
        r#"
        SELECT id, election_id, citizen_id, citizen_name, nominated_by, status, endorsements, created_at
        FROM mayor_candidates WHERE id = $1 AND election_id = $2
        "#,
    )
    .bind(&req.candidate_id)
    .bind(election_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?
    .ok_or_else(|| MayorError::not_found("Кандидат не найден на этих выборах"))?;

    if round == 1 {
        if candidate.status != CandidateStatus::Qualified.as_str()
            && candidate.status != CandidateStatus::Nominated.as_str()
        {
            return Err(MayorError::bad_request(
                "В первом туре можно голосовать только за квалифицированных кандидатов",
            ));
        }
    } else if round == 2
        && candidate.status != CandidateStatus::Advanced.as_str() {
            return Err(MayorError::bad_request(
                "Во втором туре можно голосовать только за вышедших в финал",
            ));
        }

    let vote_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let inserted = sqlx::query(
        r#"
        INSERT INTO mayor_votes (id, election_id, round, voter_id, candidate_id, created_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (election_id, round, voter_id) DO NOTHING
        "#,
    )
    .bind(&vote_id)
    .bind(election_id)
    .bind(round)
    .bind(&voter_id)
    .bind(&req.candidate_id)
    .bind(now)
    .execute(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    if inserted.rows_affected() == 0 {
        return Err(MayorError::bad_request("Вы уже голосовали в этом туре"));
    }

    let voter_name: String = sqlx::query_scalar("SELECT name FROM citizens WHERE id = $1")
        .bind(&voter_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))?
        .unwrap_or_else(|| voter_id.clone());

    let event_id = format!("election_vote_{election_id}_r{round}_{voter_id}");
    let event = build_signed_mayor_election_vote_cast_event(
        &event_id,
        election_id,
        &vote_id,
        round,
        &voter_id,
        &voter_name,
        &req.candidate_id,
        &auth.citizen_name,
        now.timestamp(),
    );
    submit_mayor_pending_event(state.clone(), event).await?;

    sqlx::query_as::<_, MayorVote>(
        r#"
        SELECT id, election_id, round, voter_id, candidate_id, created_at
        FROM mayor_votes WHERE id = $1
        "#,
    )
    .bind(&vote_id)
    .fetch_one(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))
}

pub async fn finalize_round(
    state: Arc<AppState>,
    auth: &AuthContext,
    election_id: &str,
) -> Result<ElectionDetail, MayorError> {
    if !auth.can_manage_mayor_elections() {
        return Err(MayorError::forbidden(
            "Only Founder, Aiya, Mayor or Guardian can finalize a round",
        ));
    }
    let _ = resolve_auth_citizen(auth, &state.db).await?;
    let election = fetch_election(&state.db, election_id).await?;
    let round = election.current_round;

    if election.status != ElectionStatus::Round1.as_str()
        && election.status != ElectionStatus::Round2.as_str()
    {
        return Err(MayorError::bad_request(
            "Финализация доступна только для Round1 или Round2",
        ));
    }

    let total = count_votes_round(&state.db, election_id, round).await?;
    if total == 0 {
        return Err(MayorError::bad_request(
            "Нельзя завершить тур без голосов",
        ));
    }

    let mut candidates = list_candidates(&state.db, election_id).await?;
    let mut scored: Vec<(MayorCandidate, i64)> = Vec::new();
    for c in candidates.drain(..) {
        if round == 1
            && c.status != CandidateStatus::Qualified.as_str()
            && c.status != CandidateStatus::Nominated.as_str()
        {
            continue;
        }
        if round == 2 && c.status != CandidateStatus::Advanced.as_str() {
            continue;
        }
        let votes = count_votes_for_candidate(&state.db, election_id, round, &c.id).await?;
        scored.push((c, votes));
    }
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.citizen_name.cmp(&b.0.citizen_name)));

    if scored.is_empty() {
        return Err(MayorError::bad_request("Нет кандидатов для подведения итогов"));
    }

    if round == 1 {
        let (leader, leader_votes) = &scored[0];
        if has_first_round_majority(*leader_votes, total) {
            appoint_mayor(state.clone(), &auth.citizen_name, election_id, leader, round).await?;
            return build_election_detail(&state.db, election_id).await;
        }

        if scored.len() < 2 {
            // Single candidate leftover — appoint.
            appoint_mayor(state.clone(), &auth.citizen_name, election_id, leader, round).await?;
            return build_election_detail(&state.db, election_id).await;
        }

        let top2: Vec<String> = vec![scored[0].0.id.clone(), scored[1].0.id.clone()];
        sqlx::query(
            r#"
            UPDATE mayor_candidates
            SET status = CASE
                WHEN id = ANY($1) THEN $2
                ELSE $3
            END
            WHERE election_id = $4
            "#,
        )
        .bind(&top2)
        .bind(CandidateStatus::Advanced.as_str())
        .bind(CandidateStatus::Eliminated.as_str())
        .bind(election_id)
        .execute(&state.db)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))?;

        let now = Utc::now();
        sqlx::query(
            r#"
            UPDATE mayor_elections
            SET status = $1, current_round = 2, round2_started_at = $2
            WHERE id = $3
            "#,
        )
        .bind(ElectionStatus::Round2.as_str())
        .bind(now)
        .bind(election_id)
        .execute(&state.db)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))?;

        let vote_id = format!("{election_id}_r{round}_result");
        let event_id = format!("election_vote_result_{vote_id}");
        let event = build_signed_mayor_election_vote_result_event(
            &event_id,
            election_id,
            &vote_id,
            round,
            "runoff",
            None,
            None,
            None,
            &top2,
            &auth.citizen_name,
            now.timestamp(),
        );
        submit_mayor_pending_event(state.clone(), event).await?;

        return build_election_detail(&state.db, election_id).await;
    }

    // Round 2 — plurality (ties broken by name order already applied).
    appoint_mayor(
        state.clone(),
        &auth.citizen_name,
        election_id,
        &scored[0].0,
        round,
    )
    .await?;
    build_election_detail(&state.db, election_id).await
}

pub async fn list_elections(
    pool: &PgPool,
    query: ListElectionsQuery,
) -> Result<Vec<MayorElection>, MayorError> {
    let mut sql = String::from(
        r#"
        SELECT id, city_name, status, current_round, winner_id, winner_name,
               created_by, created_at, round1_started_at, round2_started_at, completed_at
        FROM mayor_elections WHERE 1=1
        "#,
    );
    let mut binds: Vec<String> = Vec::new();

    if let Some(status) = query.status.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if ElectionStatus::from_str(status).is_none() {
            return Err(MayorError::bad_request("Invalid election status filter"));
        }
        binds.push(status.to_string());
        sql.push_str(&format!(" AND status = ${}", binds.len()));
    }
    if let Some(city) = query
        .city_name
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        binds.push(city.to_string());
        sql.push_str(&format!(" AND city_name = ${}", binds.len()));
    }
    sql.push_str(" ORDER BY created_at DESC");

    let mut q = sqlx::query_as::<_, MayorElection>(sqlx::AssertSqlSafe(sql));
    for b in &binds {
        q = q.bind(b);
    }
    q.fetch_all(pool)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))
}

const MAYOR_APPLICATION_ACTIVE_STATUSES: &[&str] = &["pending", "approved", "confirmed"];

/// How often the background task scans for expired pending mayor applications.
pub const MAYOR_APPLICATION_EXPIRY_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(5 * 60);

#[derive(Debug, Default, Clone, Copy)]
pub struct ExpireMayorApplicationsResult {
    pub applications_timed_out: u64,
    pub citizens_role_reset: u64,
}

/// Restore role after a mayor node application times out.
///
/// - Mayor (re-application) → keep Mayor
/// - Candidate (citizenship) → keep Candidate
/// - Mayor candidate / Citizen / anything else → Citizen
pub fn restored_role_after_mayor_application_timeout(previous_role: &str) -> &'static str {
    match previous_role {
        r if r == Role::Mayor.as_str() => Role::Mayor.as_str(),
        r if r == Role::Candidate.as_str() => Role::Candidate.as_str(),
        _ => Role::Citizen.as_str(),
    }
}

#[derive(Debug, sqlx::FromRow)]
struct ExpiredMayorApplicationRow {
    id: Uuid,
    citizen_id: String,
    previous_role: String,
}

/// Mark pending mayor applications past `expires_at` as `timed_out`, restore citizen role
/// per [`restored_role_after_mayor_application_timeout`], and emit `MayorApplicationTimedOut`.
pub async fn expire_pending_mayor_applications(
    state: Arc<AppState>,
) -> Result<ExpireMayorApplicationsResult, sqlx::Error> {
    let rows = sqlx::query_as::<_, ExpiredMayorApplicationRow>(
        r#"
        SELECT a.id, a.citizen_id, c.role AS previous_role
        FROM mayor_applications a
        JOIN citizens c ON c.id = a.citizen_id
        WHERE a.status = 'pending' AND a.expires_at < NOW()
        ORDER BY a.expires_at ASC
        "#,
    )
    .fetch_all(&state.db)
    .await?;

    let mut result = ExpireMayorApplicationsResult::default();
    let timestamp = Utc::now().timestamp();

    for row in rows {
        let restored = restored_role_after_mayor_application_timeout(&row.previous_role);
        let previous_status = row.previous_role.clone();

        let updated = sqlx::query(
            r#"
            UPDATE mayor_applications
            SET status = 'timed_out'
            WHERE id = $1 AND status = 'pending'
            "#,
        )
        .bind(row.id)
        .execute(&state.db)
        .await?;

        if updated.rows_affected() == 0 {
            continue;
        }
        result.applications_timed_out += 1;

        if previous_status != restored {
            let role_upd = sqlx::query(
                r#"
                UPDATE citizens
                SET role = $1
                WHERE id = $2 AND role = $3
                "#,
            )
            .bind(restored)
            .bind(&row.citizen_id)
            .bind(&previous_status)
            .execute(&state.db)
            .await?;

            if role_upd.rows_affected() > 0 {
                sqlx::query(
                    r#"
                    UPDATE api_keys
                    SET role = $1
                    WHERE citizen_id = $2 AND is_active = TRUE
                    "#,
                )
                .bind(restored)
                .bind(&row.citizen_id)
                .execute(&state.db)
                .await?;
                result.citizens_role_reset += 1;
            }
        }

        tracing::info!(
            application_id = %row.id,
            citizen_id = %row.citizen_id,
            previous_status = %previous_status,
            restored_status = %restored,
            "mayor application timed out"
        );

        let event_id = format!("mayor_application_timed_out_{}", row.id);
        let event = build_signed_mayor_application_timed_out_event(
            &event_id,
            &row.id.to_string(),
            &row.citizen_id,
            &previous_status,
            restored,
            &state.node_id,
            timestamp,
        );

        if let Err(e) = submit_mayor_pending_event(state.clone(), event).await {
            tracing::warn!(
                application_id = %row.id,
                error = ?e,
                "failed to emit MayorApplicationTimedOut"
            );
        }
    }

    Ok(result)
}

pub async fn background_mayor_application_expiry(state: Arc<AppState>) {
    let mut interval = time::interval(MAYOR_APPLICATION_EXPIRY_INTERVAL);
    loop {
        interval.tick().await;
        tracing::debug!("mayor application expiry tick");
        match expire_pending_mayor_applications(state.clone()).await {
            Ok(result) if result.applications_timed_out > 0 => {
                tracing::info!(
                    expired_applications = result.applications_timed_out,
                    citizens_role_reset = result.citizens_role_reset,
                    "expired pending mayor applications"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "expire pending mayor applications failed");
            }
        }
    }
}

/// Spawn the mayor application timeout worker (every 5 minutes).
pub fn start_mayor_timeout_worker(state: Arc<AppState>) {
    tokio::spawn(async move {
        background_mayor_application_expiry(state).await;
    });
}

fn is_valid_node_ip(ip: &str) -> bool {
    let s = ip.trim();
    if s.is_empty() || s.len() > 512 {
        return false;
    }
    if s.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    s.chars().any(|c| c.is_ascii_digit())
}

async fn resolve_apply_citizen(
    auth: &AuthContext,
    pool: &PgPool,
) -> Result<String, MayorError> {
    if auth.is_node {
        return Err(MayorError::forbidden(
            "Учётные данные узла не могут подавать заявку мэра",
        ));
    }
    if auth.role.is_agent() {
        return Err(MayorError::forbidden(
            auth.insufficient_permissions_message().to_string(),
        ));
    }

    let citizen_id = auth.resolve_account_id(pool).await;
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT id, status FROM citizens WHERE id = $1")
            .bind(&citizen_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| MayorError::Database(e.to_string()))?;

    let (id, status) =
        row.ok_or_else(|| MayorError::bad_request("Гражданин не найден в реестре"))?;
    if status != "active" {
        return Err(MayorError::bad_request(format!(
            "Подать заявку может только активный гражданин (текущий статус: {status})"
        )));
    }
    Ok(id)
}

async fn mayor_application_ip_taken(pool: &PgPool, node_ip: &str) -> Result<bool, MayorError> {
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS(
            SELECT 1 FROM mayor_applications
            WHERE node_ip = $1 AND status = ANY($2)
        )",
    )
    .bind(node_ip)
    .bind(MAYOR_APPLICATION_ACTIVE_STATUSES)
    .fetch_one(pool)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;
    Ok(taken)
}

/// Pick a sync peer for a new mayor node application.
///
/// Strategy: uniform random among registry nodes with status `alive` (same as gossip / block sync),
/// excluding this node's id. If no other alive peer exists, fall back to this node
/// (`QUAZAR_NODE_ID` @ `QUAZAR_NODE_URL`) so single-node dev can still apply.
async fn select_mayor_sync_peer(state: &AppState) -> Result<String, MayorError> {
    use crate::nodes::format_peer_address;

    let peer = state
        .node_registry
        .pick_random_alive_peer_excluding(&state.node_id)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))?;

    if let Some(node) = peer {
        return Ok(format_peer_address(&node.id, &node.url));
    }

    let url = std::env::var("QUAZAR_NODE_URL")
        .unwrap_or_else(|_| "http://localhost:8080".to_string());
    Ok(format_peer_address(&state.node_id, &url))
}

async fn citizen_has_active_mayor_application(
    pool: &PgPool,
    citizen_id: &str,
) -> Result<bool, MayorError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(
            SELECT 1 FROM mayor_applications
            WHERE citizen_id = $1 AND status = ANY($2)
        )",
    )
    .bind(citizen_id)
    .bind(MAYOR_APPLICATION_ACTIVE_STATUSES)
    .fetch_one(pool)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;
    Ok(exists)
}

pub async fn apply_for_mayor_node(
    state: Arc<AppState>,
    auth: &AuthContext,
    req: MayorApplyRequest,
) -> Result<MayorApplyResponse, MayorError> {
    let citizen_id = resolve_apply_citizen(auth, &state.db).await?;

    let node_ip = req.ip.trim().to_string();
    if !is_valid_node_ip(&node_ip) {
        return Err(MayorError::bad_request(
            "Укажите корректный IP-адрес или адрес узла",
        ));
    }

    if mayor_application_ip_taken(&state.db, &node_ip).await? {
        return Err(MayorError::bad_request(
            "Этот IP уже занят другой заявкой на узел мэра",
        ));
    }

    if citizen_has_active_mayor_application(&state.db, &citizen_id).await? {
        return Err(MayorError::bad_request(
            "У вас уже есть активная заявка на узел мэра",
        ));
    }

    let application_id = Uuid::new_v4();
    let now = Utc::now();
    let expires_at = now + Duration::hours(24);
    let sync_peer = select_mayor_sync_peer(&state).await?;

    sqlx::query(
        r#"
        INSERT INTO mayor_applications (id, citizen_id, node_ip, sync_peer, status, created_at, expires_at)
        VALUES ($1, $2, $3, $4, 'pending', $5, $6)
        "#,
    )
    .bind(application_id)
    .bind(&citizen_id)
    .bind(&node_ip)
    .bind(&sync_peer)
    .bind(now)
    .bind(expires_at)
    .execute(&state.db)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(db_err) = &e {
            if db_err.constraint() == Some("idx_mayor_applications_node_ip_active")
                || db_err.constraint() == Some("idx_mayor_applications_citizen_active")
            {
                return MayorError::bad_request(
                    "Заявка не создана: IP или гражданин уже участвуют в активной заявке",
                );
            }
        }
        MayorError::Database(e.to_string())
    })?;

    Ok(MayorApplyResponse {
        application_id: application_id.to_string(),
        sync_peer,
    })
}

fn russian_hours_word(hours: i64) -> &'static str {
    let n = hours.abs();
    let last_two = n % 100;
    if (11..=14).contains(&last_two) {
        return "часов";
    }
    match n % 10 {
        1 => "час",
        2 | 3 | 4 => "часа",
        _ => "часов",
    }
}

pub fn mayor_application_status_message(
    status: &str,
    node_id: Option<&str>,
    expires_at: chrono::DateTime<Utc>,
    now: chrono::DateTime<Utc>,
) -> String {
    match status {
        "pending" => {
            let hours = (expires_at - now).num_hours().max(0);
            if hours == 0 {
                return "Узел ещё не запущен. Осталось менее часа.".to_string();
            }
            format!(
                "Узел ещё не запущен. Осталось {} {}.",
                hours,
                russian_hours_word(hours)
            )
        }
        "approved" => {
            if let Some(id) = node_id.filter(|s| !s.is_empty()) {
                format!(
                    "Узел {id} зарегистрирован. Завершите установку и дождитесь подтверждения синхронизации с сетью."
                )
            } else {
                "Узел зарегистрирован. Завершите установку и дождитесь подтверждения синхронизации с сетью."
                    .to_string()
            }
        }
        "confirmed" => {
            if let Some(id) = node_id.filter(|s| !s.is_empty()) {
                format!("Узел {id} успешно подключён к сети Quazar.")
            } else {
                "Узел мэра успешно подключён к сети Quazar.".to_string()
            }
        }
        "timed_out" => {
            "Срок действия заявки истёк. Подайте новую заявку через /api/mayor/apply.".to_string()
        }
        other => format!("Статус заявки: {other}."),
    }
}

pub async fn get_mayor_application_status(
    pool: &PgPool,
    application_id: Uuid,
) -> Result<MayorApplicationStatusResponse, MayorError> {
    let row: Option<(String, Option<String>, chrono::DateTime<Utc>)> = sqlx::query_as(
        r#"
        SELECT status, node_id, expires_at
        FROM mayor_applications
        WHERE id = $1
        "#,
    )
    .bind(application_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    let (status, node_id, expires_at) =
        row.ok_or_else(|| MayorError::not_found("Заявка на узел мэра не найдена"))?;

    let message =
        mayor_application_status_message(&status, node_id.as_deref(), expires_at, Utc::now());

    Ok(MayorApplicationStatusResponse {
        status,
        node_id,
        message,
    })
}

#[derive(Debug, sqlx::FromRow)]
struct PendingMayorApplicationRow {
    id: uuid::Uuid,
    citizen_id: String,
    sync_peer: Option<String>,
    node_id: Option<String>,
    node_secret: Option<String>,
}

fn resolve_setup_api_key(
    query: &MayorSetupPackageQuery,
    headers: &HeaderMap,
) -> Result<String, MayorError> {
    if let Some(key) = query
        .api_key
        .as_ref()
        .map(|k| k.trim())
        .filter(|k| !k.is_empty())
    {
        return Ok(key.to_string());
    }

    if let Some(auth) = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(rest) = auth.strip_prefix("Bearer ") {
            let key = rest.trim();
            if !key.is_empty() {
                return Ok(key.to_string());
            }
        }
    }

    if let Some(key) = headers
        .get("X-API-Key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty())
    {
        return Ok(key.to_string());
    }

    Err(MayorError::forbidden("Укажите api_key в query или заголовке Authorization / X-API-Key"))
}

pub async fn build_mayor_setup_package(
    state: Arc<AppState>,
    node_ip: &str,
    raw_api_key: &str,
) -> Result<MayorSetupPackageResponse, MayorError> {
    let ip = node_ip.trim();
    if !is_valid_node_ip(ip) {
        return Err(MayorError::bad_request(
            "Укажите корректный IP-адрес или адрес узла",
        ));
    }

    let key_data = KeyStore::validate_key(&state.db, raw_api_key)
        .await
        .ok_or_else(|| MayorError::forbidden("Неверный или неактивный API key"))?;

    if key_data.citizen_id.is_empty() {
        return Err(MayorError::forbidden("API key не привязан к гражданину"));
    }

    let app = sqlx::query_as::<_, PendingMayorApplicationRow>(
        r#"
        SELECT id, citizen_id, sync_peer, node_id, node_secret
        FROM mayor_applications
        WHERE node_ip = $1 AND status = 'pending'
        LIMIT 1
        "#,
    )
    .bind(ip)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?
    .ok_or_else(|| {
        MayorError::not_found("Заявка на узел мэра для этого IP не найдена или не в статусе pending")
    })?;

    if app.citizen_id != key_data.citizen_id {
        return Err(MayorError::forbidden(
            "API key не принадлежит гражданину этой заявки",
        ));
    }

    let citizen_name: String = sqlx::query_scalar("SELECT name FROM citizens WHERE id = $1")
        .bind(&app.citizen_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))?
        .ok_or_else(|| MayorError::not_found("Гражданин заявки не найден"))?;

    let mut node_id = app.node_id.clone();
    let mut node_secret = app.node_secret.clone();
    let mut sync_peer = app.sync_peer.clone();

    if node_id.as_deref().unwrap_or("").is_empty() {
        node_id = Some(
            allocate_mayor_node_id(&state.db)
                .await
                .map_err(|e| MayorError::Database(e.to_string()))?,
        );
    }
    if node_secret.as_deref().unwrap_or("").is_empty() {
        node_secret = Some(generate_node_secret_hex());
    }
    if sync_peer.as_deref().unwrap_or("").is_empty() {
        sync_peer = Some(select_mayor_sync_peer(&state).await?);
    }

    let node_id = node_id.expect("node_id set");
    let node_secret = node_secret.expect("node_secret set");
    let sync_peer = sync_peer.expect("sync_peer set");
    let peer_url = peer_url_from_sync_peer(&sync_peer);
    let now = Utc::now();

    sqlx::query(
        r#"
        UPDATE mayor_applications
        SET node_id = $1,
            node_secret = $2,
            sync_peer = $3,
            package_delivered_at = $4
        WHERE id = $5 AND status = 'pending'
        "#,
    )
    .bind(&node_id)
    .bind(&node_secret)
    .bind(&sync_peer)
    .bind(now)
    .bind(app.id)
    .execute(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    let network_id = resolve_network_id(&state.db)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))?;
    let version = resolve_network_version(&state.db)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))?;

    Ok(MayorSetupPackageResponse {
        node_id: node_id.clone(),
        node_secret: format_secret_for_package(&node_secret),
        sync_peer,
        peer_url,
        qz_id: app.citizen_id,
        citizen_name,
        network_id,
        version,
    })
}

pub async fn deliver_mayor_node_package(
    state: Arc<AppState>,
    auth: &AuthContext,
    application_id: Uuid,
) -> Result<MayorNodePackageParams, MayorError> {
    let citizen_id = resolve_apply_citizen(auth, &state.db).await?;

    let app = sqlx::query_as::<_, PendingMayorApplicationRow>(
        r#"
        SELECT id, citizen_id, sync_peer, node_id, node_secret
        FROM mayor_applications
        WHERE id = $1
        "#,
    )
    .bind(application_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?
    .ok_or_else(|| MayorError::not_found("Заявка на узел мэра не найдена"))?;

    if app.citizen_id != citizen_id {
        return Err(MayorError::forbidden(
            "Пакет установки доступен только автору заявки",
        ));
    }

    let status: String = sqlx::query_scalar("SELECT status FROM mayor_applications WHERE id = $1")
        .bind(application_id)
        .fetch_one(&state.db)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))?;

    if !matches!(status.as_str(), "pending" | "approved") {
        return Err(MayorError::bad_request(format!(
            "Пакет недоступен для статуса заявки: {status}"
        )));
    }

    let mut node_id = app.node_id.clone();
    let mut node_secret = app.node_secret.clone();
    let mut sync_peer = app.sync_peer.clone();

    if node_id.as_deref().unwrap_or("").is_empty() {
        node_id = Some(
            allocate_mayor_node_id(&state.db)
                .await
                .map_err(|e| MayorError::Database(e.to_string()))?,
        );
    }
    if node_secret.as_deref().unwrap_or("").is_empty() {
        node_secret = Some(generate_node_secret_hex());
    }
    if sync_peer.as_deref().unwrap_or("").is_empty() {
        sync_peer = Some(select_mayor_sync_peer(&state).await?);
    }

    let node_id = node_id.expect("node_id set");
    let node_secret = node_secret.expect("node_secret set");
    let sync_peer = sync_peer.expect("sync_peer set");
    let peer_url = peer_url_from_sync_peer(&sync_peer);

    if app.node_id.as_deref() != Some(node_id.as_str())
        || app.node_secret.as_deref() != Some(node_secret.as_str())
    {
        sqlx::query(
            r#"
            UPDATE mayor_applications
            SET node_id = $1,
                node_secret = $2,
                sync_peer = $3,
                package_delivered_at = COALESCE(package_delivered_at, NOW())
            WHERE id = $4
            "#,
        )
        .bind(&node_id)
        .bind(&node_secret)
        .bind(&sync_peer)
        .bind(application_id)
        .execute(&state.db)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))?;
    }

    let network_id = resolve_network_id(&state.db)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))?;
    let version = resolve_network_version(&state.db)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))?;

    Ok(MayorNodePackageParams {
        node_id,
        node_secret: format_secret_for_package(&node_secret),
        network_id,
        version,
        sync_peer,
        peer_url,
    })
}

#[derive(Debug, Deserialize)]
pub struct MayorNodeStartedRequest {
    pub node_id: String,
    pub node_secret: String,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MayorNodeStartedResponse {
    pub application_id: String,
    pub node_id: String,
    pub event_id: String,
    pub status: String,
}

fn resolve_register_node_url(provided: Option<&str>, node_ip: &str) -> String {
    if let Some(u) = provided.map(str::trim).filter(|s| !s.is_empty()) {
        return u.to_string();
    }
    let ip = node_ip.trim();
    if ip.starts_with("http://") || ip.starts_with("https://") {
        return ip.to_string();
    }
    format!("http://{ip}:8080")
}

fn network_register_base_url() -> Option<String> {
    if let Ok(u) = std::env::var("QUAZAR_NETWORK_URL") {
        let u = u.trim().trim_end_matches('/').to_string();
        if !u.is_empty() {
            return Some(u);
        }
    }
    let bootstrap = std::env::var("QUAZAR_BOOTSTRAP_PEERS").ok()?;
    for entry in bootstrap.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (_, url) = entry.split_once('@')?;
        return Some(url.trim().trim_end_matches('/').to_string());
    }
    None
}

/// STEP 7: joining mayor node registers with the network using application credentials.
pub async fn register_mayor_node_started(
    state: Arc<AppState>,
    req: MayorNodeStartedRequest,
) -> Result<MayorNodeStartedResponse, MayorError> {
    let node_id = req.node_id.trim();
    if node_id.is_empty() {
        return Err(MayorError::bad_request("node_id обязателен"));
    }
    if req.node_secret.trim().is_empty() {
        return Err(MayorError::bad_request("node_secret обязателен"));
    }

    let app = sqlx::query_as::<_, MayorConfirmApplicationRow>(
        r#"
        SELECT id, citizen_id, node_ip, node_id, node_secret, status, package_delivered_at
        FROM mayor_applications
        WHERE node_id = $1
        LIMIT 1
        "#,
    )
    .bind(node_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?
    .ok_or_else(|| MayorError::not_found("Заявка для этого node_id не найдена"))?;

    let stored_secret = app
        .node_secret
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| MayorError::bad_request("У заявки не задан node_secret"))?;

    if !node_secrets_match(stored_secret, &req.node_secret) {
        return Err(MayorError::forbidden("Неверный node_secret"));
    }

    if app.package_delivered_at.is_none() {
        return Err(MayorError::bad_request(
            "Пакет установки ещё не выдан (package_delivered_at)",
        ));
    }

    match app.status.as_str() {
        "pending" | "approved" => {}
        "confirmed" => {
            return Err(MayorError::bad_request(
                "Заявка уже подтверждена (confirmed)",
            ));
        }
        "timed_out" => {
            return Err(MayorError::bad_request("Срок заявки истёк"));
        }
        other => {
            return Err(MayorError::bad_request(format!(
                "Регистрация недоступна для статуса заявки: {other}"
            )));
        }
    }

    let citizen_name: String = sqlx::query_scalar("SELECT name FROM citizens WHERE id = $1")
        .bind(&app.citizen_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))?
        .ok_or_else(|| MayorError::not_found("Гражданин заявки не найден"))?;

    let node_url = resolve_register_node_url(req.url.as_deref(), &app.node_ip);
    let version = resolve_network_version(&state.db)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))?;
    let now = Utc::now();

    let peer = Node {
        id: node_id.to_string(),
        url: node_url.clone(),
        public_key: None,
        status: NodeStatus::Alive,
        last_seen: now,
        version: version.clone(),
    };
    state
        .node_registry
        .upsert_node(&peer)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))?;

    sqlx::query(
        r#"
        UPDATE mayor_applications
        SET status = 'approved'
        WHERE id = $1 AND status IN ('pending', 'approved')
        "#,
    )
    .bind(app.id)
    .execute(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    let event_id = format!("node_started_{}_{}", node_id, now.timestamp());
    let event = build_signed_node_started_event(
        &event_id,
        &app.id.to_string(),
        &app.citizen_id,
        &citizen_name,
        node_id,
        &node_url,
        &version,
        &state.node_id,
        now.timestamp(),
    );

    submit_mayor_pending_event(state.clone(), event).await?;

    Ok(MayorNodeStartedResponse {
        application_id: app.id.to_string(),
        node_id: node_id.to_string(),
        event_id,
        status: "approved".to_string(),
    })
}

pub async fn mayor_node_started_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<MayorNodeStartedRequest>,
) -> impl IntoResponse {
    match register_mayor_node_started(state, req).await {
        Ok(resp) => Json(ApiResponse::success(resp)).into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

/// On startup, a new mayor node POSTs to the bootstrap network URL to register (STEP 7).
pub async fn register_mayor_node_with_network_on_startup(state: Arc<AppState>) {
    if state.node_id == "QZ-NODE" {
        return;
    }

    let Some(base) = network_register_base_url() else {
        return;
    };

    let secret = match std::env::var("QUAZAR_NODE_SECRET") {
        Ok(s) if !s.trim().is_empty() => s,
        _ => return,
    };

    let node_url = std::env::var("QUAZAR_NODE_URL").unwrap_or_else(|_| {
        resolve_register_node_url(None, "127.0.0.1")
    });

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let url = format!("{base}/api/mayor/node-started");
    let body = serde_json::json!({
        "node_id": state.node_id,
        "node_secret": secret,
        "url": node_url,
    });

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "mayor node-started startup: http client");
            return;
        }
    };

    match client.post(&url).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!(url = %url, node_id = %state.node_id, "mayor node registered with network");
        }
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!(http = %status, body = %text, url = %url, "mayor node-started request failed");
        }
        Err(e) => tracing::warn!(error = %e, url = %url, "mayor node-started request error"),
    }
}

#[derive(Debug, Deserialize)]
pub struct MayorConfirmRequest {
    pub node_id: String,
    pub node_secret: String,
}

#[derive(Debug, Serialize)]
pub struct MayorConfirmResponse {
    pub application_id: String,
    pub event_id: String,
    pub status: String,
}

fn normalize_node_secret(secret: &str) -> String {
    let s = secret.trim();
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    s.to_lowercase()
}

fn node_secrets_match(stored: &str, provided: &str) -> bool {
    normalize_node_secret(stored) == normalize_node_secret(provided)
}

#[derive(Debug, sqlx::FromRow)]
struct MayorConfirmApplicationRow {
    id: Uuid,
    citizen_id: String,
    node_ip: String,
    node_id: Option<String>,
    node_secret: Option<String>,
    status: String,
    package_delivered_at: Option<chrono::DateTime<Utc>>,
}

async fn network_block_count(pool: &PgPool) -> Result<i64, MayorError> {
    sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM blocks")
        .fetch_one(pool)
        .await
        .map(|n: i64| n)
        .map_err(|e| MayorError::Database(e.to_string()))
}

async fn fetch_peer_block_count(peer_url: &str) -> Result<i64, MayorError> {
    let base = peer_url.trim().trim_end_matches('/');
    let url = format!("{base}/status");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| MayorError::Database(e.to_string()))?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| MayorError::bad_request(format!("Не удалось опросить узел ({url}): {e}")))?;

    if !resp.status().is_success() {
        return Err(MayorError::bad_request(format!(
            "Узел вернул HTTP {} на /status",
            resp.status()
        )));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| MayorError::bad_request(format!("Некорректный ответ /status: {e}")))?;

    let blocks = body
        .get("data")
        .and_then(|d| d.get("blocks"))
        .and_then(|v| v.as_i64())
        .or_else(|| body.get("blocks").and_then(|v| v.as_i64()))
        .ok_or_else(|| MayorError::bad_request("Ответ /status не содержит поле blocks"))?;

    Ok(blocks)
}

/// Verifies the mayor node is registered, package was delivered, and chain height matches the network.
pub async fn verify_mayor_node_synced(
    state: &AppState,
    app: &MayorConfirmApplicationRow,
) -> Result<(), MayorError> {
    if app.package_delivered_at.is_none() {
        return Err(MayorError::bad_request(
            "Пакет установки ещё не выдан (package_delivered_at)",
        ));
    }

    let node_id = app
        .node_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| MayorError::bad_request("У заявки не назначен node_id"))?;

    let network_blocks = network_block_count(&state.db).await?;

    let node_row: Option<(String, String, chrono::DateTime<Utc>)> = sqlx::query_as(
        "SELECT url, status, last_seen FROM nodes WHERE id = $1",
    )
    .bind(node_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    let (node_url, node_status, last_seen) = node_row.ok_or_else(|| {
        MayorError::bad_request("Узел ещё не зарегистрирован в сети (ожидается NodeStarted)")
    })?;

    if node_status != "alive" {
        return Err(MayorError::bad_request(format!(
            "Узел {node_id} не в статусе alive (текущий: {node_status})"
        )));
    }

    let stale_cutoff = Utc::now() - chrono::Duration::minutes(15);
    if last_seen < stale_cutoff {
        return Err(MayorError::bad_request(
            "Узел не отмечен как недавно активный (last_seen > 15 мин)",
        ));
    }

    let peer_blocks = if state.node_id == node_id {
        network_blocks
    } else {
        let url = if node_url.trim().is_empty() {
            format!("http://{}:8080", app.node_ip.trim())
        } else {
            node_url
        };
        fetch_peer_block_count(&url).await?
    };

    if network_blocks > 0 && peer_blocks < network_blocks {
        return Err(MayorError::bad_request(format!(
            "Узел не синхронизирован: blocks={peer_blocks}, ожидается >= {network_blocks}"
        )));
    }

    Ok(())
}

async fn submit_mayor_pending_event(
    state: Arc<AppState>,
    event: Event,
) -> Result<(), MayorError> {
    EventValidator::validate_event(&event, &state.db)
        .await
        .map_err(|e| MayorError::bad_request(e.message()))?;

    match pending::insert(&state.db, &event).await {
        Ok(pending::PendingInsertResult::Inserted) => {}
        Ok(pending::PendingInsertResult::AlreadyExists) => return Ok(()),
        Err(e) => return Err(MayorError::Database(e.to_string())),
    }

    let state_gossip = state.clone();
    let event_gossip = event.clone();
    tokio::spawn(async move {
        gossip::push_pending_to_peers(&state_gossip, &event_gossip).await;
    });

    let state_clone = state.clone();
    tokio::spawn(async move {
        block_producer::try_create_block(state_clone).await;
    });

    Ok(())
}

/// Confirms mayorship after STEP 7 register + sync. Bypasses election/candidacy via `MayorConfirmed`.
pub async fn confirm_mayor_node(
    state: Arc<AppState>,
    node_id: &str,
    node_secret: &str,
) -> Result<MayorConfirmResponse, MayorError> {
    let node_id = node_id.trim();
    if node_id.is_empty() {
        return Err(MayorError::bad_request("node_id обязателен"));
    }
    if node_secret.trim().is_empty() {
        return Err(MayorError::bad_request("node_secret обязателен"));
    }

    let app = sqlx::query_as::<_, MayorConfirmApplicationRow>(
        r#"
        SELECT id, citizen_id, node_ip, node_id, node_secret, status, package_delivered_at
        FROM mayor_applications
        WHERE node_id = $1
        LIMIT 1
        "#,
    )
    .bind(node_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?
    .ok_or_else(|| MayorError::not_found("Заявка для этого node_id не найдена"))?;

    let stored_secret = app
        .node_secret
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| MayorError::bad_request("У заявки не задан node_secret"))?;

    if !node_secrets_match(stored_secret, node_secret) {
        return Err(MayorError::forbidden("Неверный node_secret"));
    }

    if app.status == "confirmed" {
        return Ok(MayorConfirmResponse {
            application_id: app.id.to_string(),
            event_id: format!("mayor_confirmed_{}", app.id),
            status: "confirmed".to_string(),
        });
    }

    if app.status != "approved" {
        return Err(MayorError::bad_request(format!(
            "Подтверждение доступно только после регистрации узла (статус approved, сейчас: {})",
            app.status
        )));
    }

    verify_mayor_node_synced(&state, &app).await?;

    let citizen_name: String = sqlx::query_scalar("SELECT name FROM citizens WHERE id = $1")
        .bind(&app.citizen_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))?
        .ok_or_else(|| MayorError::not_found("Гражданин заявки не найден"))?;

    let event_id = format!("mayor_confirmed_{}", app.id);
    let timestamp = Utc::now().timestamp();
    let event = build_signed_mayor_confirmed_event(
        &event_id,
        &app.id.to_string(),
        &app.citizen_id,
        &citizen_name,
        node_id,
        &state.node_id,
        timestamp,
    );

    submit_mayor_pending_event(state, event).await?;

    Ok(MayorConfirmResponse {
        application_id: app.id.to_string(),
        event_id,
        status: "pending_confirmation".to_string(),
    })
}

/// Called after STEP 7 `NodeStarted` on the coordinating node (same checks, no secret re-check).
pub async fn try_confirm_mayor_after_node_started(
    state: Arc<AppState>,
    node_id: &str,
) -> Result<Option<MayorConfirmResponse>, MayorError> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT node_secret, status FROM mayor_applications WHERE node_id = $1",
    )
    .bind(node_id.trim())
    .fetch_optional(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    let Some((secret, status)) = row else {
        return Ok(None);
    };

    if status == "confirmed" {
        return Ok(None);
    }
    if status != "approved" {
        return Ok(None);
    }

    let resp = confirm_mayor_node(state, node_id, &secret).await?;
    Ok(Some(resp))
}

pub async fn mayor_confirm_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<MayorConfirmRequest>,
) -> impl IntoResponse {
    match confirm_mayor_node(state, &req.node_id, &req.node_secret).await {
        Ok(resp) => Json(ApiResponse::success(resp)).into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

/// New mayor nodes request confirmation from bootstrap after background sync.
pub async fn maybe_request_mayor_confirmation_on_startup(state: Arc<AppState>) {
    if std::env::var("QUAZAR_BOOTSTRAP_PEERS")
        .map(|v| v.trim().is_empty())
        .unwrap_or(true)
    {
        return;
    }

    let node_id = state.node_id.clone();
    let secret = match std::env::var("QUAZAR_NODE_SECRET") {
        Ok(s) if !s.trim().is_empty() => s,
        _ => return,
    };

    let status: Option<String> = sqlx::query_scalar(
        "SELECT status FROM mayor_applications WHERE node_id = $1",
    )
    .bind(&node_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    if status.as_deref() != Some("approved") {
        return;
    }

    let policy = block_producer::BlockPolicy::from_env();
    let wait_secs = policy.sync_interval_secs.saturating_mul(2).max(30);
    tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;

    let bootstrap = std::env::var("QUAZAR_BOOTSTRAP_PEERS").unwrap_or_default();
    let Some(first) = bootstrap.split(',').map(str::trim).find(|s| !s.is_empty()) else {
        return;
    };
    let Some((_peer_id, peer_url)) = first.split_once('@') else {
        tracing::warn!(entry = %first, "cannot request mayor confirm: invalid bootstrap format");
        return;
    };

    let url = format!(
        "{}/api/mayor/confirm",
        peer_url.trim().trim_end_matches('/')
    );
    let body = serde_json::json!({
        "node_id": node_id,
        "node_secret": secret,
    });

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "mayor confirm startup: http client");
            return;
        }
    };

    match client.post(&url).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!(url = %url, "mayor confirmation requested from bootstrap");
        }
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!(http = %status, body = %text, "mayor confirm request failed");
        }
        Err(e) => tracing::warn!(error = %e, url = %url, "mayor confirm request error"),
    }
}

pub async fn mayor_setup_package_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<MayorSetupPackageQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let api_key = match resolve_setup_api_key(&query, &headers) {
        Ok(k) => k,
        Err(e) => return e.to_response().into_response(),
    };
    match build_mayor_setup_package(state, &query.ip, &api_key).await {
        Ok(pkg) => Json(ApiResponse::success(pkg)).into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

// --- HTTP handlers (elections) ---

pub async fn mayor_apply_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<MayorApplyRequest>,
) -> impl IntoResponse {
    match apply_for_mayor_node(state, &auth, req).await {
        Ok(resp) => (StatusCode::CREATED, Json(ApiResponse::success(resp))).into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

pub async fn mayor_application_status_handler(
    State(state): State<Arc<AppState>>,
    Path(application_id): Path<String>,
) -> impl IntoResponse {
    let application_id = match Uuid::parse_str(application_id.trim()) {
        Ok(id) => id,
        Err(_) => {
            return MayorError::bad_request("Некорректный идентификатор заявки")
                .to_response()
                .into_response();
        }
    };

    match get_mayor_application_status(&state.db, application_id).await {
        Ok(resp) => Json(ApiResponse::success(resp)).into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

pub async fn mayor_package_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let application_id = match Uuid::parse_str(id.trim()) {
        Ok(u) => u,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::error("Некорректный идентификатор заявки")),
            )
                .into_response();
        }
    };
    match deliver_mayor_node_package(state, &auth, application_id).await {
        Ok(params) => Json(ApiResponse::success(params)).into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

pub async fn create_election_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateElectionRequest>,
) -> impl IntoResponse {
    if auth.is_node {
        return response::forbidden("Node credentials cannot open mayor elections");
    }
    match create_election(state, &auth, req).await {
        Ok(row) => (StatusCode::CREATED, Json(ApiResponse::success(row))).into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

pub async fn nominate_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<NominateRequest>,
) -> impl IntoResponse {
    match nominate_candidate(state, &auth, req).await {
        Ok(row) => (StatusCode::CREATED, Json(ApiResponse::success(row))).into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

pub async fn endorse_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<EndorseRequest>,
) -> impl IntoResponse {
    match endorse_candidate(state, &auth, req).await {
        Ok(row) => Json(ApiResponse::success(row)).into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

pub async fn start_election_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match start_round1(state, &auth, &id).await {
        Ok(row) => Json(ApiResponse::success(row)).into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

pub async fn vote_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<VoteRequest>,
) -> impl IntoResponse {
    match cast_vote(state, &auth, &id, req).await {
        Ok(row) => (StatusCode::CREATED, Json(ApiResponse::success(row))).into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

pub async fn finalize_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match finalize_round(state, &auth, &id).await {
        Ok(detail) => Json(ApiResponse::success(detail)).into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

pub async fn get_election_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match build_election_detail(&state.db, &id).await {
        Ok(detail) => Json(ApiResponse::success(detail)).into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

pub async fn list_elections_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListElectionsQuery>,
) -> impl IntoResponse {
    match list_elections(&state.db, query).await {
        Ok(rows) => Json(ApiResponse::success(serde_json::json!({
            "elections": rows,
            "total": rows.len(),
        })))
        .into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn absolute_majority_is_fifty_percent_plus_one() {
        assert_eq!(absolute_majority_threshold(0), 1);
        assert_eq!(absolute_majority_threshold(1), 1);
        assert_eq!(absolute_majority_threshold(2), 2);
        assert_eq!(absolute_majority_threshold(3), 2);
        assert_eq!(absolute_majority_threshold(100), 51);
        assert_eq!(absolute_majority_threshold(101), 51);
    }

    #[test]
    fn first_round_majority_detection() {
        assert!(has_first_round_majority(51, 100));
        assert!(!has_first_round_majority(50, 100));
        assert!(has_first_round_majority(2, 3));
        assert!(!has_first_round_majority(1, 3));
        assert!(!has_first_round_majority(0, 0));
    }

    #[test]
    fn election_status_roundtrip() {
        for s in [
            ElectionStatus::Nominating,
            ElectionStatus::Round1,
            ElectionStatus::Round2,
            ElectionStatus::Completed,
            ElectionStatus::Cancelled,
        ] {
            assert_eq!(ElectionStatus::from_str(s.as_str()), Some(s));
        }
        assert!(ElectionStatus::from_str("Open").is_none());
    }

    #[test]
    fn runoff_needed_when_no_majority() {
        // 40, 35, 25 of 100 — leader has 40 < 51 → runoff between top two
        assert!(!has_first_round_majority(40, 100));
        assert!(has_first_round_majority(60, 100));
    }

    #[test]
    fn agent_cannot_vote_in_election() {
        let agent = AuthContext::from_api_key("bot".into(), Role::Agent);
        assert!(!agent.can_vote_in_election());
        assert!(!agent.can_vote());
        let citizen = AuthContext::from_api_key("alice".into(), Role::Citizen);
        assert!(citizen.can_vote_in_election());
    }

    #[test]
    fn node_secret_normalization_matches_install_script() {
        assert!(node_secrets_match("0xAbCd", "abcd"));
        assert!(node_secrets_match("0xABCD", "0xabcd"));
        assert!(!node_secrets_match("abcd", "abce"));
    }

    #[test]
    fn mayor_application_status_messages() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let expires = now + Duration::hours(23);
        let msg = mayor_application_status_message("pending", None, expires, now);
        assert!(msg.contains("23"));
        assert!(msg.contains("Узел ещё не запущен"));

        let approved = mayor_application_status_message(
            "approved",
            Some("QZ-NODE-3"),
            expires,
            now,
        );
        assert!(approved.contains("QZ-NODE-3"));
        assert!(approved.contains("синхронизации"));

        let confirmed =
            mayor_application_status_message("confirmed", Some("QZ-NODE-3"), expires, now);
        assert!(confirmed.contains("успешно"));

        let timed_out = mayor_application_status_message("timed_out", None, expires, now);
        assert!(timed_out.contains("истёк"));
    }

    #[test]
    fn restored_role_after_timeout_keeps_mayor_and_candidate() {
        assert_eq!(
            restored_role_after_mayor_application_timeout("Mayor"),
            "Mayor"
        );
        assert_eq!(
            restored_role_after_mayor_application_timeout("Candidate"),
            "Candidate"
        );
        assert_eq!(
            restored_role_after_mayor_application_timeout("Citizen"),
            "Citizen"
        );
        // «Кандидат в мэры» / прочие роли → Гражданин
        assert_eq!(
            restored_role_after_mayor_application_timeout("Guardian"),
            "Citizen"
        );
        assert_eq!(
            restored_role_after_mayor_application_timeout("Agent"),
            "Citizen"
        );
        assert_eq!(
            restored_role_after_mayor_application_timeout(""),
            "Citizen"
        );
    }
}
