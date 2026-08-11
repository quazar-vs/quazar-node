use crate::types::{is_role_candidacy_target, CityData, QuazarEventType};
use crate::models::Event;
use serde_json::Value;
use sqlx::PgPool;

/// Canonical display form of the reserved founder name (QZ-1).
pub const FOUNDER_DISPLAY_NAME: &str = "Successful";

/// Lowercase key used for reserved-name checks and uniqueness.
pub const FOUNDER_NAME_KEY: &str = "successful";

#[derive(Debug)]
pub enum ValidationError {
    InvalidEventType(String),
    MissingRequiredField(String),
    InvalidCitizenName(String),
    DuplicateCitizenName,
    DuplicateEventId,
    InvalidPublicKey,
    DataValidationFailed(String),
    HashMismatch,
}

impl ValidationError {
    pub fn message(&self) -> String {
        match self {
            ValidationError::InvalidEventType(t) => format!("Unknown event_type: {}", t),
            ValidationError::MissingRequiredField(f) => format!("Missing required field: {}", f),
            ValidationError::InvalidCitizenName(msg) => msg.clone(),
            ValidationError::DuplicateCitizenName => "Citizen name already exists".to_string(),
            ValidationError::DuplicateEventId => "event_id already exists".to_string(),
            ValidationError::InvalidPublicKey => "Invalid public_key".to_string(),
            ValidationError::DataValidationFailed(msg) => msg.clone(),
            ValidationError::HashMismatch => "Event hash does not match content".to_string(),
        }
    }
}

/// Trim + lowercase for storage and case-insensitive comparison.
pub fn normalize_citizen_name(raw: &str) -> String {
    raw.trim().to_lowercase()
}

pub fn is_reserved_citizen_name(name: &str) -> bool {
    normalize_citizen_name(name) == FOUNDER_NAME_KEY
}

/// Validate citizen login/name and return the normalized (lowercase) form for storage.
///
/// Rules: 1–13 Latin letters only (A–Z / a–z); no digits, spaces, or punctuation;
/// `Successful` is reserved for the founder.
pub fn normalize_and_validate_citizen_name(raw: &str) -> Result<String, &'static str> {
    let name = normalize_citizen_name(raw);
    let result = if name.is_empty() || name.len() > 13 {
        Err("Имя должно быть от 1 до 13 символов")
    } else if !name.chars().all(|c| c.is_ascii_alphabetic()) {
        Err("Имя должно содержать только латинские буквы (A–Z, a–z)")
    } else if is_reserved_citizen_name(&name) {
        Err("Имя Successful зарезервировано за основателем")
    } else {
        Ok(name)
    };

    if let Err(reason) = &result {
        tracing::warn!(
            name = %raw.trim(),
            reason = %reason,
            "rejected invalid citizen name"
        );
    }
    result
}

pub struct EventValidator;

