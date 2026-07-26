use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::models::Event;
use crate::types::Role;

pub async fn apply_event_projections_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    events: &[Event],
) -> Result<(), String> {
    for event in events {
        match event.event_type.as_str() {
            "CitizenAdded" => apply_citizen_added(tx, event).await?,
            "PassportIssued" => apply_passport_issued(tx, event).await?,
            "PassportRevoked" => apply_passport_revoked(tx, event).await?,
            "CitizenSuspended" => apply_citizen_status(tx, event, "suspended").await?,
            "CitizenRestored" => apply_citizen_status(tx, event, "active").await?,
            "CitizenUpdated" => apply_citizen_updated(tx, event).await?,
            "CandidateNominated" => apply_candidate_nominated(tx, event).await?,
            "CandidateVoted" => apply_candidate_voted(tx, event).await?,
            "CandidateApproved" => apply_candidate_approved(tx, event).await?,
            "CandidateAppointed" => apply_candidate_appointed(tx, event).await?,
            "NodeStarted" => apply_node_started(tx, event).await?,
            "MayorConfirmed" => apply_mayor_confirmed(tx, event).await?,
            "LawProposed" => apply_law_proposed(tx, event).await?,
            "LawVoteStarted" => apply_law_vote_started(tx, event).await?,
            "LawVoteResult" => apply_law_vote_result(tx, event).await?,
            "VoteCast" => apply_vote_cast(tx, event).await?,
            "ElectionAnnounced" => apply_election_announced(tx, event).await?,
            "ElectionCandidate" => apply_election_candidate(tx, event).await?,
            "ElectionVoteStarted" => apply_election_vote_started(tx, event).await?,
            "ElectionVoteResult" => apply_election_vote_result(tx, event).await?,
            "ChatHistoryAdded" => apply_chat_history_added(tx, event).await?,
            _ => {}
        }
    }
    Ok(())
}

fn require_str<'a>(data: &'a serde_json::Value, field: &str) -> Result<&'a str, String> {
    data.get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{}: missing {}", field, field))
}

fn require_i64(data: &serde_json::Value, field: &str) -> Result<i64, String> {
    data.get(field)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| format!("{}: missing {}", field, field))
}

async fn apply_citizen_added(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let citizen_id = require_str(&event.data, "citizen_id")?;
    let citizen_name = require_str(&event.data, "citizen_name")?;
    let public_key = require_str(&event.data, "public_key")?;
    let role_str = event
        .data
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("Citizen");
    let role = Role::from_str(role_str)
        .ok_or_else(|| format!("CitizenAdded: invalid role {}", role_str))?;

    let name_taken: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM citizens WHERE name = $1)",
    )
    .bind(citizen_name)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| format!("CitizenAdded name check failed for {}: {}", citizen_id, e))?;

    if name_taken {
        tracing::warn!(
            citizen_id = %citizen_id,
            name = %citizen_name,
            event_id = %event.event_id,
            "CitizenAdded skipped: name already registered"
        );
        return Ok(());
    }

    sqlx::query(
        r#"
        INSERT INTO citizens (id, name, public_key, status, role, created_at, passport_issued)
        VALUES ($1, $2, $3, 'pending', $4, $5, FALSE)
        ON CONFLICT (id) DO UPDATE SET
            name = EXCLUDED.name,
            public_key = EXCLUDED.public_key,
            role = EXCLUDED.role
        "#,
    )
    .bind(citizen_id)
    .bind(citizen_name)
    .bind(public_key)
    .bind(role.as_str())
    .bind(event.timestamp)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("CitizenAdded projection failed for {}: {}", citizen_id, e))?;

    tracing::info!(
        citizen_id = %citizen_id,
        name = %citizen_name,
        event_id = %event.event_id,
        "CitizenAdded projected to SQL"
    );

    Ok(())
}

async fn apply_passport_issued(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let passport_id = require_str(&event.data, "passport_id")?;
    let citizen_id = require_str(&event.data, "citizen_id")?;
    let issued_at = event
        .data
        .get("issued_at")
        .and_then(|v| v.as_i64())
        .unwrap_or(event.timestamp);
    let expires_at = require_i64(&event.data, "expires_at")?;

    sqlx::query(
        "INSERT INTO passports (id, citizen_id, issued_at, expires_at, is_valid)
         VALUES ($1, $2, $3, $4, TRUE)
         ON CONFLICT (id) DO UPDATE SET
            expires_at = EXCLUDED.expires_at,
            is_valid = TRUE",
    )
    .bind(passport_id)
    .bind(citizen_id)
    .bind(issued_at)
    .bind(expires_at)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("PassportIssued projection failed for {}: {}", passport_id, e))?;

    sqlx::query(
        "UPDATE citizens SET passport_issued = TRUE, passport_expires = $1, status = 'active' WHERE id = $2",
    )
    .bind(expires_at)
    .bind(citizen_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("PassportIssued citizen update failed for {}: {}", citizen_id, e))?;

    tracing::info!(
        citizen_id = %citizen_id,
        passport_id = %passport_id,
        event_id = %event.event_id,
        "PassportIssued projected to SQL"
    );

    Ok(())
}

