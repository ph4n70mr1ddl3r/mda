//! Outbox drain worker (PLAN §5.9.4 / §5.18): claims pending `sys_outbox` rows
//! and turns them into side-effects. Kinds handled:
//! - `workflow.transitioned` → an in-app notification to the actor (legacy).
//! - `notification.fanout` → full multi-channel fan-out (§5.18): type lookup,
//!   per-user preferences, in-app + email (+ webhook, added by §5.21) delivery.
//! - `webhook.deliver` → outbound signed delivery (§5.21, wired in that slice).
//!
//! At-least-once; idempotent on the outbox row id. Poison messages move to
//! `status = 'dead'` after [`MAX_RETRIES`] failures.

use std::sync::Arc;
use std::time::Duration;

use mda_api::notifications::{self, Channel};
use mda_api::webhooks;
use mda_core::SecretStore;
use sqlx::PgPool;

/// Maximum retry attempts before moving a failing item to the dead-letter queue.
const MAX_RETRIES: i32 = 10;

/// Spawn the background drain loop with the default channel set (in-app + email)
/// and a default secret store + HTTP client.
pub fn spawn_drain(pool: PgPool) {
    let secrets: Arc<dyn SecretStore> =
        Arc::new(mda_api::secrets::LocalSecretStore::from_env());
    spawn_drain_with(
        pool,
        notifications::default_channels(),
        secrets,
        reqwest::Client::new(),
    );
}

/// Spawn the drain loop with an explicit channel set, secret store, and HTTP
/// client (used to add the webhook channel alongside §5.21 and to inject a
/// test secret store / client).
pub fn spawn_drain_with(
    pool: PgPool,
    channels: Vec<Box<dyn Channel>>,
    secrets: Arc<dyn SecretStore>,
    http: reqwest::Client,
) {
    tokio::spawn(async move {
        tracing::info!("outbox drain worker started");
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Err(e) = drain_once(&pool, &channels, secrets.as_ref(), &http).await {
                tracing::warn!(?e, "outbox drain pass failed");
            }
        }
    });
}

async fn drain_once(
    pool: &PgPool,
    channels: &[Box<dyn Channel>],
    secrets: &dyn SecretStore,
    http: &reqwest::Client,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    // Claim a batch and snapshot `attempts` in the same locked read. The rows
    // are held FOR UPDATE within this transaction, so the pre-increment count
    // used for the dead-letter decision cannot change underneath us — no need
    // for a per-row follow-up query (avoids an N+1).
    let rows: Vec<(uuid::Uuid, uuid::Uuid, String, serde_json::Value, i32)> = sqlx::query_as(
        "SELECT id, tenant_id, kind, payload, attempts FROM sys_outbox
          WHERE status = 'pending'
          ORDER BY created_at
          LIMIT 50
          FOR UPDATE SKIP LOCKED",
    )
    .fetch_all(&mut *tx)
    .await?;

    for (id, tenant, kind, payload, current_attempts) in rows {
        // Handlers run against the pool (their own transactions); the row lock
        // in `tx` simply reserves the row for this pass.
        let res = process(pool, channels, secrets, http, tenant, &kind, &payload).await;
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

/// Turn one outbox row into side-effect(s).
async fn process(
    pool: &PgPool,
    channels: &[Box<dyn Channel>],
    secrets: &dyn SecretStore,
    http: &reqwest::Client,
    tenant: uuid::Uuid,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    match kind {
        "notification.fanout" => {
            // full multi-channel fan-out (§5.18). Errors are logged inside
            // fanout per-channel; a row-level error propagates to retry.
            notifications::fanout(pool, channels, payload)
                .await
                .map_err(|e| {
                    tracing::warn!(?e, "notification fanout failed");
                    sqlx::Error::Configuration(e.to_string().into())
                })?;
            Ok(())
        }
        "integration.inbound" => {
            // a webhook receiver (§5.21) verified + enqueued an external event;
            // find the inbound flow bound to that webhook and materialize it.
            let webhook_id = payload
                .get("webhook_id")
                .and_then(|v| v.as_str())
                .and_then(|s| uuid::Uuid::parse_str(s).ok());
            let external = payload
                .get("payload")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            if let Some(webhook_id) = webhook_id {
                match mda_integration::flow_for_webhook(pool, tenant, webhook_id).await {
                    Ok(Some(flow)) => {
                        let entity_id = match mda_meta::loader::entity_id_by_name(
                            pool, tenant, &flow.entity,
                        )
                        .await
                        {
                            Ok(id) => id,
                            Err(e) => {
                                tracing::warn!(?e, entity = %flow.entity, "inbound flow target entity missing");
                                let _ = mda_integration::record_failure(
                                    pool, tenant, &flow, &e.to_string(),
                                )
                                .await;
                                return Err(sqlx::Error::Configuration(e.to_string().into()));
                            }
                        };
                        let def = mda_meta::loader::load_entity_definition(pool, tenant, entity_id)
                            .await
                            .map_err(|e| sqlx::Error::Configuration(e.to_string().into()))?;
                        if let Err(e) =
                            mda_integration::run_inbound(pool, &def, &flow, &external, uuid::Uuid::nil())
                                .await
                        {
                            tracing::warn!(?e, "inbound flow run failed");
                            // a filtered record is expected (not a poison message).
                            let is_filtered = matches!(e, mda_core::Error::Invalid(ref m) if m.contains("filtered"));
                            if !is_filtered {
                                let _ =
                                    mda_integration::record_failure(pool, tenant, &flow, &e.to_string())
                                        .await;
                                return Err(sqlx::Error::Configuration(e.to_string().into()));
                            }
                        }
                        Ok(())
                    }
                    Ok(None) => {
                        tracing::debug!(%webhook_id, "no inbound flow bound to webhook; skipping");
                        Ok(())
                    }
                    Err(e) => Err(sqlx::Error::Configuration(e.to_string().into())),
                }
            } else {
                Ok(())
            }
        }
        "webhook.deliver" => {
            // outbound signed delivery (§5.21).
            webhooks::deliver(pool, secrets, http, payload).await.map_err(|e| {
                tracing::warn!(?e, "webhook delivery failed");
                sqlx::Error::Configuration(e.to_string().into())
            })?;
            Ok(())
        }
        "workflow.transitioned" => {
            // legacy: an in-app notification addressed to the actor.
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
                .execute(pool)
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