impl EventValidator {
    fn require_str<'a>(data: &'a Value, field: &str) -> Result<&'a str, ValidationError> {
        data.get(field)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ValidationError::MissingRequiredField(field.to_string()))
    }

    pub fn validate_citizen_name(name: &str, existing_names: &[String]) -> Result<(), ValidationError> {
        let normalized = normalize_and_validate_citizen_name(name)
            .map_err(|msg| ValidationError::InvalidCitizenName(msg.to_string()))?;
        if existing_names
            .iter()
            .any(|n| normalize_citizen_name(n) == normalized)
        {
            return Err(ValidationError::DuplicateCitizenName);
        }
        Ok(())
    }

    pub fn validate_event_data(
        event_type: &QuazarEventType,
        data: &Value,
        citizens: &[String],
    ) -> Result<(), ValidationError> {
        match event_type {
            QuazarEventType::CitizenAdded => {
                let name = data
                    .get("citizen_name")
                    .and_then(|v| v.as_str())
                    .ok_or(ValidationError::MissingRequiredField("citizen_name".to_string()))?;
                Self::validate_citizen_name(name, citizens)?;
                data.get("birth_place")
                    .ok_or(ValidationError::MissingRequiredField("birth_place".to_string()))?;
                let public_key = data
                    .get("public_key")
                    .and_then(|v| v.as_str())
                    .ok_or(ValidationError::MissingRequiredField("public_key".to_string()))?;
                crate::crypto::validate_public_key_hex(public_key)?;
                if let Some(role) = data.get("role").and_then(|v| v.as_str()) {
                    if crate::types::is_governance_role_str(role) {
                        return Err(ValidationError::DataValidationFailed(
                            "Governance roles cannot be set at registration; use candidacy flow"
                                .to_string(),
                        ));
                    }
                }
                Ok(())
            }
            QuazarEventType::PassportIssued => {
                Self::require_str(data, "citizen_id")?;
                Self::require_str(data, "passport_id")?;
                if data.get("expires_at").is_none() {
                    return Err(ValidationError::MissingRequiredField("expires_at".to_string()));
                }
                Ok(())
            }
            QuazarEventType::LawAdded => {
                data.get("law_id")
                    .ok_or(ValidationError::MissingRequiredField("law_id".to_string()))?;
                data.get("title")
                    .ok_or(ValidationError::MissingRequiredField("title".to_string()))?;
                data.get("content")
                    .ok_or(ValidationError::MissingRequiredField("content".to_string()))?;
                Ok(())
            }
            QuazarEventType::AiyaElected => {
                data.get("new_aiya")
                    .ok_or(ValidationError::MissingRequiredField("new_aiya".to_string()))?;
                data.get("votes")
                    .ok_or(ValidationError::MissingRequiredField("votes".to_string()))?;
                Ok(())
            }
            QuazarEventType::PeerListUpdate => {
                let peers = data.get("peers").and_then(|v| v.as_array()).ok_or(
                    ValidationError::MissingRequiredField("peers".to_string()),
                )?;
                if peers.is_empty() {
                    return Err(ValidationError::DataValidationFailed(
                        "peers array must not be empty".to_string(),
                    ));
                }
                for peer in peers {
                    peer.get("id")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .ok_or(ValidationError::MissingRequiredField(
                            "peers[].id".to_string(),
                        ))?;
                    peer.get("url")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .ok_or(ValidationError::MissingRequiredField(
                            "peers[].url".to_string(),
                        ))?;
                }
                Ok(())
            }
            QuazarEventType::VoteCast => {
                data.get("vote_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or(ValidationError::MissingRequiredField("vote_id".to_string()))?;
                data.get("citizen_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or(ValidationError::MissingRequiredField("citizen_id".to_string()))?;
                let choice = data
                    .get("choice")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or(ValidationError::MissingRequiredField("choice".to_string()))?;
                if !matches!(choice, "For" | "Against") {
                    return Err(ValidationError::DataValidationFailed(format!(
                        "Invalid vote: {}",
                        choice
                    )));
                }
                let kind = data.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                if kind == "mayor" {
                    Self::require_str(data, "candidate_id")?;
                    Self::require_str(data, "ballot_id")?;
                    if data.get("round").and_then(|v| v.as_i64()).is_none() {
                        return Err(ValidationError::MissingRequiredField("round".to_string()));
                    }
                }
                Ok(())
            }
            QuazarEventType::CitizenRemoved | QuazarEventType::CitizenUpdated => {
                Self::require_str(data, "citizen_id")?;
                if matches!(event_type, QuazarEventType::CitizenUpdated) {
                    let has_status = data
                        .get("status")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| !s.is_empty());
                    let has_role = data
                        .get("role")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| !s.is_empty());
                    if !has_status && !has_role {
                        return Err(ValidationError::MissingRequiredField(
                            "status or role".to_string(),
                        ));
                    }
                    if has_role {
                        let role = data
                            .get("role")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if crate::types::Role::from_str(role).is_none() {
                            return Err(ValidationError::DataValidationFailed(format!(
                                "Invalid role: {}",
                                role
                            )));
                        }
                    }
                }
                Ok(())
            }
            QuazarEventType::NodeAdded => {
                data.get("node_id")
                    .or_else(|| data.get("id"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or(ValidationError::MissingRequiredField("node_id".to_string()))?;
                data.get("url")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or(ValidationError::MissingRequiredField("url".to_string()))?;
                Ok(())
            }
            QuazarEventType::SystemInit | QuazarEventType::SystemUpgrade => {
                Self::require_str(data, "version")?;
                Ok(())
            }
            QuazarEventType::SystemConfig => {
                let has_config_key = data
                    .get("config_key")
                    .or_else(|| data.get("key"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .is_some();
                if !has_config_key {
                    return Err(ValidationError::MissingRequiredField(
                        "config_key (setting name in event data, not the API Authorization key)"
                            .to_string(),
                    ));
                }
                Self::require_str(data, "value")?;
                Ok(())
            }
            QuazarEventType::CitizenRequested => {
                let email = data
                    .get("email")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let name = data
                    .get("desired_name")
                    .or_else(|| data.get("citizen_name"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                if email.is_none() && name.is_none() {
                    return Err(ValidationError::MissingRequiredField(
                        "email or desired_name/citizen_name".to_string(),
                    ));
                }
                if let Some(n) = name {
                    // Format only — uniqueness is enforced at application API / CitizenAdded.
                    Self::validate_citizen_name(n, &[])?;
                }
                let _ = citizens;
                Ok(())
            }
            QuazarEventType::CitizenApplicationRejected => {
                let email = data
                    .get("email")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let application_id = data
                    .get("application_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                if email.is_none() && application_id.is_none() {
                    return Err(ValidationError::MissingRequiredField(
                        "email or application_id".to_string(),
                    ));
                }
                Self::require_str(data, "reason")?;
                Ok(())
            }
            QuazarEventType::CitizenSuspended | QuazarEventType::CitizenRestored => {
                Self::require_str(data, "citizen_id")?;
                Ok(())
            }
            QuazarEventType::PassportSuspended | QuazarEventType::PassportRevoked => {
                Self::require_str(data, "citizen_id")?;
                Self::require_str(data, "passport_id")?;
                Ok(())
            }
            QuazarEventType::LawProposed | QuazarEventType::LawAmended | QuazarEventType::LawRepealed => {
                Self::require_str(data, "law_id")?;
                Ok(())
            }
            QuazarEventType::LawVoteStarted | QuazarEventType::LawVoteResult => {
                Self::require_str(data, "law_id")?;
                Self::require_str(data, "vote_id")?;
                Ok(())
            }
            QuazarEventType::ElectionAnnounced => {
                Self::require_str(data, "election_id")?;
                let kind = data.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                if kind == "mayor" {
                    Self::require_str(data, "city_name")?;
                    Self::require_str(data, "created_by")?;
                }
                Ok(())
            }
            QuazarEventType::ElectionCandidate => {
                Self::require_str(data, "election_id")?;
                Self::require_str(data, "candidate_id")?;
                let kind = data.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                if kind == "mayor" {
                    let action = data.get("action").and_then(|v| v.as_str()).unwrap_or("nominate");
                    match action {
                        "endorse" => {
                            Self::require_str(data, "endorsement_id")?;
                            Self::require_str(data, "endorser_id")?;
                        }
                        _ => {
                            Self::require_str(data, "citizen_id")?;
                            Self::require_str(data, "nominated_by")?;
                        }
                    }
                }
                Ok(())
            }
            QuazarEventType::ElectionVoteStarted | QuazarEventType::ElectionVoteResult => {
                Self::require_str(data, "election_id")?;
                Self::require_str(data, "vote_id")?;
                let kind = data.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                if kind == "mayor" {
                    if data.get("round").and_then(|v| v.as_i64()).is_none() {
                        return Err(ValidationError::MissingRequiredField("round".to_string()));
                    }
                    if matches!(event_type, QuazarEventType::ElectionVoteResult) {
                        Self::require_str(data, "outcome")?;
                    }
                }
                Ok(())
            }
            QuazarEventType::AppointmentGuardian | QuazarEventType::AppointmentJudge => {
                Self::require_str(data, "citizen_id")?;
                Self::require_str(data, "appointed_by")?;
                Ok(())
            }
            QuazarEventType::AppointmentRevoked => {
                Self::require_str(data, "citizen_id")?;
                Self::require_str(data, "role")?;
                Ok(())
            }
            QuazarEventType::CourtCaseOpened | QuazarEventType::CourtRuling
            | QuazarEventType::CourtAppeal | QuazarEventType::CourtAppealRuling => {
                Self::require_str(data, "case_id")?;
                Ok(())
            }
            QuazarEventType::DomainRegistered | QuazarEventType::DomainTransferred
            | QuazarEventType::DomainExpired => {
                Self::require_str(data, "domain")?;
                Ok(())
            }
            QuazarEventType::NodeRemoved => {
                Self::require_str(data, "node_id")?;
                Ok(())
            }
            QuazarEventType::InfraMigration => {
                Self::require_str(data, "target")?;
                Ok(())
            }
            QuazarEventType::ConstitutionFullText => {
                Self::require_str(data, "text")?;
                Ok(())
            }
            QuazarEventType::VoteStarted => {
                Self::require_str(data, "vote_id")?;
                Self::require_str(data, "title")?;
                Ok(())
            }
            QuazarEventType::VoteFinalized => {
                Self::require_str(data, "vote_id")?;
                Ok(())
            }
            QuazarEventType::CandidateNominated => {
                Self::require_str(data, "candidacy_id")?;
                Self::require_str(data, "citizen_id")?;
                Self::require_str(data, "target_role")?;
                Self::require_str(data, "nominator_id")?;
                let role = Self::require_str(data, "target_role")?;
                if !is_role_candidacy_target(role) {
                    return Err(ValidationError::DataValidationFailed(format!(
                        "Invalid target_role for candidacy: {}",
                        role
                    )));
                }
                Ok(())
            }
            QuazarEventType::CandidateVoted => {
                Self::require_str(data, "candidacy_id")?;
                Self::require_str(data, "citizen_id")?;
                let vote = Self::require_str(data, "vote")?;
                if !matches!(vote, "For" | "Against") {
                    return Err(ValidationError::DataValidationFailed(format!(
                        "Invalid vote: {}",
                        vote
                    )));
                }
                Ok(())
            }
            QuazarEventType::CandidateApproved | QuazarEventType::CandidateAppointed => {
                Self::require_str(data, "candidacy_id")?;
                Self::require_str(data, "citizen_id")?;
                Self::require_str(data, "target_role")?;
                Ok(())
            }
            QuazarEventType::CityRegistered => {
                let _: CityData = serde_json::from_value(data.clone())
                    .map_err(|_| ValidationError::DataValidationFailed(
                        "Неверный формат данных города".to_string(),
                    ))?;
                Ok(())
            }
            QuazarEventType::YoutubeIntroPosted
            | QuazarEventType::YoutubeCommentPosted
            | QuazarEventType::YoutubeCommentReplied => {
                Self::require_str(data, "video_id")?;
                Self::require_str(data, "comment_id")?;
                Self::require_str(data, "channel_id")?;
                Ok(())
            }
            QuazarEventType::HermesExternalAction => {
                Self::require_str(data, "action")?;
                Self::require_str(data, "source")?;
                Ok(())
            }
            QuazarEventType::ChatHistoryAdded => {
                Self::require_str(data, "archive_id")?;
                Self::require_str(data, "session_id")?;
                Self::require_str(data, "user_id")?;
                Self::require_str(data, "compressed_data")?;
                if data.get("original_size").and_then(|v| v.as_i64()).is_none() {
                    return Err(ValidationError::MissingRequiredField(
                        "original_size".to_string(),
                    ));
                }
                if data.get("compressed_size").and_then(|v| v.as_i64()).is_none() {
                    return Err(ValidationError::MissingRequiredField(
                        "compressed_size".to_string(),
                    ));
                }
                Ok(())
            }
            QuazarEventType::MayorNodeLaunchRequested => {
                Self::require_str(data, "application_id")?;
                Self::require_str(data, "citizen_id")?;
                Self::require_str(data, "node_ip")?;
                Ok(())
            }
            QuazarEventType::NodeStarted => {
                Self::require_str(data, "application_id")?;
                Self::require_str(data, "citizen_id")?;
                Self::require_str(data, "node_id")?;
                Self::require_str(data, "url")?;
                Ok(())
            }
            QuazarEventType::MayorConfirmed => {
                Self::require_str(data, "application_id")?;
                Self::require_str(data, "citizen_id")?;
                Self::require_str(data, "node_id")?;
                Ok(())
            }
            QuazarEventType::MayorApplicationTimedOut => {
                Self::require_str(data, "application_id")?;
                Self::require_str(data, "citizen_id")?;
                Self::require_str(data, "previous_status")?;
                Self::require_str(data, "restored_status")?;
                Ok(())
            }
            QuazarEventType::ExchangeRequestCreated
            | QuazarEventType::ExchangeRequestVerified
            | QuazarEventType::ExchangeRequestRejected => {
                Self::require_str(data, "request_id")?;
                Self::require_str(data, "svod_code")?;
                Ok(())
            }
            QuazarEventType::ExchangeListingOpened => {
                Self::require_str(data, "listing_id")?;
                Self::require_str(data, "request_id")?;
                Self::require_str(data, "svod_code")?;
                Ok(())
            }
            QuazarEventType::ExchangeDealOpened
            | QuazarEventType::ExchangeDealCancelled
            | QuazarEventType::ExchangeDeliveryConfirmed
            | QuazarEventType::ExchangeSettled => {
                Self::require_str(data, "deal_id")?;
                Self::require_str(data, "listing_id")?;
                Self::require_str(data, "svod_code")?;
                Ok(())
            }
            QuazarEventType::ExchangeInventoryAdjusted => {
                Self::require_str(data, "citizen_id")?;
                Self::require_str(data, "svod_code")?;
                Ok(())
            }
            QuazarEventType::ExchangeNeedCreated => {
                Self::require_str(data, "need_id")?;
                Self::require_str(data, "citizen_id")?;
                Self::require_str(data, "svod_code")?;
                Ok(())
            }
            QuazarEventType::ExchangeAgentListed => {
                Self::require_str(data, "need_id")?;
                Self::require_str(data, "listing_id")?;
                Self::require_str(data, "svod_code")?;
                Ok(())
            }
            QuazarEventType::ExchangeAgentDeal => {
                Self::require_str(data, "deal_id")?;
                Self::require_str(data, "listing_id")?;
                Self::require_str(data, "svod_code")?;
                Ok(())
            }
            QuazarEventType::LotExpired => {
                Self::require_str(data, "listing_id")?;
                Self::require_str(data, "svod_code")?;
                Ok(())
            }
            QuazarEventType::SvodServiceCreated
            | QuazarEventType::SvodServiceUpdated
            | QuazarEventType::SvodServiceDeactivated => {
                Self::require_str(data, "code")?;
                Ok(())
            }
        }
    }

    pub async fn validate_event(event: &Event, db: &PgPool) -> Result<(), ValidationError> {
        if event.event_id.trim().is_empty() {
            return Err(ValidationError::MissingRequiredField("event_id".to_string()));
        }
        if event.title.trim().is_empty() {
            return Err(ValidationError::MissingRequiredField("title".to_string()));
        }
        if event.description.trim().is_empty() {
            return Err(ValidationError::MissingRequiredField("description".to_string()));
        }
        if event.initiator.trim().is_empty() {
            return Err(ValidationError::MissingRequiredField("initiator".to_string()));
        }
        if event.timestamp <= 0 {
            return Err(ValidationError::DataValidationFailed(
                "timestamp must be positive".to_string(),
            ));
        }

        if event_id_exists(db, &event.event_id).await {
            return Err(ValidationError::DuplicateEventId);
        }

        let event_type = QuazarEventType::from_str(&event.event_type)
            .map_err(ValidationError::InvalidEventType)?;

        let citizens = get_all_citizen_names(db).await.unwrap_or_default();
        Self::validate_event_data(&event_type, &event.data, &citizens)?;

        if matches!(event_type, QuazarEventType::CitizenUpdated) {
            if let Some(role) = event.data.get("role").and_then(|v| v.as_str()) {
                if crate::types::is_governance_role_str(role) {
                    let citizen_id = Self::require_str(&event.data, "citizen_id")?;
                    validate_governance_role_via_candidacy(db, citizen_id, role).await?;
                }
            }
        }

        if matches!(event_type, QuazarEventType::CityRegistered) {
            let city_data: CityData = serde_json::from_value(event.data.clone())
                .map_err(|_| ValidationError::DataValidationFailed(
                    "Неверный формат данных города".to_string(),
                ))?;

            if event.initiator != city_data.mayor_id {
                return Err(ValidationError::DataValidationFailed(
                    "Мэр не может зарегистрировать город от имени другого гражданина".to_string(),
                ));
            }

            if city_data.is_capital && event.initiator != "QZ-1" {
                return Err(ValidationError::DataValidationFailed(
                    "Только коренной житель (QZ-1) может зарегистрировать столицу".to_string(),
                ));
            }
        }

        let event_hash = crate::blockchain::compute_event_hash(event);
        if let Some(provided) = event.hash.as_ref().filter(|h| !h.is_empty()) {
            if provided != &event_hash {
                return Err(ValidationError::HashMismatch);
            }
        }

        crate::crypto::verify_event_signatures(event, &event_hash, db).await?;

        Ok(())
    }
}

async fn event_id_exists(db: &PgPool, event_id: &str) -> bool {
    let in_pending: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pending_events WHERE event_id = $1)",
    )
    .bind(event_id)
    .fetch_one(db)
    .await
    .unwrap_or(false);

    let in_events: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM events WHERE event_id = $1)",
    )
    .bind(event_id)
    .fetch_one(db)
    .await
    .unwrap_or(false);

    in_pending || in_events
}

async fn validate_governance_role_via_candidacy(
    db: &PgPool,
    citizen_id: &str,
    role: &str,
) -> Result<(), ValidationError> {
    let has_candidacy: bool = sqlx::query_scalar(
        "SELECT EXISTS(
            SELECT 1 FROM candidacies
            WHERE citizen_id = $1 AND target_role = $2 AND status IN ('Approved', 'Appointed')
        )",
    )
    .bind(citizen_id)
    .bind(role)
    .fetch_one(db)
    .await
    .map_err(|e| ValidationError::DataValidationFailed(e.to_string()))?;

    if !has_candidacy {
        return Err(ValidationError::DataValidationFailed(
            "Governance roles can only be assigned through candidacy appointment".to_string(),
        ));
    }
    Ok(())
}