async fn apply_passport_revoked(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let passport_id = require_str(&event.data, "passport_id")?;
    let citizen_id = require_str(&event.data, "citizen_id")?;

    sqlx::query("UPDATE passports SET is_valid = FALSE WHERE id = $1")
        .bind(passport_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("PassportRevoked projection failed for {}: {}", passport_id, e))?;

    sqlx::query(
        "UPDATE citizens SET passport_issued = FALSE, passport_expires = NULL WHERE id = $1",
    )
    .bind(citizen_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("PassportRevoked citizen update failed for {}: {}", citizen_id, e))?;

    tracing::info!(
        citizen_id = %citizen_id,
        passport_id = %passport_id,
        event_id = %event.event_id,
        "PassportRevoked projected to SQL"
    );

    Ok(())
}

async fn apply_citizen_status(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
    status: &str,
) -> Result<(), String> {
    let citizen_id = require_str(&event.data, "citizen_id")?;

    sqlx::query("UPDATE citizens SET status = $1 WHERE id = $2")
        .bind(status)
        .bind(citizen_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("{} projection failed for {}: {}", event.event_type, citizen_id, e))?;

    tracing::info!(
        citizen_id = %citizen_id,
        status = %status,
        event_id = %event.event_id,
        "citizen status projected to SQL"
    );

    Ok(())
}

async fn apply_citizen_updated(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    if event.data.get("status").is_some() {
        let status = require_str(&event.data, "status")?;
        apply_citizen_status(tx, event, status).await?;
    }

    if event.data.get("role").is_some() {
        let citizen_id = require_str(&event.data, "citizen_id")?;
        let role_str = require_str(&event.data, "role")?;
        let role = Role::from_str(role_str)
            .ok_or_else(|| format!("CitizenUpdated: invalid role {}", role_str))?;

        sqlx::query("UPDATE citizens SET role = $1 WHERE id = $2")
            .bind(role.as_str())
            .bind(citizen_id)
            .execute(&mut **tx)
            .await
            .map_err(|e| format!("CitizenUpdated role projection failed for {}: {}", citizen_id, e))?;

        sqlx::query(
            "UPDATE api_keys SET role = $1 WHERE citizen_id = $2",
        )
        .bind(role.as_str())
        .bind(citizen_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("CitizenUpdated api_keys sync failed for {}: {}", citizen_id, e))?;

        tracing::info!(
            citizen_id = %citizen_id,
            role = %role.as_str(),
            event_id = %event.event_id,
            "citizen role projected to SQL"
        );
    }

    Ok(())
}

async fn apply_candidate_nominated(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let candidacy_id = require_str(&event.data, "candidacy_id")?;
    let citizen_id = require_str(&event.data, "citizen_id")?;
    let target_role = require_str(&event.data, "target_role")?;
    let nominator_id = require_str(&event.data, "nominator_id")?;
    let threshold = event
        .data
        .get("threshold")
        .and_then(|v| v.as_i64())
        .unwrap_or(1) as i32;

    sqlx::query(
        r#"
        INSERT INTO candidacies (id, citizen_id, target_role, status, votes_for, votes_against, threshold, nominator_id, created_at)
        VALUES ($1, $2, $3, 'Active', 0, 0, $4, $5, to_timestamp($6))
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(candidacy_id)
    .bind(citizen_id)
    .bind(target_role)
    .bind(threshold)
    .bind(nominator_id)
    .bind(event.timestamp as f64)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("CandidateNominated projection failed: {}", e))?;

    tracing::info!(candidacy_id = %candidacy_id, event_id = %event.event_id, "CandidateNominated projected");
    Ok(())
}

