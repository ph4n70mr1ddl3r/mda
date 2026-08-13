//! Notifications & messaging (PLAN §5.18): a first-class platform subsystem
//! built on `sys_notification` + the real-time channel + the transactional
//! outbox. The engine knows no notification *content* — types + templates are
//! metadata; this is the generic delivery machinery.
//!
//! - **Types are metadata** ([`NotificationType`]): an opaque key, default
//!   channels, a template link (§5.19), and a digestible flag.
//! - **Per-user preferences** ([`Preference`]): mute a type / opt out of a
//!   channel; honored at **fan-out** time (a muted type is never produced).
//! - **Multi-channel delivery** via pluggable [`Channel`]s: in-app (writes
//!   `sys_notification` + emits a `notification.created` event the SSE relay
//!   fans out) and email (renders the template → records a `sys_message`). Every
//!   channel except in-app is an async side-effect routed through the outbox.
//! - **Digest/batching**: a digestible type's unread notifications are rolled
//!   into one summary by the background [`digest_once`] sweep.
//!
//! Fan-out is driven by the outbox drain: a `notification.fanout` row carries
//! `{type_key, recipients, entity, record_id, context}` and the drain calls
//! [`fanout`]. [`dispatch`] enqueues such a row transactionally (call it from a
//! rule/workflow write path).

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use mda_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

// ===== public API routes =====

pub fn routes() -> Router<AppState> {
    Router::new()
        // inbox (existing)
        .route("/api/notifications", get(list_notifications))
        .route("/api/notifications/:id", axum::routing::patch(mark_read))
        // types (metadata authoring)
        .route(
            "/api/notification-types",
            post(create_type).get(list_types),
        )
        .route("/api/notification-types/:key", get(get_type))
        // per-user preferences
        .route(
            "/api/notification-preferences",
            get(list_prefs).put(set_prefs),
        )
        // delivered messages log
        .route("/api/messages", get(list_messages))
        // system-triggered dispatch (also the testable entry point for fan-out)
        .route("/api/notifications/dispatch", post(dispatch_endpoint))
}

// ===== inbox =====

