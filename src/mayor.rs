use std::sync::Arc;

use axum::{
    extract::{Extension, Path, Query, State},
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
use crate::types::Role;
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

    fn to_response(self) -> (StatusCode, Json<ApiResponse>) {
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
    pool: &PgPool,
    election_id: &str,
    candidate: &MayorCandidate,
) -> Result<MayorElection, MayorError> {
    let now = Utc::now();

    sqlx::query(
        r#"
        UPDATE mayor_candidates SET status = $1 WHERE id = $2
        "#,
    )
    .bind(CandidateStatus::Winner.as_str())
    .bind(&candidate.id)
    .execute(pool)
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
    .execute(pool)
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
    .execute(pool)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    sqlx::query("UPDATE citizens SET role = $1 WHERE id = $2")
        .bind(Role::Mayor.as_str())
        .bind(&candidate.citizen_id)
        .execute(pool)
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
    .execute(pool)
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
    .execute(pool)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    tracing::info!(
        election = %election_id,
        mayor = %candidate.citizen_name,
        "mayor elected"
    );

    fetch_election(pool, election_id).await
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
    sqlx::query(
        r#"
        INSERT INTO mayor_elections (id, city_name, status, current_round, created_by)
        VALUES ($1, $2, $3, 0, $4)
        "#,
    )
    .bind(&id)
    .bind(&city_name)
    .bind(ElectionStatus::Nominating.as_str())
    .bind(&creator_id)
    .execute(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

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

    let inserted = sqlx::query(
        r#"
        INSERT INTO mayor_candidates (
            id, election_id, citizen_id, citizen_name, nominated_by, status, endorsements
        ) VALUES ($1, $2, $3, $4, $5, $6, 0)
        ON CONFLICT (election_id, citizen_id) DO NOTHING
        "#,
    )
    .bind(&id)
    .bind(&req.election_id)
    .bind(&citizen_id)
    .bind(&citizen_name)
    .bind(&nominator_id)
    .bind(CandidateStatus::Nominated.as_str())
    .execute(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    if inserted.rows_affected() == 0 {
        return Err(MayorError::bad_request(
            "Этот гражданин уже номинирован на этих выборах",
        ));
    }

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
    let inserted = sqlx::query(
        r#"
        INSERT INTO mayor_endorsements (id, candidate_id, citizen_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (candidate_id, citizen_id) DO NOTHING
        "#,
    )
    .bind(&endorsement_id)
    .bind(&req.candidate_id)
    .bind(&endorser_id)
    .execute(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    if inserted.rows_affected() == 0 {
        return Err(MayorError::bad_request("Вы уже поддержали этого кандидата"));
    }

    refresh_endorsement_count(&state.db, &req.candidate_id).await?;

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
    } else if round == 2 {
        if candidate.status != CandidateStatus::Advanced.as_str() {
            return Err(MayorError::bad_request(
                "Во втором туре можно голосовать только за вышедших в финал",
            ));
        }
    }

    let vote_id = Uuid::new_v4().to_string();
    let inserted = sqlx::query(
        r#"
        INSERT INTO mayor_votes (id, election_id, round, voter_id, candidate_id)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (election_id, round, voter_id) DO NOTHING
        "#,
    )
    .bind(&vote_id)
    .bind(election_id)
    .bind(round)
    .bind(&voter_id)
    .bind(&req.candidate_id)
    .execute(&state.db)
    .await
    .map_err(|e| MayorError::Database(e.to_string()))?;

    if inserted.rows_affected() == 0 {
        return Err(MayorError::bad_request("Вы уже голосовали в этом туре"));
    }

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
            appoint_mayor(&state.db, election_id, leader).await?;
            return build_election_detail(&state.db, election_id).await;
        }

        if scored.len() < 2 {
            // Single candidate leftover — appoint.
            appoint_mayor(&state.db, election_id, leader).await?;
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

        return build_election_detail(&state.db, election_id).await;
    }

    // Round 2 — plurality (ties broken by name order already applied).
    appoint_mayor(&state.db, election_id, &scored[0].0).await?;
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

    let mut q = sqlx::query_as::<_, MayorElection>(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    q.fetch_all(pool)
        .await
        .map_err(|e| MayorError::Database(e.to_string()))
}

// --- HTTP handlers ---

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
}