async fn apply_candidate_voted(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let candidacy_id = require_str(&event.data, "candidacy_id")?;
    let citizen_id = require_str(&event.data, "citizen_id")?;
    let vote = require_str(&event.data, "vote")?;
    let vote_id = format!("{}_{}", candidacy_id, citizen_id);

    sqlx::query(
        r#"
        INSERT INTO candidacy_votes (id, candidacy_id, citizen_id, vote, created_at)
        VALUES ($1, $2, $3, $4, to_timestamp($5))
        ON CONFLICT (candidacy_id, citizen_id) DO UPDATE SET vote = EXCLUDED.vote
        "#,
    )
    .bind(&vote_id)
    .bind(candidacy_id)
    .bind(citizen_id)
    .bind(vote)
    .bind(event.timestamp as f64)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("CandidateVoted projection failed: {}", e))?;

    sqlx::query(
        r#"
        UPDATE candidacies SET
            votes_for = (SELECT COUNT(*)::int FROM candidacy_votes WHERE candidacy_id = $1 AND vote = 'For'),
            votes_against = (SELECT COUNT(*)::int FROM candidacy_votes WHERE candidacy_id = $1 AND vote = 'Against')
        WHERE id = $1
        "#,
    )
    .bind(candidacy_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("CandidateVoted count refresh failed: {}", e))?;

    tracing::info!(candidacy_id = %candidacy_id, voter = %citizen_id, event_id = %event.event_id, "CandidateVoted projected");
    Ok(())
}

async fn apply_candidate_approved(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let candidacy_id = require_str(&event.data, "candidacy_id")?;
    let citizen_id = require_str(&event.data, "citizen_id")?;
    let target_role = require_str(&event.data, "target_role")?;

    sqlx::query(
        "UPDATE candidacies SET status = 'Approved', approved_at = to_timestamp($1) WHERE id = $2 AND status = 'Active'",
    )
    .bind(event.timestamp as f64)
    .bind(candidacy_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("CandidateApproved projection failed: {}", e))?;

    // Register Candidate (role) with nominated target_role.
    sqlx::query(
        r#"
        UPDATE citizens
        SET role = $1, candidate_type = $2, target_role = $3
        WHERE id = $4
        "#,
    )
    .bind(Role::Candidate.as_str())
    .bind(crate::types::CandidateType::Role.as_str())
    .bind(target_role)
    .bind(citizen_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("CandidateApproved citizen candidacy register failed: {}", e))?;

    sqlx::query(
        "UPDATE api_keys SET is_active = FALSE WHERE citizen_id = $1 AND is_active = TRUE",
    )
    .bind(citizen_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("CandidateApproved api_keys deactivate failed: {}", e))?;

    tracing::info!(candidacy_id = %candidacy_id, event_id = %event.event_id, "CandidateApproved projected");
    Ok(())
}

async fn apply_candidate_appointed(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let candidacy_id = require_str(&event.data, "candidacy_id")?;
    let citizen_id = require_str(&event.data, "citizen_id")?;
    let target_role = require_str(&event.data, "target_role")?;
    let role = Role::from_str(target_role)
        .ok_or_else(|| format!("CandidateAppointed: invalid role {}", target_role))?;

    sqlx::query(
        "UPDATE candidacies SET status = 'Appointed', appointed_at = to_timestamp($1) WHERE id = $2",
    )
    .bind(event.timestamp as f64)
    .bind(candidacy_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("CandidateAppointed candidacy update failed: {}", e))?;

    sqlx::query("UPDATE citizens SET role = $1, candidate_type = NULL, target_role = NULL WHERE id = $2")
        .bind(role.as_str())
        .bind(citizen_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("CandidateAppointed role projection failed: {}", e))?;

    sqlx::query(
        "UPDATE api_keys SET role = $1 WHERE citizen_id = $2",
    )
    .bind(role.as_str())
    .bind(citizen_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("CandidateAppointed api_keys sync failed: {}", e))?;

    tracing::info!(candidacy_id = %candidacy_id, citizen_id = %citizen_id, event_id = %event.event_id, "CandidateAppointed projected");
    Ok(())
}

