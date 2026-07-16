use sqlx::{PgPool, Postgres, Transaction};

use crate::models::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingInsertResult {
    Inserted,
    AlreadyExists,
}

pub async fn insert(pool: &PgPool, event: &Event) -> Result<PendingInsertResult, String> {
    let data = serde_json::to_string(event).map_err(|e| e.to_string())?;
    let result = sqlx::query(
        "INSERT INTO pending_events (event_id, event_data) VALUES ($1, $2)
         ON CONFLICT (event_id) DO NOTHING",
    )
    .bind(&event.event_id)
    .bind(data)
    .execute(pool)
    .await
    .map_err(|e| format!("Failed to insert pending event {}: {}", event.event_id, e))?;

    if result.rows_affected() == 0 {
        Ok(PendingInsertResult::AlreadyExists)
    } else {
        Ok(PendingInsertResult::Inserted)
    }
}

pub async fn fetch_all(pool: &PgPool) -> Result<Vec<Event>, String> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT event_data FROM pending_events ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(rows
        .iter()
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect())
}

/// Take the first `n` pending events in id order, locking rows (`FOR UPDATE`).
pub async fn fetch_n(pool: &PgPool, n: usize) -> Result<Vec<Event>, String> {
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    let events = fetch_n_in_tx(&mut tx, n).await?;
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(events)
}

/// Same as [`fetch_n`], but within an existing transaction (holds row locks until commit/rollback).
pub async fn fetch_n_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    n: usize,
) -> Result<Vec<Event>, String> {
    if n == 0 {
        return Ok(Vec::new());
    }
    let rows: Vec<(i64, String)> = sqlx::query_as(
        r#"
        SELECT id, event_data
        FROM pending_events
        ORDER BY id
        FOR UPDATE
        LIMIT $1
        "#,
    )
    .bind(n as i64)
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| e.to_string())?;

    let mut events = Vec::with_capacity(rows.len());
    for (_id, data) in rows {
        let event: Event = serde_json::from_str(&data)
            .map_err(|e| format!("Failed to deserialize pending event: {e}"))?;
        events.push(event);
    }
    Ok(events)
}

pub async fn exists(pool: &PgPool, event_id: &str) -> Result<bool, String> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pending_events WHERE event_id = $1)")
        .bind(event_id)
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())
}

pub async fn count(pool: &PgPool) -> Result<i64, String> {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_events")
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())
}

/// Age in seconds of the oldest pending row (`created_at`), if any.
pub async fn oldest_age_secs(pool: &PgPool) -> Result<Option<i64>, String> {
    let oldest: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT MIN(created_at) FROM pending_events")
            .fetch_one(pool)
            .await
            .map_err(|e| e.to_string())?;
    Ok(oldest.map(|ts| (chrono::Utc::now() - ts).num_seconds().max(0)))
}

pub async fn delete_by_ids(pool: &PgPool, event_ids: &[String]) -> Result<(), String> {
    for event_id in event_ids {
        sqlx::query("DELETE FROM pending_events WHERE event_id = $1")
            .bind(event_id)
            .execute(pool)
            .await
            .map_err(|e| format!("Failed to delete pending event {}: {}", event_id, e))?;
    }
    Ok(())
}

pub async fn delete_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    event_ids: &[String],
) -> Result<(), String> {
    for event_id in event_ids {
        sqlx::query("DELETE FROM pending_events WHERE event_id = $1")
            .bind(event_id)
            .execute(&mut **tx)
            .await
            .map_err(|e| format!("Failed to delete pending event {}: {}", event_id, e))?;
    }
    Ok(())
}
