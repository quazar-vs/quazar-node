use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NodeStatus {
    Alive,
    Dead,
    Syncing,
}

impl std::fmt::Display for NodeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            NodeStatus::Alive => "alive",
            NodeStatus::Dead => "dead",
            NodeStatus::Syncing => "syncing",
        })
    }
}

impl From<String> for NodeStatus {
    fn from(s: String) -> Self {
        match s.as_str() {
            "alive" => NodeStatus::Alive,
            "dead" => NodeStatus::Dead,
            "syncing" => NodeStatus::Syncing,
            _ => NodeStatus::Dead,
        }
    }
}

fn default_last_seen() -> DateTime<Utc> {
    Utc::now()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub url: String,
    pub public_key: Option<String>,
    pub status: NodeStatus,
    #[serde(default = "default_last_seen")]
    pub last_seen: DateTime<Utc>,
    pub version: String,
}

#[derive(Clone)]
pub struct NodeRegistry {
    pool: PgPool,
}

impl NodeRegistry {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn upsert_node(&self, node: &Node) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            r#"
            INSERT INTO nodes (id, url, public_key, status, last_seen, version, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, COALESCE((SELECT created_at FROM nodes WHERE id = $1), $7))
            ON CONFLICT (id) DO UPDATE SET
                url = CASE
                    WHEN EXCLUDED.url IS NULL OR TRIM(EXCLUDED.url) = '' THEN nodes.url
                    ELSE EXCLUDED.url
                END,
                public_key = CASE
                    WHEN EXCLUDED.public_key IS NULL OR TRIM(EXCLUDED.public_key) = '' THEN nodes.public_key
                    ELSE EXCLUDED.public_key
                END,
                status = EXCLUDED.status,
                last_seen = EXCLUDED.last_seen,
                version = EXCLUDED.version
            "#,
        )
        .bind(&node.id)
        .bind(&node.url)
        .bind(node.public_key.clone().unwrap_or_default())
        .bind(node.status.to_string())
        .bind(node.last_seen)
        .bind(&node.version)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_all_nodes(&self) -> Result<Vec<Node>, sqlx::Error> {
        let rows = sqlx::query_as::<_, NodeRow>(
            "SELECT id, url, public_key, status, last_seen, version FROM nodes ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Node::from).collect())
    }

    /// One random peer with status `alive` (online for P2P), excluding `exclude_id`.
    pub async fn pick_random_alive_peer_excluding(
        &self,
        exclude_id: &str,
    ) -> Result<Option<Node>, sqlx::Error> {
        let row = sqlx::query_as::<_, NodeRow>(
            r#"
            SELECT id, url, public_key, status, last_seen, version
            FROM nodes
            WHERE status = 'alive' AND id <> $1 AND TRIM(url) <> ''
            ORDER BY RANDOM()
            LIMIT 1
            "#,
        )
        .bind(exclude_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(Node::from))
    }
}

/// Peer address as in `QUAZAR_BOOTSTRAP_PEERS` (`id@url`).
pub fn format_peer_address(id: &str, url: &str) -> String {
    format!("{}@{}", id.trim(), url.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_peer_address_matches_bootstrap_style() {
        assert_eq!(
            format_peer_address("QZ-NODE-2", "http://node:8080"),
            "QZ-NODE-2@http://node:8080"
        );
    }
}

#[derive(sqlx::FromRow)]
struct NodeRow {
    id: String,
    url: String,
    public_key: String,
    status: String,
    last_seen: DateTime<Utc>,
    version: String,
}

impl From<NodeRow> for Node {
    fn from(row: NodeRow) -> Self {
        Node {
            id: row.id,
            url: row.url,
            public_key: if row.public_key.is_empty() {
                None
            } else {
                Some(row.public_key)
            },
            status: NodeStatus::from(row.status),
            last_seen: row.last_seen,
            version: row.version,
        }
    }
}