async fn apply_node_started(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let application_id = require_str(&event.data, "application_id")?;
    let node_id = require_str(&event.data, "node_id")?;
    let url = require_str(&event.data, "url")?;
    let version = event
        .data
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.7.0");
    let status = event
        .data
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("alive");

    sqlx::query(
        r#"
        UPDATE mayor_applications
        SET status = 'approved'
        WHERE id = $1::uuid AND status IN ('pending', 'approved')
        "#,
    )
    .bind(application_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("NodeStarted application projection failed: {}", e))?;

    if let Ok(app_uuid) = Uuid::parse_str(application_id) {
        crate::candidates::mark_status_tx(
            tx,
            app_uuid,
            "approved",
            None,
            Some(serde_json::json!({
                "node_id": node_id,
                "node_url": url,
            })),
            None,
        )
        .await
        .map_err(|e| format!("NodeStarted candidates sync failed: {}", e))?;
    }

    sqlx::query(
        r#"
        INSERT INTO nodes (id, url, public_key, status, last_seen, version, created_at)
        VALUES ($1, $2, '', $3, to_timestamp($4), $5, to_timestamp($4))
        ON CONFLICT (id) DO UPDATE SET
            url = EXCLUDED.url,
            status = EXCLUDED.status,
            last_seen = EXCLUDED.last_seen,
            version = EXCLUDED.version
        "#,
    )
    .bind(node_id)
    .bind(url)
    .bind(status)
    .bind(event.timestamp as f64)
    .bind(version)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("NodeStarted node projection failed: {}", e))?;

    tracing::info!(
        application_id = %application_id,
        node_id = %node_id,
        event_id = %event.event_id,
        "NodeStarted projected"
    );
    Ok(())
}

async fn apply_mayor_confirmed(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let application_id = require_str(&event.data, "application_id")?;
    let citizen_id = require_str(&event.data, "citizen_id")?;
    let role = Role::Mayor;

    sqlx::query(
        r#"
        UPDATE mayor_applications
        SET status = 'confirmed'
        WHERE id = $1::uuid AND status IN ('pending', 'approved', 'confirmed')
        "#,
    )
    .bind(application_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("MayorConfirmed application projection failed: {}", e))?;

    if let Ok(app_uuid) = Uuid::parse_str(application_id) {
        crate::candidates::mark_status_tx(
            tx,
            app_uuid,
            "confirmed",
            None,
            None,
            Some(citizen_id),
        )
        .await
        .map_err(|e| format!("MayorConfirmed candidates sync failed: {}", e))?;
    }

    sqlx::query(
        "UPDATE citizens SET role = $1, candidate_type = NULL, target_role = NULL WHERE id = $2",
    )
    .bind(role.as_str())
    .bind(citizen_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("MayorConfirmed role projection failed: {}", e))?;

    sqlx::query("UPDATE api_keys SET role = $1 WHERE citizen_id = $2")
        .bind(role.as_str())
        .bind(citizen_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("MayorConfirmed api_keys sync failed: {}", e))?;

    tracing::info!(
        application_id = %application_id,
        citizen_id = %citizen_id,
        event_id = %event.event_id,
        "MayorConfirmed projected"
    );
    Ok(())
}

async fn apply_law_proposed(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let law_id = require_str(&event.data, "law_id")?;
    let title = require_str(&event.data, "title")?;
    let description = require_str(&event.data, "description")?;
    let proposer_id = require_str(&event.data, "proposer_id")?;
    let threshold = event
        .data
        .get("threshold")
        .and_then(|v| v.as_i64())
        .unwrap_or(1) as i32;

    sqlx::query(
        r#"
        INSERT INTO initiatives (id, title, description, status, proposer_id, votes_for, votes_against, threshold, created_at)
        VALUES ($1, $2, $3, 'Proposed', $4, 0, 0, $5, to_timestamp($6))
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(law_id)
    .bind(title)
    .bind(description)
    .bind(proposer_id)
    .bind(threshold)
    .bind(event.timestamp as f64)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("LawProposed projection failed: {}", e))?;

    tracing::info!(law_id = %law_id, event_id = %event.event_id, "LawProposed projected");
    Ok(())
}

async fn apply_law_vote_started(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let law_id = require_str(&event.data, "law_id")?;
    sqlx::query(
        "UPDATE initiatives SET status = 'Proposed' WHERE id = $1 AND status != 'Passed'",
    )
    .bind(law_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("LawVoteStarted projection failed: {}", e))?;

    tracing::info!(law_id = %law_id, event_id = %event.event_id, "LawVoteStarted projected");
    Ok(())
}

async fn apply_law_vote_result(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let law_id = require_str(&event.data, "law_id")?;
    let result = event
        .data
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("Passed");

    if result == "Passed" {
        sqlx::query(
            "UPDATE initiatives SET status = 'Passed', passed_at = to_timestamp($1) WHERE id = $2",
        )
        .bind(event.timestamp as f64)
        .bind(law_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("LawVoteResult projection failed: {}", e))?;
    }

    tracing::info!(law_id = %law_id, result = %result, event_id = %event.event_id, "LawVoteResult projected");
    Ok(())
}

