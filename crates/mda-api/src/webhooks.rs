//! Event & webhook contract (PLAN §5.21) + inbound verification (§14).
//!
//! **Outbound:** a versioned, HMAC-signed JSON envelope delivered to webhook
//! subscribers via the transactional outbox (at-least-once + DLQ). The contract
//! is structural — event *types* and payloads are metadata/extension-defined.
//! `event_id` is the idempotency key; `schema_version` lets consumers evolve.
//! Signing: `X-MDA-Signature: t=<unix_ts>,v1=<hex hmac>` over `"<t>.<body>"`,
//! guarding origin integrity + replay within a window.
//!
//! **Inbound (§14):** the receiver verifies the same signature scheme (shared
//! secret via [`SecretStore`]) + replay window, dedupes on
//! `(webhook_id, event_id)`, and records the payload for an integration flow
//! (§5.22) to consume.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use mda_core::{Error, Result, SecretStore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

/// Replay window (seconds) for inbound signature timestamps.
const REPLAY_WINDOW_SECS: i64 = 300;
/// The envelope contract version.
const SCHEMA_VERSION: u32 = 1;

type HmacSha256 = Hmac<Sha256>;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/webhooks", post(create_webhook).get(list_webhooks))
        .route("/api/webhooks/:id", get(get_webhook).delete(delete_webhook))
        .route("/api/webhooks/:id/deliveries", get(list_deliveries))
        .route("/api/webhooks/:id/replay", post(replay))
        // inbound receiver (§14): unauthenticated at the edge — verified by
        // the shared-secret signature in the headers.
        .route("/api/integrations/webhooks/:id", post(inbound))
}

// ===== envelope + signing =====

/// The outbound delivery envelope (§5.21).
#[derive(Debug, Serialize)]
pub struct Envelope {
    pub event_id: String,
    pub tenant_id: Uuid,
    pub schema_version: u32,
    pub r#type: String,
    pub entity: Option<String>,
    pub record_id: Option<Uuid>,
    pub occurred_at: String,
    pub actor: Option<Uuid>,
    pub data: Value,
}

/// Sign `body` (the serialized envelope) with `secret`. Returns the
/// `X-MDA-Signature` header value: `t=<unix_ts>,v1=<hex>`.
pub fn sign(secret: &[u8], unix_ts: i64, body: &str) -> String {
    let signed = format!("{unix_ts}.{body}");
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac key any length");
    mac.update(signed.as_bytes());
    let v1 = hex::encode(mac.finalize().into_bytes());
    format!("t={unix_ts},v1={v1}")
}

/// Verify an `X-MDA-Signature` header against `body` with `secret`. Returns the
/// parsed timestamp on success. Fails on missing/malformed header, bad MAC, or a
/// timestamp outside the replay window.
pub fn verify(secret: &[u8], header: &str, body: &str, now_unix: i64) -> Result<i64> {
    let mut t = None;
    let mut v1 = None;
    for part in header.split(',') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("t=") {
            t = rest.parse::<i64>().ok();
        } else if let Some(rest) = part.strip_prefix("v1=") {
            v1 = Some(rest);
        }
    }
    let ts = t.ok_or_else(|| Error::Invalid("signature missing t=".into()))?;
    let got = v1.ok_or_else(|| Error::Invalid("signature missing v1=".into()))?;
    if (now_unix - ts).abs() > REPLAY_WINDOW_SECS {
        return Err(Error::Invalid(
            "signature timestamp outside replay window".into(),
        ));
    }
    let signed = format!("{ts}.{body}");
    // constant-time compare via hmac (avoid early-exit timing leak).
    let mut want_mac = HmacSha256::new_from_slice(secret).map_err(|e| Error::Internal(e.into()))?;
    want_mac.update(signed.as_bytes());
    let got_bytes =
        hex::decode(got).map_err(|e| Error::Invalid(format!("bad signature hex: {e}")))?;
    want_mac
        .verify_slice(&got_bytes)
        .map_err(|_| Error::Forbidden("signature mismatch".into()))?;
    Ok(ts)
}

// ===== outbound delivery (driven by the outbox drain) =====