async fn list_notifications(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<Value>>> {
    let rows: Vec<(Value,)> = sqlx::query_as(
        "SELECT to_jsonb(n.*) AS doc FROM sys_notification n
          WHERE tenant_id = $1 AND user_id = $2 AND digested_at IS NULL
          ORDER BY read_at NULLS FIRST, created_at DESC LIMIT 50",
    )
    .bind(user.tenant_id)
    .bind(user.user_id)
    .fetch_all(&st.pool)
    .await
    .map_err(Error::internal)?;
    Ok(Json(rows.into_iter().map(|(v,)| v).collect()))
}

#[derive(Deserialize)]
struct MarkRead {
    #[serde(default)]
    read: Option<bool>,
}

async fn mark_read(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<MarkRead>,
) -> ApiResult<StatusCode> {
    if body.read.unwrap_or(true) {
        sqlx::query(
            "UPDATE sys_notification SET read_at = now()
              WHERE id = $1 AND tenant_id = $2 AND user_id = $3 AND read_at IS NULL",
        )
        .bind(id)
        .bind(user.tenant_id)
        .bind(user.user_id)
        .execute(&st.pool)
        .await
        .map_err(Error::internal)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ===== notification types (metadata) =====

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct NotificationType {
    pub key: String,
    pub label: String,
    pub default_channels: Vec<String>,
    pub template_name: Option<String>,
    pub digestible: bool,
}

/// Load a notification type by key under the tenant (RLS-gated → tenant GUC).
pub async fn load_type(pool: &PgPool, tenant: Uuid, key: &str) -> Result<Option<NotificationType>> {
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    let row: Option<(String, Vec<String>, Option<String>, bool)> = sqlx::query_as(
        "SELECT label, default_channels, template_name, digestible
           FROM meta.md_notification_type WHERE tenant_id = $1 AND key = $2",
    )
    .bind(tenant)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(row.map(|(label, default_channels, template_name, digestible)| NotificationType {
        key: key.to_string(),
        label,
        default_channels,
        template_name,
        digestible,
    }))
}

#[derive(Debug, Deserialize)]
struct CreateTypeBody {
    key: String,
    label: String,
    #[serde(default = "default_in_app")]
    default_channels: Vec<String>,
    template_name: Option<String>,
    #[serde(default)]
    digestible: bool,
}

fn default_in_app() -> Vec<String> {
    vec!["in_app".to_string()]
}

#[derive(Debug, Serialize)]
struct TypeOut {
    key: String,
    label: String,
    default_channels: Vec<String>,
    template_name: Option<String>,
    digestible: bool,
}

async fn create_type(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateTypeBody>,
) -> ApiResult<(StatusCode, Json<TypeOut>)> {
    if body.key.trim().is_empty() {
        return Err(Error::Invalid("key is required".into()).into());
    }
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let row: Option<(String, Vec<String>, Option<String>, bool)> = sqlx::query_as(
        "INSERT INTO meta.md_notification_type
            (tenant_id, key, label, default_channels, template_name, digestible)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (tenant_id, key) DO NOTHING
         RETURNING label, default_channels, template_name, digestible",
    )
    .bind(user.tenant_id)
    .bind(&body.key)
    .bind(&body.label)
    .bind(&body.default_channels)
    .bind(&body.template_name)
    .bind(body.digestible)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    let (label, default_channels, template_name, digestible) =
        row.ok_or_else(|| Error::Conflict(format!("type {} exists", body.key)))?;
    Ok((
        StatusCode::CREATED,
        Json(TypeOut {
            key: body.key,
            label,
            default_channels,
            template_name,
            digestible,
        }),
    ))
}

async fn list_types(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<TypeOut>>> {
    #[allow(clippy::type_complexity)]
    type TypeRow = (String, String, Vec<String>, Option<String>, bool);
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let rows: Vec<TypeRow> = sqlx::query_as(
        "SELECT key, label, default_channels, template_name, digestible
           FROM meta.md_notification_type WHERE tenant_id = $1 ORDER BY key",
    )
    .bind(user.tenant_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(
        rows.into_iter()
            .map(|r| TypeOut {
                key: r.0,
                label: r.1,
                default_channels: r.2,
                template_name: r.3,
                digestible: r.4,
            })
            .collect(),
    ))
}

async fn get_type(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(key): Path<String>,
) -> ApiResult<Json<Value>> {
    let t = load_type(&st.pool, user.tenant_id, &key)
        .await?
        .ok_or_else(|| Error::NotFound(format!("type {key}")))?;
    Ok(Json(json!({
        "key": t.key,
        "label": t.label,
        "default_channels": t.default_channels,
        "template_name": t.template_name,
        "digestible": t.digestible,
    })))
}

// ===== preferences =====

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct Preference {
    pub type_key: String,
    pub channel: String,
    pub opted_in: bool,
}

/// Load a user's preferences for a type (app-layer tenant filter; no GUC needed).
async fn load_prefs(
    pool: &PgPool,
    tenant: Uuid,
    user: Uuid,
    type_key: &str,
) -> Result<Vec<Preference>> {
    let prefs: Vec<Preference> = sqlx::query_as(
        "SELECT type_key, channel, opted_in FROM sys_notification_preference
          WHERE tenant_id = $1 AND user_id = $2 AND type_key = $3",
    )
    .bind(tenant)
    .bind(user)
    .bind(type_key)
    .fetch_all(pool)
    .await
    .map_err(Error::internal)?;
    Ok(prefs)
}

/// Effective channels for a recipient = type defaults minus opted-out channels.
/// A channel is delivered iff there is no preference OR the preference says
/// opted_in. A muted type (all channels opted-out) → empty → nothing produced.
pub fn effective_channels(defaults: &[String], prefs: &[Preference]) -> Vec<String> {
    defaults
        .iter()
        .filter(|ch| {
            prefs
                .iter()
                .find(|p| &p.channel == *ch)
                .map(|p| p.opted_in)
                .unwrap_or(true)
        })
        .cloned()
        .collect()
}

async fn list_prefs(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<Preference>>> {
    let prefs: Vec<Preference> = sqlx::query_as(
        "SELECT type_key, channel, opted_in FROM sys_notification_preference
          WHERE tenant_id = $1 AND user_id = $2 ORDER BY type_key, channel",
    )
    .bind(user.tenant_id)
    .bind(user.user_id)
    .fetch_all(&st.pool)
    .await
    .map_err(Error::internal)?;
    Ok(Json(prefs))
}

#[derive(Debug, Deserialize)]
struct SetPrefsBody {
    preferences: Vec<Preference>,
}

async fn set_prefs(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<SetPrefsBody>,
) -> ApiResult<StatusCode> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    for p in &body.preferences {
        sqlx::query(
            "INSERT INTO sys_notification_preference
                (tenant_id, user_id, type_key, channel, opted_in, updated_at)
             VALUES ($1, $2, $3, $4, $5, now())
             ON CONFLICT (tenant_id, user_id, type_key, channel)
             DO UPDATE SET opted_in = EXCLUDED.opted_in, updated_at = now()",
        )
        .bind(user.tenant_id)
        .bind(user.user_id)
        .bind(&p.type_key)
        .bind(&p.channel)
        .bind(p.opted_in)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
    }
    tx.commit().await.map_err(Error::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_messages(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<Value>>> {
    let rows: Vec<(Value,)> = sqlx::query_as(
        "SELECT to_jsonb(m.*) AS doc FROM sys_message m
          WHERE tenant_id = $1 AND user_id = $2 ORDER BY created_at DESC LIMIT 50",
    )
    .bind(user.tenant_id)
    .bind(user.user_id)
    .fetch_all(&st.pool)
    .await
    .map_err(Error::internal)?;
    Ok(Json(rows.into_iter().map(|(v,)| v).collect()))
}

// ===== channels (pluggable delivery) =====

/// Context handed to a channel for one delivery to one recipient.
pub struct Delivery {
    pub tenant: Uuid,
    pub recipient: Uuid,
    pub type_key: String,
    pub label: String,
    pub entity: Option<String>,
    pub record_id: Option<Uuid>,
    pub context: Value,
    pub template_name: Option<String>,
}

/// A delivery channel. Implementations are pluggable (§5.18: "a `Channel` trait,
/// analogous to `Connector`"). In-app + email ship now; webhook (§5.21) is added
/// alongside the integration layer.
#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &'static str;
    async fn deliver(&self, pool: &PgPool, d: &Delivery) -> Result<()>;
}

/// In-app: write `sys_notification` + emit a `notification.created` event the
/// real-time relay fans out (§5.10).
pub struct InAppChannel;

#[async_trait]
impl Channel for InAppChannel {
    fn name(&self) -> &'static str {
        "in_app"
    }
    async fn deliver(&self, pool: &PgPool, d: &Delivery) -> Result<()> {
        sqlx::query(
            "INSERT INTO sys_notification (tenant_id, user_id, type, entity, record_id, payload)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(d.tenant)
        .bind(d.recipient)
        .bind(&d.type_key)
        .bind(&d.entity)
        .bind(d.record_id)
        .bind(&d.context)
        .execute(pool)
        .await
        .map_err(Error::internal)?;
        sqlx::query(
            "INSERT INTO sys_event_log (tenant_id, type, entity, record_id, actor_id, payload)
             VALUES ($1, 'notification.created', $2, $3, $4, $5)",
        )
        .bind(d.tenant)
        .bind(&d.entity)
        .bind(d.record_id)
        .bind(d.recipient)
        .bind(json!({ "type_key": d.type_key, "user_id": d.recipient }))
        .execute(pool)
        .await
        .map_err(Error::internal)?;
        Ok(())
    }
}

/// Email: render the linked template (§5.19) under the render context and record
/// a `sys_message` (delivered-message log). The recipient address is resolved
/// from `sec_user.email`. SMTP transport is a follow-up; the message is recorded
/// here for audit + delivery retries.
pub struct EmailChannel;

#[async_trait]
impl Channel for EmailChannel {
    fn name(&self) -> &'static str {
        "email"
    }
    async fn deliver(&self, pool: &PgPool, d: &Delivery) -> Result<()> {
        // resolve recipient email under the tenant GUC (sec_user is RLS-gated).
        let (to_addr, body, content_type) = {
            let mut tx = pool.begin().await.map_err(Error::internal)?;
            mda_security::set_tenant(&mut tx, d.tenant).await?;
            let to: Option<String> = sqlx::query_scalar(
                "SELECT email FROM sec.sec_user WHERE id = $1 AND active = TRUE",
            )
            .bind(d.recipient)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Error::internal)?;

            let (body, content_type) = match &d.template_name {
                Some(name) => {
                    let row: Option<(String, String)> = sqlx::query_as(
                        "SELECT body, content_type FROM meta.md_template
                          WHERE tenant_id = $1 AND name = $2 ORDER BY locale NULLS FIRST LIMIT 1",
                    )
                    .bind(d.tenant)
                    .bind(name)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(Error::internal)?;
                    match row {
                        Some((b, ct)) => {
                            let tpl = mda_reports::Template {
                                name: name.clone(),
                                kind: "email".into(),
                                body: b,
                                content_type: ct.clone(),
                                locale: None,
                            };
                            let reg = mda_expression::Registry::new();
                            let rendered = mda_reports::render(&tpl, &d.context, &reg)
                                .map_err(Error::internal)?;
                            (rendered.body, rendered.content_type)
                        }
                        None => (d.context.to_string(), "application/json".to_string()),
                    }
                }
                None => (d.context.to_string(), "application/json".to_string()),
            };
            tx.commit().await.map_err(Error::internal)?;
            (to, body, content_type)
        };

        sqlx::query(
            "INSERT INTO sys_message (tenant_id, user_id, to_addr, type_key, subject, body, content_type, record_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(d.tenant)
        .bind(d.recipient)
        .bind(&to_addr)
        .bind(&d.type_key)
        .bind(&d.label)
        .bind(&body)
        .bind(&content_type)
        .bind(d.record_id)
        .execute(pool)
        .await
        .map_err(Error::internal)?;
        Ok(())
    }
}

/// The default channel set (in-app + email). The webhook channel is added by the
/// integration/webhook layer (§5.21).
pub fn default_channels() -> Vec<Box<dyn Channel>> {
    vec![Box::new(InAppChannel), Box::new(EmailChannel)]
}

// ===== fan-out (driven by the outbox drain) =====

/// Run fan-out for a `notification.fanout` outbox payload. Resolves the type,
/// honors per-user preferences, and delivers to each effective channel.
/// Best-effort per channel: a failing channel is logged but does not abort the
/// whole fan-out (the outbox row still succeeds; partial delivery is acceptable
/// for at-least-once — the failing channel will retry on the next outbox row
/// that targets it).
pub async fn fanout(pool: &PgPool, channels: &[Box<dyn Channel>], payload: &Value) -> Result<()> {
    let tenant: Uuid = payload
        .get("tenant_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| Error::Invalid("notification.fanout missing tenant_id".into()))?;
    let type_key = payload
        .get("type_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Invalid("notification.fanout missing type_key".into()))?;
    let recipients = payload
        .get("recipients")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let entity = payload.get("entity").and_then(|v| v.as_str()).map(String::from);
    let record_id = payload
        .get("record_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());
    let context = payload.get("context").cloned().unwrap_or(Value::Null);

    // Resolve the type (defaults to in-app only if the type isn't registered).
    let (label, default_channels, template_name) = match load_type(pool, tenant, type_key).await? {
        Some(t) => (t.label, t.default_channels, t.template_name),
        None => (type_key.to_string(), vec!["in_app".to_string()], None),
    };

    for r in recipients {
        let recipient = r.as_str().and_then(|s| Uuid::parse_str(s).ok());
        let Some(recipient) = recipient else {
            continue;
        };
        let prefs = load_prefs(pool, tenant, recipient, type_key).await?;
        let eff = effective_channels(&default_channels, &prefs);
        if eff.is_empty() {
            // muted type — never produced (§5.18).
            tracing::debug!(%type_key, %recipient, "notification muted; not produced");
            continue;
        }
        let delivery = Delivery {
            tenant,
            recipient,
            type_key: type_key.to_string(),
            label: label.clone(),
            entity: entity.clone(),
            record_id,
            context: context.clone(),
            template_name: template_name.clone(),
        };
        for ch_name in eff {
            if let Some(ch) = channels.iter().find(|c| c.name() == ch_name) {
                if let Err(e) = ch.deliver(pool, &delivery).await {
                    tracing::warn!(%type_key, %recipient, channel = ch.name(), ?e, "channel delivery failed");
                }
            }
        }
    }
    Ok(())
}

// ===== dispatch (enqueue from the write path) =====

/// Enqueue a `notification.fanout` outbox row transactionally. Call this from a
/// rule/workflow write path so the fan-out is durable and at-least-once.
pub async fn dispatch(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    type_key: &str,
    recipients: &[Uuid],
    entity: Option<&str>,
    record_id: Option<Uuid>,
    context: &Value,
) -> Result<()> {
    let recipients: Vec<String> = recipients.iter().map(|u| u.to_string()).collect();
    let payload = json!({
        "tenant_id": tenant,
        "type_key": type_key,
        "recipients": recipients,
        "entity": entity,
        "record_id": record_id,
        "context": context,
    });
    sqlx::query("INSERT INTO sys_outbox (tenant_id, kind, payload) VALUES ($1, 'notification.fanout', $2)")
        .bind(tenant)
        .bind(&payload)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
    Ok(())
}

/// `POST /api/notifications/dispatch` — system-triggered fan-out (also the
/// testable entry point). Enqueues a `notification.fanout` row; the drain
/// delivers it.
#[derive(Debug, Deserialize)]
struct DispatchBody {
    type_key: String,
    recipients: Vec<Uuid>,
    entity: Option<String>,
    record_id: Option<Uuid>,
    #[serde(default)]
    context: Value,
}

async fn dispatch_endpoint(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<DispatchBody>,
) -> ApiResult<StatusCode> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    dispatch(
        &mut tx,
        user.tenant_id,
        &body.type_key,
        &body.recipients,
        body.entity.as_deref(),
        body.record_id,
        &body.context,
    )
    .await?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(StatusCode::ACCEPTED)
}

// ===== digest (background sweep) =====

/// One digest pass: for each (tenant, user, type) whose type is digestible, roll
/// unread notifications older than the window into a single summary and mark the
/// originals digested. Prevents notification storms from a bulk event (§5.18).
pub async fn digest_once(pool: &PgPool) -> Result<u64> {
    // Find digestible groups with more than one undelivered (unread, not yet
    // digested) notification older than the window.
    let groups: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
        "SELECT n.tenant_id, n.user_id, n.type
           FROM sys_notification n
           JOIN meta.md_notification_type t
             ON t.tenant_id = n.tenant_id AND t.key = n.type
          WHERE n.read_at IS NULL AND n.digested_at IS NULL
            AND t.digestible = TRUE
            AND n.created_at < now() - ($1 || '')::interval
          GROUP BY n.tenant_id, n.user_id, n.type
         HAVING count(*) > 1",
    )
    .bind(format!("{} seconds", DIGEST_WINDOW_SECS))
    .fetch_all(pool)
    .await
    .map_err(Error::internal)?;

    let mut rolled = 0u64;
    for (tenant, user, type_key) in groups {
        // collect the ids + a summary of the batch.
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM sys_notification
              WHERE tenant_id = $1 AND user_id = $2 AND type = $3
                AND read_at IS NULL AND digested_at IS NULL
              ORDER BY created_at",
        )
        .bind(tenant)
        .bind(user)
        .bind(&type_key)
        .fetch_all(pool)
        .await
        .map_err(Error::internal)?;
        if ids.len() < 2 {
            continue;
        }
        let summary = json!({ "type": type_key, "count": ids.len(), "rolled_up": ids });
        sqlx::query(
            "INSERT INTO sys_notification (tenant_id, user_id, type, payload)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(tenant)
        .bind(user)
        .bind(format!("{type_key}.digest"))
        .bind(&summary)
        .execute(pool)
        .await
        .map_err(Error::internal)?;
        sqlx::query("UPDATE sys_notification SET digested_at = now() WHERE id = ANY($1)")
            .bind(&ids)
            .execute(pool)
            .await
            .map_err(Error::internal)?;
        rolled += ids.len() as u64;
    }
    Ok(rolled)
}

/// Digest window (seconds). A digestible type's unread notifications older than
/// this are candidates for roll-up.
const DIGEST_WINDOW_SECS: i64 = 300;

/// Spawn the background digest sweep.
pub fn spawn_digest(pool: PgPool) {
    tokio::spawn(async move {
        tracing::info!("notification digest sweep started");
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if let Err(e) = digest_once(&pool).await {
                tracing::warn!(?e, "digest sweep failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_channels_respects_opt_out() {
        let defaults = vec!["in_app".into(), "email".into()];
        // no prefs → all defaults
        assert_eq!(
            effective_channels(&defaults, &[]),
            vec!["in_app".to_string(), "email".to_string()]
        );
        // opt out of email
        let prefs = vec![Preference {
            type_key: "x".into(),
            channel: "email".into(),
            opted_in: false,
        }];
        assert_eq!(
            effective_channels(&defaults, &prefs),
            vec!["in_app".to_string()]
        );
        // mute the type (all channels out) → empty (never produced)
        let prefs = vec![
            Preference {
                type_key: "x".into(),
                channel: "in_app".into(),
                opted_in: false,
            },
            Preference {
                type_key: "x".into(),
                channel: "email".into(),
                opted_in: false,
            },
        ];
        assert!(effective_channels(&defaults, &prefs).is_empty());
        // an opted-in pref keeps the channel even if it weren't a default (no-op here)
        let prefs = vec![Preference {
            type_key: "x".into(),
            channel: "in_app".into(),
            opted_in: true,
        }];
        assert_eq!(
            effective_channels(&defaults, &prefs),
            vec!["in_app".to_string(), "email".to_string()]
        );
    }
}