async fn apply_vote_cast(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let kind = event
        .data
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if kind == "mayor" {
        return apply_mayor_vote_cast(tx, event).await;
    }

    let vote_id = require_str(&event.data, "vote_id")?;
    let citizen_id = require_str(&event.data, "citizen_id")?;
    let choice = require_str(&event.data, "choice")?;
    let record_id = format!("{vote_id}_{citizen_id}");

    let is_initiative: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM initiatives WHERE id = $1)",
    )
    .bind(vote_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| format!("VoteCast initiative check failed: {}", e))?;

    if is_initiative {
        sqlx::query(
            r#"
            INSERT INTO initiative_votes (id, initiative_id, citizen_id, vote, created_at)
            VALUES ($1, $2, $3, $4, to_timestamp($5))
            ON CONFLICT (initiative_id, citizen_id) DO UPDATE SET vote = EXCLUDED.vote
            "#,
        )
        .bind(&record_id)
        .bind(vote_id)
        .bind(citizen_id)
        .bind(choice)
        .bind(event.timestamp as f64)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("VoteCast initiative projection failed: {}", e))?;

        sqlx::query(
            r#"
            UPDATE initiatives SET
                votes_for = (SELECT COUNT(*)::int FROM initiative_votes WHERE initiative_id = $1 AND vote = 'For'),
                votes_against = (SELECT COUNT(*)::int FROM initiative_votes WHERE initiative_id = $1 AND vote = 'Against')
            WHERE id = $1
            "#,
        )
        .bind(vote_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("VoteCast initiative count refresh failed: {}", e))?;
    } else {
        let is_referendum: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM referendums WHERE id = $1)",
        )
        .bind(vote_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| format!("VoteCast referendum check failed: {}", e))?;

        if is_referendum {
            sqlx::query(
                r#"
                INSERT INTO referendum_votes (id, referendum_id, citizen_id, vote, created_at)
                VALUES ($1, $2, $3, $4, to_timestamp($5))
                ON CONFLICT (referendum_id, citizen_id) DO UPDATE SET vote = EXCLUDED.vote
                "#,
            )
            .bind(&record_id)
            .bind(vote_id)
            .bind(citizen_id)
            .bind(choice)
            .bind(event.timestamp as f64)
            .execute(&mut **tx)
            .await
            .map_err(|e| format!("VoteCast referendum projection failed: {}", e))?;

            sqlx::query(
                r#"
                UPDATE referendums SET
                    votes_for = (SELECT COUNT(*)::int FROM referendum_votes WHERE referendum_id = $1 AND vote = 'For'),
                    votes_against = (SELECT COUNT(*)::int FROM referendum_votes WHERE referendum_id = $1 AND vote = 'Against')
                WHERE id = $1
                "#,
            )
            .bind(vote_id)
            .execute(&mut **tx)
            .await
            .map_err(|e| format!("VoteCast referendum count refresh failed: {}", e))?;
        }
    }

    tracing::info!(vote_id = %vote_id, voter = %citizen_id, event_id = %event.event_id, "VoteCast projected");
    Ok(())
}

async fn apply_election_announced(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let kind = event
        .data
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if kind == "mayor" || event.data.get("city_name").and_then(|v| v.as_str()).is_some() {
        return apply_mayor_election_announced(tx, event).await;
    }

    let election_id = require_str(&event.data, "election_id")?;
    let title = require_str(&event.data, "title")?;
    let target_decision = require_str(&event.data, "target_decision")?;
    let announcer_id = event
        .data
        .get("announcer_id")
        .and_then(|v| v.as_str())
        .unwrap_or("system");

    sqlx::query(
        r#"
        INSERT INTO referendums (id, title, description, target_decision, status, announcer_id, votes_for, votes_against, created_at)
        VALUES ($1, $2, $3, $4, 'Active', $5, 0, 0, to_timestamp($6))
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(election_id)
    .bind(title)
    .bind(event.description.clone())
    .bind(target_decision)
    .bind(announcer_id)
    .bind(event.timestamp as f64)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("ElectionAnnounced projection failed: {}", e))?;

    tracing::info!(election_id = %election_id, event_id = %event.event_id, "ElectionAnnounced projected");
    Ok(())
}

async fn apply_election_candidate(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let kind = event
        .data
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if kind != "mayor" {
        return Ok(());
    }
    let action = event
        .data
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("nominate");
    match action {
        "endorse" => apply_mayor_election_endorse(tx, event).await,
        _ => apply_mayor_election_nominate(tx, event).await,
    }
}

async fn apply_election_vote_started(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let kind = event
        .data
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if kind != "mayor" {
        return Ok(());
    }
    apply_mayor_election_vote_started(tx, event).await
}

