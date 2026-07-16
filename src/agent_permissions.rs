//! Admin API for individual Agent permissions.

use std::sync::Arc;

use axum::{
    extract::{Extension, Path, State},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::auth::{
    grant_agent_permission, is_valid_agent_permission, list_agent_permissions,
    revoke_agent_permission, AuthContext,
};
use crate::response::{self, ApiResponse};
use crate::types::Role;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct GrantPermissionsRequest {
    /// One or more permission strings to grant.
    pub permissions: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct PermissionsListResponse {
    pub agent_id: String,
    pub permissions: Vec<String>,
}

fn require_admin(auth: &AuthContext) -> Option<axum::response::Response> {
    if !auth.can_manage_api_keys() {
        return Some(response::forbidden(
            "Only Aiya and Founder can manage agent permissions",
        ));
    }
    None
}

/// Ensure the target citizen exists and has role Agent (includes digital Agents).
async fn load_target_agent(
    pool: &sqlx::PgPool,
    id: &str,
) -> Result<(String, String), axum::response::Response> {
    let row: Option<(String, String, bool)> = sqlx::query_as(
        "SELECT id, role, is_digital FROM citizens WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|e| response::internal_error(format!("Database error: {e}")))?;

    let Some((citizen_id, role_str, _is_digital)) = row else {
        return Err(response::not_found("Citizen not found"));
    };

    let role = Role::from_str(&role_str).unwrap_or(Role::Citizen);
    if !role.is_agent() {
        return Err(response::bad_request(
            "Target citizen must have role Agent",
        ));
    }

    Ok((citizen_id, role_str))
}

/// GET /admin/agent/:id/permissions
pub async fn list_permissions_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = require_admin(&auth) {
        return resp;
    }

    let (agent_id, _) = match load_target_agent(&state.db, &id).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    match list_agent_permissions(&state.db, &agent_id).await {
        Ok(permissions) => Json(ApiResponse::success(PermissionsListResponse {
            agent_id,
            permissions,
        }))
        .into_response(),
        Err(e) => response::internal_error(format!("Failed to list permissions: {e}")),
    }
}

/// POST /admin/agent/:id/permissions
pub async fn grant_permissions_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<GrantPermissionsRequest>,
) -> impl IntoResponse {
    if let Some(resp) = require_admin(&auth) {
        return resp;
    }

    if req.permissions.is_empty() {
        return response::bad_request("permissions must not be empty");
    }

    let invalid: Vec<&str> = req
        .permissions
        .iter()
        .map(String::as_str)
        .filter(|p| !is_valid_agent_permission(p))
        .collect();
    if !invalid.is_empty() {
        return response::bad_request(format!(
            "Invalid permission(s): {}. Allowed: chat.read, chat.send, chat.manage, blockchain.read",
            invalid.join(", ")
        ));
    }

    let (agent_id, _) = match load_target_agent(&state.db, &id).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let mut granted = Vec::new();
    let mut already = Vec::new();
    for perm in &req.permissions {
        match grant_agent_permission(
            &state.db,
            &agent_id,
            perm,
            Some(&auth.citizen_name),
        )
        .await
        {
            Ok(true) => granted.push(perm.clone()),
            Ok(false) => already.push(perm.clone()),
            Err(e) => {
                return response::internal_error(format!("Failed to grant permission: {e}"))
            }
        }
    }

    let all = match list_agent_permissions(&state.db, &agent_id).await {
        Ok(p) => p,
        Err(e) => {
            return response::internal_error(format!("Failed to list permissions: {e}"))
        }
    };

    Json(ApiResponse::success(serde_json::json!({
        "agent_id": agent_id,
        "granted": granted,
        "already_present": already,
        "permissions": all,
    })))
    .into_response()
}

/// DELETE /admin/agent/:id/permissions/:perm
pub async fn revoke_permission_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((id, perm)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Some(resp) = require_admin(&auth) {
        return resp;
    }

    if !is_valid_agent_permission(&perm) {
        return response::bad_request(format!(
            "Invalid permission: {perm}. Allowed: chat.read, chat.send, chat.manage, blockchain.read"
        ));
    }

    let (agent_id, _) = match load_target_agent(&state.db, &id).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    match revoke_agent_permission(&state.db, &agent_id, &perm).await {
        Ok(true) => {
            let remaining = list_agent_permissions(&state.db, &agent_id)
                .await
                .unwrap_or_default();
            Json(ApiResponse::success(serde_json::json!({
                "agent_id": agent_id,
                "revoked": perm,
                "permissions": remaining,
            })))
            .into_response()
        }
        Ok(false) => response::not_found(format!(
            "Permission '{perm}' was not granted to this agent"
        )),
        Err(e) => response::internal_error(format!("Failed to revoke permission: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{
        grant_agent_permission, grant_default_agent_permissions, has_agent_permission,
        list_agent_permissions, revoke_agent_permission, PERM_CHAT_MANAGE, PERM_CHAT_READ,
        PERM_CHAT_SEND,
    };
    use crate::types::Role;
    use std::time::Duration;

    #[test]
    fn admin_gate_allows_founder_aiya_only() {
        assert!(require_admin(&AuthContext::from_api_key(
            "f".into(),
            Role::Founder
        ))
        .is_none());
        assert!(require_admin(&AuthContext::from_api_key("a".into(), Role::Aiya)).is_none());
        assert!(require_admin(&AuthContext::from_api_key("g".into(), Role::Guardian)).is_some());
        assert!(require_admin(&AuthContext::from_api_key(
            "bot".into(),
            Role::Agent
        ))
        .is_some());
    }

    #[test]
    fn grant_request_deserializes() {
        let raw = r#"{"permissions":["chat.read","chat.send"]}"#;
        let req: GrantPermissionsRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.permissions, vec![PERM_CHAT_READ, PERM_CHAT_SEND]);
    }

    async fn test_pool() -> Option<sqlx::PgPool> {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://quazar:quazar@localhost:5432/quazar".to_string());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(2))
            .connect(&url)
            .await
            .ok()
    }

    #[tokio::test]
    async fn grant_revoke_and_access_roundtrip() {
        let Some(pool) = test_pool().await else {
            eprintln!("skipping agent_permissions DB test: no postgres");
            return;
        };
        let _ = sqlx::migrate!("./migrations").run(&pool).await;

        let agent_id = format!("agent-perm-{}", uuid::Uuid::new_v4());
        let name = format!("agentperm{}", uuid::Uuid::new_v4().simple());
        sqlx::query(
            r#"
            INSERT INTO citizens (id, name, public_key, status, role, created_at, passport_issued, is_digital)
            VALUES ($1, $2, 'pk', 'active', 'Agent', EXTRACT(EPOCH FROM NOW())::BIGINT, FALSE, TRUE)
            "#,
        )
        .bind(&agent_id)
        .bind(&name)
        .execute(&pool)
        .await
        .expect("insert agent citizen");

        // Deny by default
        assert!(!has_agent_permission(&pool, &agent_id, PERM_CHAT_READ).await);
        assert!(!has_agent_permission(&pool, &agent_id, PERM_CHAT_SEND).await);

        let denied = AuthContext::from_api_key(name.clone(), Role::Agent)
            .with_citizen_id(agent_id.clone());
        assert!(!denied.can_read_chat());
        assert!(!denied.can_send_chat());

        // Grant defaults
        let n = grant_default_agent_permissions(&pool, &agent_id, Some("test"))
            .await
            .expect("grant defaults");
        assert_eq!(n, 2);
        assert!(has_agent_permission(&pool, &agent_id, PERM_CHAT_READ).await);
        assert!(has_agent_permission(&pool, &agent_id, PERM_CHAT_SEND).await);
        assert!(!has_agent_permission(&pool, &agent_id, PERM_CHAT_MANAGE).await);

        let perms = list_agent_permissions(&pool, &agent_id).await.unwrap();
        let allowed = AuthContext::from_api_key(name.clone(), Role::Agent)
            .with_citizen_id(agent_id.clone())
            .with_agent_permissions(perms);
        assert!(allowed.can_read_chat());
        assert!(allowed.can_send_chat());
        assert!(!allowed.can_manage_chat());

        // Idempotent grant
        assert!(
            !grant_agent_permission(&pool, &agent_id, PERM_CHAT_READ, Some("test"))
                .await
                .unwrap()
        );

        // Revoke send
        assert!(
            revoke_agent_permission(&pool, &agent_id, PERM_CHAT_SEND)
                .await
                .unwrap()
        );
        assert!(!has_agent_permission(&pool, &agent_id, PERM_CHAT_SEND).await);
        assert!(has_agent_permission(&pool, &agent_id, PERM_CHAT_READ).await);

        let after = list_agent_permissions(&pool, &agent_id).await.unwrap();
        let partial = AuthContext::from_api_key(name, Role::Agent)
            .with_citizen_id(agent_id.clone())
            .with_agent_permissions(after);
        assert!(partial.can_read_chat());
        assert!(!partial.can_send_chat());

        // Cleanup
        let _ = sqlx::query("DELETE FROM citizens WHERE id = $1")
            .bind(&agent_id)
            .execute(&pool)
            .await;
    }
}