/// Deliver one `webhook.deliver` outbox payload: resolve the subscription +
/// secret, build + sign the envelope, POST it, and record the attempt. Returns
/// `Ok(())` on a 2xx ack, `Err` otherwise (the outbox retries).
pub async fn deliver(
    pool: &PgPool,
    secrets: &dyn SecretStore,
    http: &reqwest::Client,
    payload: &Value,
) -> Result<()> {
    let webhook_id: Uuid = payload
        .get("webhook_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| Error::Invalid("webhook.deliver missing webhook_id".into()))?;
    let tenant: Uuid = payload
        .get("tenant_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| Error::Invalid("webhook.deliver missing tenant_id".into()))?;

    // subscription (int.webhook is RLS-gated → tenant GUC).
    let (url, secret_ref, active) = {
        let mut tx = pool.begin().await.map_err(Error::internal)?;
        mda_security::set_tenant(&mut tx, tenant).await?;
        let row: Option<(String, String, bool)> = sqlx::query_as(
            "SELECT url, secret_ref, active FROM int.webhook WHERE id = $1 AND tenant_id = $2",
        )
        .bind(webhook_id)
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Error::internal)?;
        tx.commit().await.map_err(Error::internal)?;
        row.ok_or_else(|| Error::NotFound(format!("webhook {webhook_id}")))?
    };
    if !active {
        return Ok(()); // inactive subscription: nothing to do (counts as done).
    }

    // resolve the signing secret server-side (§5.20), audited.
    let secret =
        crate::secrets::resolve_and_audit(pool, secrets, tenant, &secret_ref, None, "webhook.sign")
            .await?;

    let envelope = Envelope {
        event_id: payload
            .get("event_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0")
            .to_string(),
        tenant_id: tenant,
        schema_version: SCHEMA_VERSION,
        r#type: payload
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("event")
            .to_string(),
        entity: payload
            .get("entity")
            .and_then(|v| v.as_str())
            .map(String::from),
        record_id: payload
            .get("record_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok()),
        occurred_at: payload
            .get("occurred_at")
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
        actor: payload
            .get("actor")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok()),
        data: payload.get("data").cloned().unwrap_or(Value::Null),
    };
    let body = serde_json::to_string(&envelope).map_err(Error::internal)?;
    let ts = chrono::Utc::now().timestamp();
    let sig = sign(&secret, ts, &body);

    // SSRF guard: subscriptions are admin-authored, but re-validate at delivery
    // time — the URL lives in the DB and may predate validation or point at a
    // host that has since been repointed internally.
    let target = mda_integration::net::parse_outbound_url(&url)?;
    mda_integration::net::assert_public_egress(&target).await?;

    let resp = http
        .post(target.clone())
        .header("X-MDA-Signature", sig)
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await;
    let (code, ok, err) = match resp {
        Ok(r) => {
            let status = r.status().as_u16() as i32;
            let ok = r.status().is_success();
            (
                Some(status),
                ok,
                if ok {
                    None
                } else {
                    Some(format!("HTTP {status}"))
                },
            )
        }
        Err(e) => (None, false, Some(e.to_string())),
    };

    // record the delivery attempt (idempotent on (webhook, event_id)).
    sqlx::query(
        "INSERT INTO sys_webhook_delivery
            (tenant_id, webhook_id, event_id, event_type, entity, record_id, url,
             status, response_code, attempts, last_error, delivered_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 1, $10,
                 CASE WHEN $8 = 'delivered' THEN now() ELSE NULL END)
         ON CONFLICT (webhook_id, event_id) DO UPDATE
           SET attempts = sys_webhook_delivery.attempts + 1,
               response_code = EXCLUDED.response_code,
               status = EXCLUDED.status,
               last_error = EXCLUDED.last_error,
               delivered_at = CASE WHEN EXCLUDED.status = 'delivered'
                                   THEN now() ELSE sys_webhook_delivery.delivered_at END",
    )
    .bind(tenant)
    .bind(webhook_id)
    .bind(&envelope.event_id)
    .bind(&envelope.r#type)
    .bind(&envelope.entity)
    .bind(envelope.record_id)
    .bind(&url)
    .bind(if ok { "delivered" } else { "failed" })
    .bind(code)
    .bind(err)
    .execute(pool)
    .await
    .map_err(Error::internal)?;

    if ok {
        Ok(())
    } else {
        Err(Error::internal(anyhow::anyhow!(
            "webhook delivery to {url} failed"
        )))
    }
}

// ===== relay: sys_event_log → webhook.deliver outbox rows =====

/// One relay pass: for each new sys_event_log row, enqueue a `webhook.deliver`
/// outbox row for every matching active webhook subscription. Idempotent via a
/// high-water mark stored out-of-band (`sys_event_log.seq`). Returns the number
/// of deliveries enqueued.
pub async fn relay_once(pool: &PgPool) -> Result<u64> {
    // The relay cursor table is migration-owned (20260133000001): runtime DDL
    // here would need CREATE rights the app role must not have.
    let last: i64 = sqlx::query_scalar("SELECT seq FROM sys_webhook_relay_cursor WHERE id = 0")
        .fetch_optional(pool)
        .await
        .map_err(Error::internal)?
        .unwrap_or(0);

    // New events since the cursor.
    #[allow(clippy::type_complexity)]
    type RelayEvent = (
        i64,
        Uuid,
        String,
        Option<String>,
        Option<Uuid>,
        Option<Uuid>,
        Value,
    );
    let events: Vec<RelayEvent> = sqlx::query_as(
        "SELECT seq, tenant_id, type, entity, record_id, actor_id, payload
           FROM sys_event_log WHERE seq > $1 ORDER BY seq LIMIT 500",
    )
    .bind(last)
    .fetch_all(pool)
    .await
    .map_err(Error::internal)?;

    if events.is_empty() {
        return Ok(0);
    }
    let mut new_cursor = last;
    let mut enqueued = 0u64;
    for (seq, tenant, etype, entity, record_id, actor, payload) in events {
        new_cursor = new_cursor.max(seq);
        // matching active subscriptions for this tenant (RLS-gated → GUC).
        let subs: Vec<(Uuid, String)> = {
            let mut tx = pool.begin().await.map_err(Error::internal)?;
            mda_security::set_tenant(&mut tx, tenant).await?;
            let rows: Vec<(Uuid, String)> = sqlx::query_as(
                "SELECT id, url FROM int.webhook
                  WHERE tenant_id = $1 AND active = TRUE
                    AND (event_types = '{}' OR $2 = ANY(event_types) OR '*' = ANY(event_types))
                    AND ($3::text IS NULL OR entity_filter IS NULL OR entity_filter = $3)",
            )
            .bind(tenant)
            .bind(&etype)
            .bind(&entity)
            .fetch_all(&mut *tx)
            .await
            .map_err(Error::internal)?;
            tx.commit().await.map_err(Error::internal)?;
            rows
        };
        for (webhook_id, _url) in subs {
            let dpayload = json!({
                "tenant_id": tenant,
                "webhook_id": webhook_id,
                "event_id": seq.to_string(),
                "event_type": etype,
                "entity": entity,
                "record_id": record_id,
                "actor": actor,
                "occurred_at": chrono::Utc::now().to_rfc3339(),
                "data": payload,
            });
            sqlx::query(
                "INSERT INTO sys_outbox (tenant_id, kind, payload) VALUES ($1, 'webhook.deliver', $2)",
            )
            .bind(tenant)
            .bind(&dpayload)
            .execute(pool)
            .await
            .map_err(Error::internal)?;
            enqueued += 1;
        }
    }
    sqlx::query(
        "INSERT INTO sys_webhook_relay_cursor (id, seq) VALUES (0, $1)
         ON CONFLICT (id) DO UPDATE SET seq = EXCLUDED.seq",
    )
    .bind(new_cursor)
    .execute(pool)
    .await
    .map_err(Error::internal)?;
    Ok(enqueued)
}

/// Spawn the relay sweep (event_log → webhook.deliver outbox rows).
pub fn spawn_relay(pool: PgPool) {
    tokio::spawn(async move {
        tracing::info!("webhook relay started");
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if let Err(e) = relay_once(&pool).await {
                tracing::warn!(?e, "webhook relay pass failed");
            }
        }
    });
}

// ===== webhook subscription API =====

#[derive(Debug, Deserialize)]
struct CreateBody {
    name: String,
    url: String,
    #[serde(default)]
    event_types: Vec<String>,
    entity_filter: Option<String>,
    secret_ref: String,
    #[serde(default = "yes")]
    active: bool,
}
fn yes() -> bool {
    true
}

#[derive(Debug, Serialize)]
struct WebhookOut {
    id: Uuid,
    name: String,
    url: String,
    event_types: Vec<String>,
    entity_filter: Option<String>,
    secret_ref: String,
    active: bool,
}

#[derive(sqlx::FromRow)]
struct WebhookRow {
    id: Uuid,
    name: String,
    url: String,
    event_types: Vec<String>,
    entity_filter: Option<String>,
    secret_ref: String,
    active: bool,
}

impl From<WebhookRow> for WebhookOut {
    fn from(r: WebhookRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            url: r.url,
            event_types: r.event_types,
            entity_filter: r.entity_filter,
            secret_ref: r.secret_ref,
            active: r.active,
        }
    }
}