async fn apply_election_vote_result(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let kind = event
        .data
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if kind != "mayor" {
        return Ok(());
    }
    apply_mayor_election_vote_result(tx, event).await
}

async fn apply_mayor_election_announced(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let election_id = require_str(&event.data, "election_id")?;
    let city_name = require_str(&event.data, "city_name")?;
    let created_by = require_str(&event.data, "created_by")?;
    let status = event
        .data
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("Nominating");

    sqlx::query(
        r#"
        INSERT INTO mayor_elections (id, city_name, status, current_round, created_by, created_at)
        VALUES ($1, $2, $3, 0, $4, to_timestamp($5))
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(election_id)
    .bind(city_name)
    .bind(status)
    .bind(created_by)
    .bind(event.timestamp as f64)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("Mayor ElectionAnnounced projection failed: {}", e))?;

    tracing::info!(
        election_id = %election_id,
        city_name = %city_name,
        event_id = %event.event_id,
        "Mayor ElectionAnnounced projected"
    );
    Ok(())
}

async fn apply_mayor_election_nominate(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let election_id = require_str(&event.data, "election_id")?;
    let candidate_id = require_str(&event.data, "candidate_id")?;
    let citizen_id = require_str(&event.data, "citizen_id")?;
    let citizen_name = event
        .data
        .get("citizen_name")
        .and_then(|v| v.as_str())
        .unwrap_or(citizen_id);
    let nominated_by = require_str(&event.data, "nominated_by")?;
    let status = event
        .data
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("Nominated");

    sqlx::query(
        r#"
        INSERT INTO mayor_candidates (
            id, election_id, citizen_id, citizen_name, nominated_by, status, endorsements, created_at
        ) VALUES ($1, $2, $3, $4, $5, $6, 0, to_timestamp($7))
        ON CONFLICT (election_id, citizen_id) DO NOTHING
        "#,
    )
    .bind(candidate_id)
    .bind(election_id)
    .bind(citizen_id)
    .bind(citizen_name)
    .bind(nominated_by)
    .bind(status)
    .bind(event.timestamp as f64)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("Mayor ElectionCandidate nominate projection failed: {}", e))?;

    tracing::info!(
        election_id = %election_id,
        candidate_id = %candidate_id,
        event_id = %event.event_id,
        "Mayor ElectionCandidate nominate projected"
    );
    Ok(())
}

async fn apply_mayor_election_endorse(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let candidate_id = require_str(&event.data, "candidate_id")?;
    let endorsement_id = require_str(&event.data, "endorsement_id")?;
    let endorser_id = require_str(&event.data, "endorser_id")?;

    sqlx::query(
        r#"
        INSERT INTO mayor_endorsements (id, candidate_id, citizen_id, created_at)
        VALUES ($1, $2, $3, to_timestamp($4))
        ON CONFLICT (candidate_id, citizen_id) DO NOTHING
        "#,
    )
    .bind(endorsement_id)
    .bind(candidate_id)
    .bind(endorser_id)
    .bind(event.timestamp as f64)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("Mayor ElectionCandidate endorse projection failed: {}", e))?;

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
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("Mayor endorsement count refresh failed: {}", e))?;

    tracing::info!(
        candidate_id = %candidate_id,
        endorser_id = %endorser_id,
        event_id = %event.event_id,
        "Mayor ElectionCandidate endorse projected"
    );
    Ok(())
}

async fn apply_mayor_election_vote_started(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let election_id = require_str(&event.data, "election_id")?;
    let round = require_i64(&event.data, "round")? as i32;
    let status = event
        .data
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or(if round >= 2 { "Round2" } else { "Round1" });

    if round <= 1 {
        sqlx::query(
            r#"
            UPDATE mayor_candidates SET status = 'Qualified'
            WHERE election_id = $1 AND status IN ('Nominated', 'Qualified')
            "#,
        )
        .bind(election_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("Mayor ElectionVoteStarted qualify failed: {}", e))?;

        sqlx::query(
            r#"
            UPDATE mayor_elections
            SET status = $1, current_round = 1, round1_started_at = COALESCE(round1_started_at, to_timestamp($2))
            WHERE id = $3 AND status IN ('Nominating', 'Round1')
            "#,
        )
        .bind(status)
        .bind(event.timestamp as f64)
        .bind(election_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("Mayor ElectionVoteStarted Round1 failed: {}", e))?;
    } else {
        sqlx::query(
            r#"
            UPDATE mayor_elections
            SET status = $1, current_round = 2, round2_started_at = COALESCE(round2_started_at, to_timestamp($2))
            WHERE id = $3 AND status IN ('Round1', 'Round2')
            "#,
        )
        .bind(status)
        .bind(event.timestamp as f64)
        .bind(election_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("Mayor ElectionVoteStarted Round2 failed: {}", e))?;
    }

    tracing::info!(
        election_id = %election_id,
        round = round,
        event_id = %event.event_id,
        "Mayor ElectionVoteStarted projected"
    );
    Ok(())
}

