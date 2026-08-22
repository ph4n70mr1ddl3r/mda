//! Outbox drain worker (PLAN §5.9.4 / §5.18): claims pending `sys_outbox` rows
//! and turns them into side-effects. Kinds handled:
//! - `workflow.transitioned` → an in-app notification to the actor (legacy).
//! - `notification.fanout` → full multi-channel fan-out (§5.18): type lookup,
//!   per-user preferences, in-app + email (+ webhook, added by §5.21) delivery.
//! - `webhook.deliver` → outbound signed delivery (§5.21, wired in that slice).
//!
//! At-least-once; idempotent on the outbox row id. A failed row is retried with
//! exponential backoff (measured from its last attempt); after [`MAX_RETRIES`]
//! failures it moves to `status = 'dead'` — the dead-letter queue (§5.9.4).

use std::sync::Arc;
use std::time::Duration;

use mda_api::notifications::{self, Channel};
use mda_api::webhooks;
use mda_core::SecretStore;
use sqlx::PgPool;

/// Maximum retry attempts before moving a failing item to the dead-letter queue.
pub const MAX_RETRIES: i32 = 10;

/// Delay before the FIRST retry of a failed item; doubles per subsequent
/// attempt (15s → 30s → 60s …), capped at [`RETRY_CAP_SECS`], with ±50 %
/// jitter (§5.9.4: "exponential backoff + jitter") so a fleet of rows that
/// failed together — an SMTP blip failing a whole batch — doesn't retry in
/// lockstep. Measured from `processed_at` (the last-attempt timestamp) — the
/// attempt itself stamps it, so every retry reschedules from its own failure.
pub const RETRY_BASE_SECS: i64 = 15;

/// Upper bound on the retry backoff.
pub const RETRY_CAP_SECS: i64 = 900;

