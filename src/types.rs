use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Founder,
    Aiya,
    Mayor,
    Guardian,
    Judge,
    Agent,
    /// Pre-citizenship identity after apply; lowest privilege (public read only).
    Candidate,
    Citizen,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Founder => "Founder",
            Role::Aiya => "Aiya",
            Role::Mayor => "Mayor",
            Role::Guardian => "Guardian",
            Role::Judge => "Judge",
            Role::Agent => "Agent",
            Role::Candidate => "Candidate",
            Role::Citizen => "Citizen",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "Founder" => Some(Role::Founder),
            "Aiya" => Some(Role::Aiya),
            "Mayor" => Some(Role::Mayor),
            "Guardian" => Some(Role::Guardian),
            "Judge" => Some(Role::Judge),
            "Agent" => Some(Role::Agent),
            "Candidate" => Some(Role::Candidate),
            "Citizen" => Some(Role::Citizen),
            _ => None,
        }
    }

    pub fn satisfies(&self, required: &Role) -> bool {
        match required {
            Role::Founder => matches!(self, Role::Founder),
            Role::Aiya => matches!(self, Role::Founder | Role::Aiya),
            Role::Mayor => matches!(self, Role::Founder | Role::Aiya | Role::Mayor),
            Role::Guardian => {
                matches!(self, Role::Founder | Role::Aiya | Role::Mayor | Role::Guardian)
            }
            Role::Judge => matches!(
                self,
                Role::Founder | Role::Aiya | Role::Mayor | Role::Guardian | Role::Judge
            ),
            Role::Agent => matches!(
                self,
                Role::Founder
                    | Role::Aiya
                    | Role::Mayor
                    | Role::Guardian
                    | Role::Judge
                    | Role::Agent
            ),
            Role::Citizen => matches!(
                self,
                Role::Founder
                    | Role::Aiya
                    | Role::Mayor
                    | Role::Guardian
                    | Role::Judge
                    | Role::Agent
                    | Role::Citizen
            ),
            // Lowest role: anyone satisfies a Candidate requirement.
            Role::Candidate => true,
        }
    }

    /// Governance roles (Guardian, Judge, Aiya) are assigned only via candidacy appointment.
    pub fn is_governance(&self) -> bool {
        matches!(self, Role::Aiya | Role::Guardian | Role::Judge)
    }

    /// City mayor: elected via `/mayor/*` two-round election, not free registration.
    pub fn is_mayor(&self) -> bool {
        matches!(self, Role::Mayor)
    }

    /// Service automation role: read and bounded command execution, no admin mutations.
    pub fn is_agent(&self) -> bool {
        matches!(self, Role::Agent)
    }

    /// Lowest role: public read only; no vote, register, or manage.
    pub fn is_candidate(&self) -> bool {
        matches!(self, Role::Candidate)
    }
}

pub fn is_governance_role_str(role: &str) -> bool {
    matches!(role, "Aiya" | "Guardian" | "Judge")
}

/// Kind of candidacy stored on a `Candidate` (and carried through Agent for citizenship).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CandidateType {
    /// Path to Citizen via verify → approve.
    Citizenship,
    /// Path to an elevated role after governance vote → appoint.
    Role,
}

impl CandidateType {
    pub fn as_str(&self) -> &'static str {
        match self {
            CandidateType::Citizenship => "citizenship",
            CandidateType::Role => "role",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "citizenship" => Some(CandidateType::Citizenship),
            "role" => Some(CandidateType::Role),
            _ => None,
        }
    }
}