async fn apply_mayor_vote_cast(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let election_id = event
        .data
        .get("election_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| event.data.get("vote_id").and_then(|v| v.as_str()))
        .ok_or_else(|| "election_id: missing election_id".to_string())?;
    let ballot_id = require_str(&event.data, "ballot_id")?;
    let voter_id = require_str(&event.data, "citizen_id")?;
    let candidate_id = require_str(&event.data, "candidate_id")?;
    let round = require_i64(&event.data, "round")? as i32;

    sqlx::query(
        r#"
        INSERT INTO mayor_votes (id, election_id, round, voter_id, candidate_id, created_at)
        VALUES ($1, $2, $3, $4, $5, to_timestamp($6))
        ON CONFLICT (election_id, round, voter_id) DO NOTHING
        "#,
    )
    .bind(ballot_id)
    .bind(election_id)
    .bind(round)
    .bind(voter_id)
    .bind(candidate_id)
    .bind(event.timestamp as f64)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("Mayor VoteCast projection failed: {}", e))?;

    tracing::info!(
        election_id = %election_id,
        voter = %voter_id,
        candidate_id = %candidate_id,
        round = round,
        event_id = %event.event_id,
        "Mayor VoteCast projected"
    );
    Ok(())
}

async fn apply_mayor_election_vote_result(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let election_id = require_str(&event.data, "election_id")?;
    let outcome = require_str(&event.data, "outcome")?;
    let round = require_i64(&event.data, "round")? as i32;

    if outcome == "runoff" {
        let advanced: Vec<String> = event
            .data
            .get("advanced_candidate_ids")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        if !advanced.is_empty() {
            sqlx::query(
                r#"
                UPDATE mayor_candidates
                SET status = CASE
                    WHEN id = ANY($1) THEN 'Advanced'
                    ELSE 'Eliminated'
                END
                WHERE election_id = $2
                "#,
            )
            .bind(&advanced)
            .bind(election_id)
            .execute(&mut **tx)
            .await
            .map_err(|e| format!("Mayor ElectionVoteResult runoff candidates failed: {}", e))?;
        }

        sqlx::query(
            r#"
            UPDATE mayor_elections
            SET status = 'Round2', current_round = 2,
                round2_started_at = COALESCE(round2_started_at, to_timestamp($1))
            WHERE id = $2 AND status IN ('Round1', 'Round2')
            "#,
        )
        .bind(event.timestamp as f64)
        .bind(election_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("Mayor ElectionVoteResult runoff election failed: {}", e))?;

        tracing::info!(
            election_id = %election_id,
            round = round,
            event_id = %event.event_id,
            "Mayor ElectionVoteResult runoff projected"
        );
        return Ok(());
    }

    if outcome == "winner" {
        let winner_candidate_id = require_str(&event.data, "winner_candidate_id")?;
        let winner_id = require_str(&event.data, "winner_id")?;
        let winner_name = event
            .data
            .get("winner_name")
            .and_then(|v| v.as_str())
            .unwrap_or(winner_id);

        sqlx::query(
            r#"
            UPDATE mayor_candidates SET status = 'Winner' WHERE id = $1
            "#,
        )
        .bind(winner_candidate_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("Mayor ElectionVoteResult winner candidate failed: {}", e))?;

        sqlx::query(
            r#"
            UPDATE mayor_candidates
            SET status = 'Eliminated'
            WHERE election_id = $1 AND id <> $2 AND status <> 'Eliminated'
            "#,
        )
        .bind(election_id)
        .bind(winner_candidate_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("Mayor ElectionVoteResult eliminate failed: {}", e))?;

        sqlx::query(
            r#"
            UPDATE citizens SET role = $1
            WHERE role = $2 AND id <> $3
            "#,
        )
        .bind(Role::Citizen.as_str())
        .bind(Role::Mayor.as_str())
        .bind(winner_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("Mayor ElectionVoteResult demote previous failed: {}", e))?;

        sqlx::query("UPDATE citizens SET role = $1 WHERE id = $2")
            .bind(Role::Mayor.as_str())
            .bind(winner_id)
            .execute(&mut **tx)
            .await
            .map_err(|e| format!("Mayor ElectionVoteResult role failed: {}", e))?;

        sqlx::query(
            r#"
            UPDATE api_keys SET role = $1
            WHERE citizen_id = $2 AND is_active = TRUE
            "#,
        )
        .bind(Role::Mayor.as_str())
        .bind(winner_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("Mayor ElectionVoteResult api_keys failed: {}", e))?;

        sqlx::query(
            r#"
            UPDATE mayor_elections
            SET status = 'Completed', winner_id = $1, winner_name = $2,
                completed_at = COALESCE(completed_at, to_timestamp($3))
            WHERE id = $4 AND status <> 'Cancelled'
            "#,
        )
        .bind(winner_id)
        .bind(winner_name)
        .bind(event.timestamp as f64)
        .bind(election_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("Mayor ElectionVoteResult complete failed: {}", e))?;

        tracing::info!(
            election_id = %election_id,
            winner_id = %winner_id,
            event_id = %event.event_id,
            "Mayor ElectionVoteResult winner projected"
        );
        return Ok(());
    }

    tracing::warn!(
        election_id = %election_id,
        outcome = %outcome,
        event_id = %event.event_id,
        "Mayor ElectionVoteResult unknown outcome"
    );
    Ok(())
}