async fn get_all_citizen_names(db: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT name FROM citizens
        UNION
        SELECT data::jsonb->>'citizen_name' FROM events
            WHERE event_type = 'CitizenAdded'
              AND data::jsonb->>'citizen_name' IS NOT NULL
        UNION
        SELECT event_data::jsonb->'data'->>'citizen_name' FROM pending_events
            WHERE event_data::jsonb->>'event_type' = 'CitizenAdded'
              AND event_data::jsonb->'data'->>'citizen_name' IS NOT NULL
        "#,
    )
    .fetch_all(db)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_and_validate_accepts_single_letter() {
        assert_eq!(normalize_and_validate_citizen_name("A").unwrap(), "a");
    }

    #[test]
    fn normalize_and_validate_lowercases() {
        assert_eq!(normalize_and_validate_citizen_name("  Alice  ").unwrap(), "alice");
    }

    #[test]
    fn normalize_and_validate_rejects_digits() {
        assert!(normalize_and_validate_citizen_name("alice1").is_err());
    }

    #[test]
    fn normalize_and_validate_rejects_too_long() {
        assert!(normalize_and_validate_citizen_name("abcdefghijklmn").is_err());
    }

    #[test]
    fn normalize_and_validate_rejects_reserved_successful() {
        let err = normalize_and_validate_citizen_name("Successful").unwrap_err();
        assert!(err.contains("Successful"));
        assert!(normalize_and_validate_citizen_name("SUCCESSFUL").is_err());
        assert!(normalize_and_validate_citizen_name("successful").is_err());
    }

    #[test]
    fn validate_citizen_name_rejects_empty() {
        let err = EventValidator::validate_citizen_name("", &[]).unwrap_err();
        assert!(matches!(err, ValidationError::InvalidCitizenName(_)));
    }

    #[test]
    fn validate_citizen_name_rejects_non_latin() {
        let err = EventValidator::validate_citizen_name("alice123", &[]).unwrap_err();
        assert!(matches!(err, ValidationError::InvalidCitizenName(_)));
    }

    #[test]
    fn validate_citizen_name_rejects_duplicate_case_insensitive() {
        let existing = vec!["Alice".to_string()];
        let err = EventValidator::validate_citizen_name("alice", &existing).unwrap_err();
        assert!(matches!(err, ValidationError::DuplicateCitizenName));
    }

    #[test]
    fn validate_citizen_name_accepts_unique_latin() {
        assert!(EventValidator::validate_citizen_name("bob", &["alice".to_string()]).is_ok());
    }

    #[test]
    fn validate_citizen_name_rejects_reserved() {
        let err = EventValidator::validate_citizen_name(FOUNDER_DISPLAY_NAME, &[]).unwrap_err();
        assert!(matches!(err, ValidationError::InvalidCitizenName(_)));
    }
}