async fn create_webhook(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateBody>,
) -> ApiResult<(StatusCode, Json<WebhookOut>)> {
    crate::admin::require_admin(&user)?;
    if body.name.trim().is_empty()
        || body.url.trim().is_empty()
        || body.secret_ref.trim().is_empty()
    {
        return Err(Error::Invalid("name, url, and secret_ref are required".into()).into());
    }
    // SSRF guard: scheme + host must be outbound-public at registration time
    // (re-checked at every delivery).
    let target = mda_integration::net::parse_outbound_url(&body.url)?;
    mda_integration::net::assert_public_egress(&target).await?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let row: Option<WebhookRow> = sqlx::query_as(
        "INSERT INTO int.webhook (tenant_id, name, url, event_types, entity_filter, secret_ref, active)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (tenant_id, name) DO NOTHING
         RETURNING id, name, url, event_types, entity_filter, secret_ref, active",
    )
    .bind(user.tenant_id)
    .bind(&body.name)
    .bind(&body.url)
    .bind(&body.event_types)
    .bind(&body.entity_filter)
    .bind(&body.secret_ref)
    .bind(body.active)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    let row = row.ok_or_else(|| Error::Conflict(format!("webhook {} exists", body.name)))?;
    Ok((StatusCode::CREATED, Json(row.into())))
}