async fn apply_chat_history_added(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<(), String> {
    let archive_id = require_str(&event.data, "archive_id")?;
    let session_id = require_str(&event.data, "session_id")?;
    let user_id = require_str(&event.data, "user_id")?;
    let compressed_hex = require_str(&event.data, "compressed_data")?;
    let original_size = require_i64(&event.data, "original_size")? as i32;
    let compressed_size = require_i64(&event.data, "compressed_size")? as i32;

    let compressed_data = hex::decode(compressed_hex)
        .map_err(|e| format!("ChatHistoryAdded: invalid compressed_data hex: {}", e))?;

    // Ensure citizen exists (skip if unknown — archive still stored once citizen appears via other events)
    let citizen_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM citizens WHERE id = $1)",
    )
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| format!("ChatHistoryAdded citizen check failed: {}", e))?;

    if !citizen_exists {
        tracing::warn!(
            user_id = %user_id,
            event_id = %event.event_id,
            "ChatHistoryAdded skipped: citizen not found"
        );
        return Ok(());
    }

    sqlx::query(
        r#"
        INSERT INTO chat_sessions (id, user_id, started_at, last_activity, is_active)
        VALUES ($1, $2, to_timestamp($3), to_timestamp($3), FALSE)
        ON CONFLICT (id) DO UPDATE SET
            is_active = FALSE,
            last_activity = GREATEST(chat_sessions.last_activity, EXCLUDED.last_activity)
        "#,
    )
    .bind(session_id)
    .bind(user_id)
    .bind(event.timestamp as f64)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("ChatHistoryAdded session upsert failed: {}", e))?;

    sqlx::query(
        r#"
        INSERT INTO chat_archives (
            id, session_id, compressed_data, original_size, compressed_size,
            created_at, event_id, is_synced
        )
        VALUES ($1, $2, $3, $4, $5, to_timestamp($6), $7, TRUE)
        ON CONFLICT (id) DO UPDATE SET
            event_id = COALESCE(chat_archives.event_id, EXCLUDED.event_id),
            is_synced = TRUE
        "#,
    )
    .bind(archive_id)
    .bind(session_id)
    .bind(&compressed_data)
    .bind(original_size)
    .bind(compressed_size)
    .bind(event.timestamp as f64)
    .bind(&event.event_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("ChatHistoryAdded archive insert failed: {}", e))?;

    // Shared room archive: clear the entire live feed (not only event.session_id).
    if user_id == crate::chat::ROOM_OWNER_ID {
        sqlx::query("DELETE FROM chat_messages")
            .execute(&mut **tx)
            .await
            .map_err(|e| format!("ChatHistoryAdded room message cleanup failed: {}", e))?;
        sqlx::query(
            "UPDATE chat_sessions SET is_active = FALSE
             WHERE is_active = TRUE AND user_id = $1",
        )
        .bind(crate::chat::ROOM_OWNER_ID)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("ChatHistoryAdded room session close failed: {}", e))?;
    } else {
        sqlx::query("DELETE FROM chat_messages WHERE session_id = $1")
            .bind(session_id)
            .execute(&mut **tx)
            .await
            .map_err(|e| format!("ChatHistoryAdded message cleanup failed: {}", e))?;
    }

    tracing::info!(
        archive_id = %archive_id,
        session_id = %session_id,
        event_id = %event.event_id,
        "ChatHistoryAdded projected"
    );
    Ok(())
}