/// Spawn the background drain loop with the default channel set (in-app + email)
/// and a default secret store + HTTP client.
pub fn spawn_drain(pool: PgPool) {
    let secrets: Arc<dyn SecretStore> = Arc::new(mda_api::secrets::LocalSecretStore::from_env());
    spawn_drain_with(
        pool,
        notifications::default_channels(),
        secrets,
        mda_integration::net::egress_client(),
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
            // drain first, then sleep: a freshly-enqueued row is processed on
            // the next runtime tick instead of waiting a full poll interval.
            // (Particularly matters at startup and in tests.)
            if let Err(e) = drain_once(&pool, &channels, secrets.as_ref(), &http).await {
                tracing::warn!(?e, "outbox drain pass failed");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

/// Claim and process one batch. Claimable rows are `pending`, plus `failed`
/// rows whose backoff has elapsed and that have retries left — a failure must
/// never strand a side-effect (at-least-once, §5.9.4). `pub` as the ops/test
/// entry point (the spawned loop is just this + sleep).
///
/// A `failed` row with NULL `processed_at` (the shape every row parked by the
/// pre-retry code has — it only stamped `done`/`dead`) counts as backoff-
/// elapsed: it is retried once immediately and stamped, then normal backoff
/// applies. The NULL-tolerant arm also means no future writer can strand a
/// row by leaving the timestamp unset.
pub async fn drain_once(
    pool: &PgPool,
    channels: &[Box<dyn Channel>],
    secrets: &dyn SecretStore,
    http: &reqwest::Client,
) -> Result<(), sqlx::Error> {
    let dc = DeliveryCtx {
        pool,
        channels,
        secrets,
        http,
    };
    let mut tx = pool.begin().await?;
    // Claim a batch and snapshot `attempts` in the same locked read. The rows
    // are held FOR UPDATE within this transaction, so the pre-increment count
    // used for the dead-letter decision cannot change underneath us — no need
    // for a per-row follow-up query (avoids an N+1).
    let rows: Vec<(uuid::Uuid, uuid::Uuid, String, serde_json::Value, i32)> = sqlx::query_as(
        "SELECT id, tenant_id, kind, payload, attempts FROM sys_outbox
          WHERE status = 'pending'
             OR (status = 'failed'
                 AND attempts < $1
                 AND (processed_at IS NULL OR processed_at < now()
                        - make_interval(secs => LEAST($2 * POWER(2, GREATEST(attempts - 1, 0))
                                                     * (0.5 + random()), $3)::int)))
          ORDER BY created_at
          LIMIT 50
          FOR UPDATE SKIP LOCKED",
    )
    .bind(MAX_RETRIES)
    .bind(RETRY_BASE_SECS as f64)
    .bind(RETRY_CAP_SECS)
    .fetch_all(&mut *tx)
    .await?;

    for (id, tenant, kind, payload, current_attempts) in rows {
        // Handlers run against the pool (their own transactions); the row lock
        // in `tx` simply reserves the row for this pass. `id` rides along so
        // every durable row a handler writes can key off it (replay-safe).
        let res = process(&dc, id, tenant, &kind, &payload).await;
        let (status, attempts_incr): (&str, i32) = match res {
            Ok(()) => ("done", 0),
            Err(e) => {
                tracing::warn!(?e, %kind, attempts = current_attempts, "outbox item failed");
                if current_attempts + 1 >= MAX_RETRIES {
                    tracing::error!(%kind, id = %id, attempts = current_attempts + 1, "outbox item moved to dead-letter queue");
                    ("dead", 1)
                } else {
                    ("failed", 1)
                }
            }
        };
        sqlx::query(
            "UPDATE sys_outbox SET status = $2, attempts = attempts + $3,
                processed_at = now()
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

/// The fixed delivery context a drain pass carries (pool + channel set +
/// secrets + egress client); the per-row bits stay separate arguments.
struct DeliveryCtx<'a> {
    pool: &'a PgPool,
    channels: &'a [Box<dyn Channel>],
    secrets: &'a dyn SecretStore,
    http: &'a reqwest::Client,
}

/// Turn one outbox row into side-effect(s). `id` is the outbox row's id —
/// handlers derive their durable rows' ids from it so an at-least-once replay
/// is a no-op, not a duplicate.
async fn process(
    dc: &DeliveryCtx<'_>,
    id: uuid::Uuid,
    tenant: uuid::Uuid,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    let DeliveryCtx {
        pool,
        channels,
        secrets,
        http,
    } = *dc;
    match kind {
        "notification.fanout" => {
            // full multi-channel fan-out (§5.18). Errors are logged inside
            // fanout per-channel; a row-level error propagates to retry. The
            // row id makes every channel row idempotent (see `fanout`).
            notifications::fanout(pool, channels, payload, id)
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
                            pool,
                            tenant,
                            &flow.entity,
                        )
                        .await
                        {
                            Ok(id) => id,
                            Err(e) => {
                                tracing::warn!(?e, entity = %flow.entity, "inbound flow target entity missing");
                                let _ = mda_integration::record_failure(
                                    pool,
                                    tenant,
                                    &flow,
                                    &e.to_string(),
                                )
                                .await;
                                return Err(sqlx::Error::Configuration(e.to_string().into()));
                            }
                        };
                        let def = mda_meta::loader::load_entity_definition(pool, tenant, entity_id)
                            .await
                            .map_err(|e| sqlx::Error::Configuration(e.to_string().into()))?;
                        if let Err(e) = mda_integration::run_inbound(
                            pool,
                            &def,
                            &flow,
                            &external,
                            uuid::Uuid::nil(),
                        )
                        .await
                        {
                            tracing::warn!(?e, "inbound flow run failed");
                            // a filtered record is expected (not a poison message).
                            let is_filtered = matches!(e, mda_core::Error::Invalid(ref m) if m.contains("filtered"));
                            if !is_filtered {
                                let _ = mda_integration::record_failure(
                                    pool,
                                    tenant,
                                    &flow,
                                    &e.to_string(),
                                )
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
            webhooks::deliver(pool, secrets, http, payload)
                .await
                .map_err(|e| {
                    tracing::warn!(?e, "webhook delivery failed");
                    sqlx::Error::Configuration(e.to_string().into())
                })?;
            Ok(())
        }
        "workflow.transitioned" => {
            // legacy: an in-app notification addressed to the actor. The id is
            // derived from (outbox row, actor) so a replay of the row is a
            // no-op — the ON CONFLICT below actually has a key to land on.
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
                let nid = notifications::delivery_row_id(id, user, b"transitioned");
                sqlx::query(
                    "INSERT INTO sys_notification (id, tenant_id, user_id, type, entity, record_id, payload)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)
                     ON CONFLICT (id) DO NOTHING",
                )
                .bind(nid)
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