async fn list_webhooks(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<WebhookOut>>> {
    crate::admin::require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let rows: Vec<WebhookRow> = sqlx::query_as(
        "SELECT id, name, url, event_types, entity_filter, secret_ref, active
           FROM int.webhook WHERE tenant_id = $1 ORDER BY name",
    )
    .bind(user.tenant_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

async fn get_webhook(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<WebhookOut>> {
    crate::admin::require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let row: Option<WebhookRow> = sqlx::query_as(
        "SELECT id, name, url, event_types, entity_filter, secret_ref, active
           FROM int.webhook WHERE tenant_id = $1 AND id = $2",
    )
    .bind(user.tenant_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    let row = row.ok_or_else(|| Error::NotFound(format!("webhook {id}")))?;
    Ok(Json(row.into()))
}

async fn delete_webhook(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    crate::admin::require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let n = sqlx::query("DELETE FROM int.webhook WHERE tenant_id = $1 AND id = $2")
        .bind(user.tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?
        .rows_affected();
    tx.commit().await.map_err(Error::internal)?;
    if n == 0 {
        return Err(Error::NotFound(format!("webhook {id}")).into());
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn list_deliveries(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<Value>>> {
    crate::admin::require_admin(&user)?;
    let rows: Vec<(Value,)> = sqlx::query_as(
        "SELECT to_jsonb(d.*) AS doc FROM sys_webhook_delivery d
          WHERE tenant_id = $1 AND webhook_id = $2 ORDER BY created_at DESC LIMIT 50",
    )
    .bind(user.tenant_id)
    .bind(id)
    .fetch_all(&st.pool)
    .await
    .map_err(Error::internal)?;
    Ok(Json(rows.into_iter().map(|(v,)| v).collect()))
}

#[derive(Debug, Deserialize)]
struct ReplayQuery {
    /// Replay from this event_id (exclusive) onward, within the retention window.
    from: Option<String>,
    #[serde(default = "default_limit")]
    limit: i64,
}
fn default_limit() -> i64 {
    100
}

/// `POST /api/webhooks/:id/replay[?from=<event_id>&limit=]` — re-enqueue
/// deliveries for events the subscriber may have missed (mirrors SSE
/// `Last-Event-ID`, §5.10.5 / §5.21).
async fn replay(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Query(q): Query<ReplayQuery>,
) -> ApiResult<Json<Value>> {
    crate::admin::require_admin(&user)?;
    // Resolve the webhook (must exist + be active) with its subscription filters
    // — a replay delivers what this subscription would have matched, nothing
    // more (same predicate as the relay).
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let sub: Option<(Vec<String>, Option<String>)> = sqlx::query_as(
        "SELECT event_types, entity_filter FROM int.webhook
          WHERE tenant_id = $1 AND id = $2 AND active = TRUE",
    )
    .bind(user.tenant_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    let (event_types, entity_filter) =
        sub.ok_or_else(|| Error::NotFound(format!("webhook {id}")))?;

    let from_seq: i64 = match q.from.as_deref() {
        Some(s) => s.parse().unwrap_or(0),
        None => 0,
    };
    // Clamp: a negative limit would mean "no limit" in Postgres — one request
    // must not be able to re-enqueue the tenant's entire event log.
    let limit = q.limit.clamp(1, 1000);
    #[allow(clippy::type_complexity)]
    type ReplayEvent = (
        i64,
        String,
        Option<String>,
        Option<Uuid>,
        Option<Uuid>,
        Value,
    );
    let events: Vec<ReplayEvent> = sqlx::query_as(
        "SELECT seq, type, entity, record_id, actor_id, payload
               FROM sys_event_log
              WHERE tenant_id = $1 AND seq > $2
                AND ($4::text[] = '{}' OR type = ANY($4) OR '*' = ANY($4))
                AND ($5::text IS NULL OR entity IS NULL OR entity = $5)
              ORDER BY seq LIMIT $3",
    )
    .bind(user.tenant_id)
    .bind(from_seq)
    .bind(limit)
    .bind(&event_types)
    .bind(entity_filter)
    .fetch_all(&st.pool)
    .await
    .map_err(Error::internal)?;

    let mut enqueued = 0u64;
    for (seq, etype, entity, record_id, actor, payload) in events {
        let dpayload = json!({
            "tenant_id": user.tenant_id,
            "webhook_id": id,
            "event_id": seq.to_string(),
            "event_type": etype,
            "entity": entity,
            "record_id": record_id,
            "actor": actor,
            "occurred_at": chrono::Utc::now().to_rfc3339(),
            "data": payload,
        });
        sqlx::query(
            "INSERT INTO sys_outbox (tenant_id, kind, payload) VALUES ($1, 'webhook.deliver', $2)",
        )
        .bind(user.tenant_id)
        .bind(&dpayload)
        .execute(&st.pool)
        .await
        .map_err(Error::internal)?;
        enqueued += 1;
    }
    Ok(Json(json!({ "enqueued": enqueued })))
}

// ===== inbound receiver (§14) =====

/// `POST /api/integrations/webhooks/:id` — receive a signed event from an
/// external system. Verifies the `X-MDA-Signature` (shared secret via the
/// SecretStore) + replay window, dedupes on `(webhook_id, X-MDA-Event-Id)`, and
/// records the payload for an integration flow (§5.22) to consume.
async fn inbound(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<StatusCode> {
    // Resolve the webhook + its tenant + secret via the SECURITY DEFINER lookup
    // (the receiver is edge-unauthenticated; int.webhook is RLS-gated, so a
    // tenant-less SELECT would see nothing under RLS). FORCE RLS was dropped on
    // int.webhook (migration 20260137000001): under FORCE even the non-superuser
    // function owner is subject to the policies and would see zero rows — every
    // managed-Postgres deployment 404'd here. Non-owner roles stay isolated.
    let row: Option<(Uuid, String)> =
        sqlx::query_as("SELECT tenant_id, secret_ref FROM mda.lookup_webhook($1)")
            .bind(id)
            .fetch_optional(&st.pool)
            .await
            .map_err(Error::internal)?;
    let (tenant, secret_ref) = row.ok_or_else(|| Error::NotFound(format!("webhook {id}")))?;

    let sig = headers
        .get("x-mda-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Error::Forbidden("missing X-MDA-Signature".into()))?;
    let body_str = std::str::from_utf8(&body)
        .map_err(|_| Error::Invalid("request body must be valid UTF-8".into()))?;
    let now = chrono::Utc::now().timestamp();
    let secret = crate::secrets::resolve_and_audit(
        &st.pool,
        st.secrets.as_ref(),
        tenant,
        &secret_ref,
        None,
        "webhook.verify",
    )
    .await?;
    let _ts = verify(&secret, sig, body_str, now)?;

    // Dedupe key: the client's X-MDA-Event-Id when supplied; otherwise a hash
    // of (webhook, signature timestamp, body) — a replayed signed request that
    // simply drops the header still dedupes within the replay window.
    let event_id: String = match headers.get("x-mda-event-id").and_then(|v| v.to_str().ok()) {
        Some(eid) => eid.to_string(),
        None => {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            h.update(id.as_bytes());
            h.update(_ts.to_le_bytes());
            h.update(&body);
            hex::encode(h.finalize())
        }
    };
    let payload: Value =
        serde_json::from_slice(&body).map_err(|e| Error::Invalid(format!("bad json: {e}")))?;
    let event_type = payload
        .get("type")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // dedupe on (webhook_id, event_id).
    let inserted = sqlx::query(
        "INSERT INTO sys_inbound_webhook (tenant_id, webhook_id, event_id, event_type, payload)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (webhook_id, event_id) DO NOTHING",
    )
    .bind(tenant)
    .bind(id)
    .bind(&event_id)
    .bind(&event_type)
    .bind(&payload)
    .execute(&st.pool)
    .await
    .map_err(Error::internal)?;
    if inserted.rows_affected() == 0 {
        return Ok(StatusCode::OK); // duplicate — already received (idempotent ack).
    }

    // enqueue for the integration flow runner (§5.22) to consume.
    sqlx::query(
        "INSERT INTO sys_outbox (tenant_id, kind, payload) VALUES ($1, 'integration.inbound', $2)",
    )
    .bind(tenant)
    .bind(json!({"webhook_id": id, "event_type": event_type, "payload": payload}))
    .execute(&st.pool)
    .await
    .map_err(Error::internal)?;
    Ok(StatusCode::ACCEPTED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_roundtrip() {
        let secret = b"super-secret-key";
        let body = r#"{"event_id":"42","type":"record.created"}"#;
        let ts = chrono::Utc::now().timestamp();
        let sig = sign(secret, ts, body);
        let got = verify(secret, &sig, body, ts).unwrap();
        assert_eq!(got, ts);
    }

    #[test]
    fn verify_rejects_tampered_body() {
        let secret = b"k";
        let ts = chrono::Utc::now().timestamp();
        let sig = sign(secret, ts, "body-a");
        assert!(verify(secret, &sig, "body-b", ts).is_err());
    }

    #[test]
    fn verify_rejects_replay_outside_window() {
        let secret = b"k";
        let ts = chrono::Utc::now().timestamp();
        let sig = sign(secret, ts, "body");
        // far future
        assert!(verify(secret, &sig, "body", ts + REPLAY_WINDOW_SECS + 100).is_err());
        // far past
        assert!(verify(secret, &sig, "body", ts - REPLAY_WINDOW_SECS - 100).is_err());
    }

    #[test]
    fn verify_rejects_wrong_secret() {
        let ts = chrono::Utc::now().timestamp();
        let sig = sign(b"secret-a", ts, "body");
        assert!(verify(b"secret-b", &sig, "body", ts).is_err());
    }
}
