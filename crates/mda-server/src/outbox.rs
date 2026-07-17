//! Outbox drain worker (PLAN §5.9.4 / §5.18): claims pending `sys_outbox` rows
//! and turns them into side-effects. Phase-1-of-this: `workflow.transitioned`
//! events become in-app `sys_notification` rows (email/SMS/push channels are
//! follow-ups). At-least-once; idempotent on the outbox row id.
//!
//! Poison messages are moved to `status = 'dead'` after [`MAX_RETRIES`] failures.

use std::time::Duration;

use sqlx::PgPool;

/// Maximum retry attempts before moving a failing item to the dead-letter queue.
const MAX_RETRIES: i32 = 10;

/// Spawn the background drain loop.
pub fn spawn_drain(pool: PgPool) {
    tokio::spawn(async move {
        tracing::info!("outbox drain worker started");
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Err(e) = drain_once(&pool).await {
                tracing::warn!(?e, "outbox drain pass failed");
            }
        }
    });
}

async fn drain_once(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    // claim a batch of pending rows
    let rows: Vec<(uuid::Uuid, uuid::Uuid, String, serde_json::Value)> = sqlx::query_as(
        "SELECT id, tenant_id, kind, payload FROM sys_outbox
          WHERE status = 'pending'
          ORDER BY created_at
          LIMIT 50
          FOR UPDATE SKIP LOCKED",
    )
    .fetch_all(&mut *tx)
    .await?;

    for (id, tenant, kind, payload) in rows {
        // Snapshot current attempts so the dead-letter decision is based on
        // the pre-increment count.
        let current_attempts: i32 = sqlx::query_scalar(
            "SELECT attempts FROM sys_outbox WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;

        let res = process(&mut tx, tenant, &kind, &payload).await;
        let (status, attempts_incr): (&str, i32) = match res {
            Ok(()) => ("done", 0),
            Err(e) => {
                tracing::warn!(?e, %kind, attempts = current_attempts, "outbox item failed");
                if current_attempts + 1 >= MAX_RETRIES {
                    tracing::error!(%kind, id = %id, attempts = current_attempts, "outbox item moved to dead-letter queue");
                    ("dead", 0)
                } else {
                    ("failed", 1)
                }
            }
        };
        sqlx::query(
            "UPDATE sys_outbox SET status = $2, attempts = attempts + $3,
                processed_at = CASE WHEN $2 IN ('done', 'dead') THEN now() ELSE processed_at END
              WHERE id = $1",
        )
        .bind(id)
        .bind(status)
        .bind(attempts_incr)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Turn one outbox row into side-effect(s). Currently: workflow.transitioned →
/// an in-app notification addressed to the actor.
async fn process(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: uuid::Uuid,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    match kind {
        "workflow.transitioned" => {
            let user = payload
                .get("actor")
                .and_then(|v| v.as_str())
                .and_then(|s| uuid::Uuid::parse_str(s).ok());
            let entity = payload.get("entity").and_then(|v| v.as_str());
            let record = payload
                .get("record_id")
                .and_then(|v| v.as_str())
                .and_then(|s| uuid::Uuid::parse_str(s).ok());
            if let Some(user) = user {
                sqlx::query(
                    "INSERT INTO sys_notification (tenant_id, user_id, type, entity, record_id, payload)
                     VALUES ($1, $2, $3, $4, $5, $6)
                     ON CONFLICT DO NOTHING",
                )
                .bind(tenant)
                .bind(user)
                .bind(kind)
                .bind(entity)
                .bind(record)
                .bind(payload)
                .execute(&mut **tx)
                .await?;
            }
            Ok(())
        }
        other => {
            tracing::debug!(kind = other, "no drain handler for outbox kind");
            Ok(())
        }
    }
}