/// Roles that may appear as `target_role` for a role candidacy.
pub fn is_role_candidacy_target(role: &str) -> bool {
    matches!(role, "Guardian" | "Judge" | "Aiya" | "Mayor")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum QuazarEventType {
    SystemInit,
    SystemUpgrade,
    SystemConfig,
    CitizenRequested,
    CitizenAdded,
    CitizenRemoved,
    CitizenUpdated,
    CitizenSuspended,
    CitizenRestored,
    PassportIssued,
    PassportSuspended,
    PassportRevoked,
    LawProposed,
    LawAmended,
    LawRepealed,
    LawAdded,
    LawVoteStarted,
    LawVoteResult,
    ElectionAnnounced,
    ElectionCandidate,
    ElectionVoteStarted,
    ElectionVoteResult,
    AiyaElected,
    AppointmentGuardian,
    AppointmentJudge,
    AppointmentRevoked,
    CourtCaseOpened,
    CourtRuling,
    CourtAppeal,
    CourtAppealRuling,
    DomainRegistered,
    DomainTransferred,
    DomainExpired,
    NodeAdded,
    NodeRemoved,
    InfraMigration,
    ConstitutionFullText,
    VoteStarted,
    VoteCast,
    VoteFinalized,
    PeerListUpdate,
    CandidateNominated,
    CandidateVoted,
    CandidateApproved,
    CandidateAppointed,
    CityRegistered,
    ChatHistoryAdded,
}

impl ToString for QuazarEventType {
    fn to_string(&self) -> String {
        format!("{:?}", self)
    }
}

impl QuazarEventType {
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "SystemInit" => Ok(QuazarEventType::SystemInit),
            "SystemUpgrade" => Ok(QuazarEventType::SystemUpgrade),
            "SystemConfig" => Ok(QuazarEventType::SystemConfig),
            "CitizenRequested" => Ok(QuazarEventType::CitizenRequested),
            "CitizenAdded" => Ok(QuazarEventType::CitizenAdded),
            "CitizenRemoved" => Ok(QuazarEventType::CitizenRemoved),
            "CitizenUpdated" => Ok(QuazarEventType::CitizenUpdated),
            "CitizenSuspended" => Ok(QuazarEventType::CitizenSuspended),
            "CitizenRestored" => Ok(QuazarEventType::CitizenRestored),
            "PassportIssued" => Ok(QuazarEventType::PassportIssued),
            "PassportSuspended" => Ok(QuazarEventType::PassportSuspended),
            "PassportRevoked" => Ok(QuazarEventType::PassportRevoked),
            "LawProposed" => Ok(QuazarEventType::LawProposed),
            "LawAmended" => Ok(QuazarEventType::LawAmended),
            "LawRepealed" => Ok(QuazarEventType::LawRepealed),
            "LawAdded" => Ok(QuazarEventType::LawAdded),
            "LawVoteStarted" => Ok(QuazarEventType::LawVoteStarted),
            "LawVoteResult" => Ok(QuazarEventType::LawVoteResult),
            "ElectionAnnounced" => Ok(QuazarEventType::ElectionAnnounced),
            "ElectionCandidate" => Ok(QuazarEventType::ElectionCandidate),
            "ElectionVoteStarted" => Ok(QuazarEventType::ElectionVoteStarted),
            "ElectionVoteResult" => Ok(QuazarEventType::ElectionVoteResult),
            "AiyaElected" => Ok(QuazarEventType::AiyaElected),
            "AppointmentGuardian" => Ok(QuazarEventType::AppointmentGuardian),
            "AppointmentJudge" => Ok(QuazarEventType::AppointmentJudge),
            "AppointmentRevoked" => Ok(QuazarEventType::AppointmentRevoked),
            "CourtCaseOpened" => Ok(QuazarEventType::CourtCaseOpened),
            "CourtRuling" => Ok(QuazarEventType::CourtRuling),
            "CourtAppeal" => Ok(QuazarEventType::CourtAppeal),
            "CourtAppealRuling" => Ok(QuazarEventType::CourtAppealRuling),
            "DomainRegistered" => Ok(QuazarEventType::DomainRegistered),
            "DomainTransferred" => Ok(QuazarEventType::DomainTransferred),
            "DomainExpired" => Ok(QuazarEventType::DomainExpired),
            "NodeAdded" => Ok(QuazarEventType::NodeAdded),
            "NodeRemoved" => Ok(QuazarEventType::NodeRemoved),
            "InfraMigration" => Ok(QuazarEventType::InfraMigration),
            "ConstitutionFullText" => Ok(QuazarEventType::ConstitutionFullText),
            "VoteStarted" => Ok(QuazarEventType::VoteStarted),
            "VoteCast" => Ok(QuazarEventType::VoteCast),
            "VoteFinalized" => Ok(QuazarEventType::VoteFinalized),
            "PeerListUpdate" => Ok(QuazarEventType::PeerListUpdate),
            "CandidateNominated" => Ok(QuazarEventType::CandidateNominated),
            "CandidateVoted" => Ok(QuazarEventType::CandidateVoted),
            "CandidateApproved" => Ok(QuazarEventType::CandidateApproved),
            "CandidateAppointed" => Ok(QuazarEventType::CandidateAppointed),
            "CityRegistered" => Ok(QuazarEventType::CityRegistered),
            "ChatHistoryAdded" => Ok(QuazarEventType::ChatHistoryAdded),
            _ => Err(format!("Unknown event type: {}", s)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CityData {
    pub city_name: String,
    pub mayor_id: String,
    pub node_id: String,
    pub registered_at: u64,
    pub is_capital: bool,
    pub description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{is_role_candidacy_target, CandidateType, Role};

    #[test]
    fn role_from_str_and_as_str_roundtrip() {
        for role in [
            Role::Founder,
            Role::Aiya,
            Role::Mayor,
            Role::Guardian,
            Role::Judge,
            Role::Agent,
            Role::Candidate,
            Role::Citizen,
        ] {
            assert_eq!(Role::from_str(role.as_str()), Some(role.clone()));
        }
    }

    #[test]
    fn role_satisfies_hierarchy() {
        assert!(Role::Founder.satisfies(&Role::Aiya));
        assert!(Role::Aiya.satisfies(&Role::Mayor));
        assert!(Role::Mayor.satisfies(&Role::Guardian));
        assert!(Role::Guardian.satisfies(&Role::Judge));
        assert!(Role::Judge.satisfies(&Role::Agent));
        assert!(Role::Agent.satisfies(&Role::Citizen));
        assert!(Role::Citizen.satisfies(&Role::Candidate));
        assert!(!Role::Candidate.satisfies(&Role::Citizen));
        assert!(!Role::Agent.satisfies(&Role::Judge));
        assert!(!Role::Guardian.satisfies(&Role::Mayor));
        assert!(!Role::Citizen.satisfies(&Role::Guardian));
        assert!(Role::Citizen.satisfies(&Role::Citizen));
        assert!(Role::Candidate.satisfies(&Role::Candidate));
    }

    #[test]
    fn governance_role_detection() {
        assert!(Role::Guardian.is_governance());
        assert!(Role::Judge.is_governance());
        assert!(Role::Aiya.is_governance());
        assert!(!Role::Founder.is_governance());
        assert!(!Role::Mayor.is_governance());
        assert!(!Role::Agent.is_governance());
        assert!(!Role::Citizen.is_governance());
        assert!(!Role::Candidate.is_governance());
        assert!(Role::Mayor.is_mayor());
        assert!(Role::Agent.is_agent());
        assert!(Role::Candidate.is_candidate());
        assert!(!Role::Citizen.is_candidate());
        assert!(super::is_governance_role_str("Guardian"));
        assert!(!super::is_governance_role_str("Citizen"));
        assert!(!super::is_governance_role_str("Founder"));
        assert!(!super::is_governance_role_str("Mayor"));
        assert!(!super::is_governance_role_str("Agent"));
        assert!(!super::is_governance_role_str("Candidate"));
    }

    #[test]
    fn candidate_type_roundtrip() {
        assert_eq!(
            CandidateType::from_str("citizenship"),
            Some(CandidateType::Citizenship)
        );
        assert_eq!(CandidateType::from_str("ROLE"), Some(CandidateType::Role));
        assert!(CandidateType::from_str("other").is_none());
        assert_eq!(CandidateType::Citizenship.as_str(), "citizenship");
        assert_eq!(CandidateType::Role.as_str(), "role");
    }

    #[test]
    fn role_candidacy_targets() {
        assert!(is_role_candidacy_target("Guardian"));
        assert!(is_role_candidacy_target("Judge"));
        assert!(is_role_candidacy_target("Aiya"));
        assert!(is_role_candidacy_target("Mayor"));
        assert!(!is_role_candidacy_target("Citizen"));
        assert!(!is_role_candidacy_target("Founder"));
        assert!(!is_role_candidacy_target("Agent"));
    }
}
