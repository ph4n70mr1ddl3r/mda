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
use std::collections::HashSet;
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
        .route("/api/notification-types", post(create_type).get(list_types))
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
    Ok(row.map(
        |(label, default_channels, template_name, digestible)| NotificationType {
            key: key.to_string(),
            label,
            default_channels,
            template_name,
            digestible,
        },
    ))
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
    /// The driving `sys_outbox` row id, when the delivery runs from the outbox
    /// drain. Channels derive their durable rows' ids from it, so an at-least-
    /// once replay of the row (worker crash before the status update) lands on
    /// the same rows and is a no-op instead of a duplicate.
    pub dedupe: Option<Uuid>,
}

/// Deterministic row id for one (outbox row, recipient, channel-tag) delivery:
/// the same replayed outbox row recomputes the same id, so `ON CONFLICT … DO
/// NOTHING` turns the replay into a no-op. `pub` for the drain's legacy
/// `workflow.transitioned` handler, which needs the same replay safety.
pub fn delivery_row_id(outbox: Uuid, recipient: Uuid, channel: &[u8]) -> Uuid {
    let mut name = [0u8; 32];
    name[..16].copy_from_slice(outbox.as_bytes());
    name[16..].copy_from_slice(recipient.as_bytes());
    let mut name = name.to_vec();
    name.extend_from_slice(channel);
    Uuid::new_v5(&Uuid::NAMESPACE_OID, &name)
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
        // Deterministic id when driven by the outbox: a replayed row lands on
        // the same notification instead of duplicating it.
        let id = d
            .dedupe
            .map(|k| delivery_row_id(k, d.recipient, b"in_app"))
            .unwrap_or_else(Uuid::new_v4);
        let res = sqlx::query(
            "INSERT INTO sys_notification (id, tenant_id, user_id, type, entity, record_id, payload)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(id)
        .bind(d.tenant)
        .bind(d.recipient)
        .bind(&d.type_key)
        .bind(&d.entity)
        .bind(d.record_id)
        .bind(&d.context)
        .execute(pool)
        .await
        .map_err(Error::internal)?;
        if res.rows_affected() == 0 {
            // already delivered by an earlier attempt at this outbox row —
            // the notification exists, so the companion event was emitted too.
            return Ok(());
        }
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

/// FLS-under-recipient (§5.18 follow-up): produce a render context for this
/// recipient where any `record` field they may not read is dropped, so an email
/// body can never leak an unreadable field. Falls back to the original context
/// when there's no record, no entity, or the identity can't be resolved.
async fn project_context_for_recipient(pool: &PgPool, d: &Delivery) -> Value {
    let (Some(entity), Some(rec)) = (d.entity.as_deref(), d.context.get("record")) else {
        return d.context.clone();
    };
    let Some(rec_obj) = rec.as_object() else {
        return d.context.clone();
    };
    let Ok(identity) = mda_security::load_identity(pool, d.recipient, d.tenant).await else {
        return d.context.clone(); // best-effort: render as-is
    };
    let mut projected = d.context.clone();
    if let Some(out) = projected.get_mut("record").and_then(|v| v.as_object_mut()) {
        let kept: serde_json::Map<String, Value> = rec_obj
            .iter()
            .filter(|(k, _)| identity.field_access(entity, k) != mda_security::Access::None)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        *out = kept;
    }
    projected
}

/// Email: render the linked template (§5.19) under the render context and record
/// a `sys_message` (delivered-message log). The recipient address is resolved
/// from `sec_user.email`. The render context is FLS-projected under the
/// recipient so a field they may not read is never emitted. The message is then
/// handed to the pluggable [`crate::mail::MailSender`] (a real SMTP relay when
/// configured, else a safe no-op — the `sys_message` row is the audit/retry
/// record either way).
pub struct EmailChannel {
    sender: std::sync::Arc<dyn crate::mail::MailSender>,
}

impl EmailChannel {
    /// Build an email channel with an explicit mail sender (tests).
    pub fn new(sender: std::sync::Arc<dyn crate::mail::MailSender>) -> Self {
        Self { sender }
    }
}

impl Default for EmailChannel {
    fn default() -> Self {
        Self {
            sender: crate::mail::sender_from_env(),
        }
    }
}

#[async_trait]
impl Channel for EmailChannel {
    fn name(&self) -> &'static str {
        "email"
    }
    async fn deliver(&self, pool: &PgPool, d: &Delivery) -> Result<()> {
        // resolve recipient email under the tenant GUC (sec_user is RLS-gated).
        let render_context = project_context_for_recipient(pool, d).await;
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
                            let rendered = mda_reports::render(&tpl, &render_context, &reg)
                                .map_err(Error::internal)?;
                            (rendered.body, rendered.content_type)
                        }
                        None => (render_context.to_string(), "application/json".to_string()),
                    }
                }
                None => (render_context.to_string(), "application/json".to_string()),
            };
            tx.commit().await.map_err(Error::internal)?;
            (to, body, content_type)
        };

        // Outbox-driven deliveries dedupe on (outbox_id, user_id) (partial
        // unique index): a replayed row finds the message already recorded and
        // neither re-inserts nor re-sends.
        let res = sqlx::query(
            "INSERT INTO sys_message
                (tenant_id, user_id, to_addr, type_key, subject, body, content_type, record_id, outbox_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (outbox_id, user_id) WHERE outbox_id IS NOT NULL DO NOTHING",
        )
        .bind(d.tenant)
        .bind(d.recipient)
        .bind(&to_addr)
        .bind(&d.type_key)
        .bind(&d.label)
        .bind(&body)
        .bind(&content_type)
        .bind(d.record_id)
        .bind(d.dedupe)
        .execute(pool)
        .await
        .map_err(Error::internal)?;
        if res.rows_affected() == 0 {
            return Ok(()); // message already recorded by an earlier attempt
        }

        // Hand the rendered message to the transport (record-then-send: the
        // durable `sys_message` row is written first; a send failure is logged
        // but does not fail the delivery, so one flaky SMTP hop can't wedge
        // the whole fan-out — the record remains the audit of what should be
        // in the recipient's mailbox).
        if let Some(to) = to_addr {
            let from =
                std::env::var("MDA_SMTP_FROM").unwrap_or_else(|_| "no-reply@mda.local".into());
            if let Err(e) = self
                .sender
                .send(&crate::mail::OutgoingEmail {
                    from,
                    to,
                    subject: d.label.clone(),
                    body,
                    content_type,
                })
                .await
            {
                tracing::warn!(?e, %d.recipient, "smtp send failed (message still recorded)");
            }
        }
        Ok(())
    }
}

/// The default channel set (in-app + email). The email channel uses the
/// env-configured mail sender (SMTP relay when configured, else a no-op). The
/// webhook channel is added by the integration/webhook layer (§5.21).
pub fn default_channels() -> Vec<Box<dyn Channel>> {
    vec![Box::new(InAppChannel), Box::new(EmailChannel::default())]
}

// ===== fan-out (driven by the outbox drain) =====

/// Run fan-out for a `notification.fanout` outbox payload. Resolves the type,
/// honors per-user preferences, and delivers to each effective channel.
/// Best-effort per channel: a failing channel is logged but does not abort the
/// whole fan-out (the outbox row still succeeds — §5.18 accepts partial
/// delivery rather than wedging the fan-out on one flaky sink).
/// `outbox` is the driving `sys_outbox` row id: every durable row a channel
/// writes derives from it, so an at-least-once replay of the row (crash before
/// the status update, or a row-level retry) is a no-op, not a duplicate.
pub async fn fanout(
    pool: &PgPool,
    channels: &[Box<dyn Channel>],
    payload: &Value,
    outbox: Uuid,
) -> Result<()> {
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
    let entity = payload
        .get("entity")
        .and_then(|v| v.as_str())
        .map(String::from);
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
            dedupe: Some(outbox),
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

// ===== recipient resolution (ADR-0013 record-share materialization) =====

/// Resolve every user who can READ a given record — the §5.18 "notify everyone
/// who can read this record" recipient set. Combines:
/// - **object-level**: an active user whose role grants `read` on the entity
///   (the wildcard `*/*` counts) — always required (the gate to read any record);
/// - **record-level**: the owner + anyone with a direct share, OR — when the
///   entity's OWD grants org-wide read — every object-level reader, OR — when
///   the OWD is `team` — the owner's teammates **and members of ancestor
///   teams** (the manager-visibility side of the ADR-0013 hierarchy).
///
/// The teammate query walks the `sec_team.parent_id` tree UPWARD from the
/// owner's team, so a record owned in a sub-team notifies members of every
/// ancestor (parent/manager) team too. Flat (no `parent_id` set) collapses to
/// same-team-only: the recursive ascent yields just the owner's team.
pub async fn resolve_record_readers(
    pool: &PgPool,
    tenant: Uuid,
    entity: &str,
    owner_id: Uuid,
    record_id: Uuid,
) -> Result<Vec<Uuid>> {
    let owd = mda_security::resolve_owd(pool, tenant, entity).await?;
    let public_read = owd.allows_read_for_all();

    let mut tx = pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    // object-level readers: any active user with a role granting read on the
    // entity (or the wildcard).
    let object_readers: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT u.id FROM sec.sec_user u
          JOIN sec.sec_role_assignment a ON a.user_id = u.id
          JOIN sec.sec_permission p ON p.role_id = a.role_id
         WHERE u.tenant_id = $1 AND u.active
           AND (p.entity = $2 OR p.entity = '*')
           AND (p.verb = 'read' OR p.verb = '*')",
    )
    .bind(tenant)
    .bind(entity)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;

    if public_read {
        tx.commit().await.map_err(Error::internal)?;
        return Ok(object_readers);
    }
    // private/team: owner + direct share principals, intersected with the
    // object-level read gate (no object-level read → can't read any record).
    // Team-OWD additionally admits members of the owner's team (ADR-0013
    // `owd_visible`, flat — sub-team hierarchy is the deeper refinement).
    let allowed: HashSet<Uuid> = object_readers.iter().copied().collect();
    let mut readers: HashSet<Uuid> = HashSet::new();
    if allowed.contains(&owner_id) {
        readers.insert(owner_id);
    }
    let shares: Vec<Uuid> = sqlx::query_scalar(
        "SELECT principal_id FROM sec.sec_record_share rs
          WHERE rs.tenant_id = $1 AND rs.entity = $2 AND rs.record_id = $3
            AND rs.access IN ('read','write')
            AND (rs.rule_id IS NULL
                 OR rs.epoch = (SELECT r.epoch FROM sec.sec_share_rule r
                                WHERE r.id = rs.rule_id AND r.active))",
    )
    .bind(tenant)
    .bind(entity)
    .bind(record_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    for p in shares {
        if allowed.contains(&p) {
            readers.insert(p);
        }
    }
    // Team-OWD: anyone in the owner's team or an ANCESTOR team (who also
    // clears the object-level read gate) can read — the manager-visibility
    // side of the ADR-0013 hierarchy. A team-less owner admits no one extra.
    // (`UNION`, not `UNION ALL`: the import path historically linked parents
    // unchecked, so `sec_team.parent_id` may hold a cycle — a deduplicating
    // recursive term terminates on one, an `UNION ALL` term spins forever;
    // same remedy as read_predicate's descendant walk.)
    if owd == mda_security::Owd::Team {
        let teammates: Vec<Uuid> = sqlx::query_scalar(
            "SELECT u.id FROM sec.sec_user u
             WHERE u.tenant_id = $2 AND u.active
               AND u.team_id IN (
                 WITH RECURSIVE ancestor_teams(tid) AS (
                      SELECT o.team_id FROM sec.sec_user o WHERE o.id = $1
                      UNION
                      SELECT parent.parent_id FROM sec.sec_team parent
                        JOIN ancestor_teams a ON parent.id = a.tid
                       WHERE parent.parent_id IS NOT NULL)
                 SELECT tid FROM ancestor_teams WHERE tid IS NOT NULL)",
        )
        .bind(owner_id)
        .bind(tenant)
        .fetch_all(&mut *tx)
        .await
        .map_err(Error::internal)?;
        for p in teammates {
            if allowed.contains(&p) {
                readers.insert(p);
            }
        }
    }
    tx.commit().await.map_err(Error::internal)?;
    Ok(readers.into_iter().collect())
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
    sqlx::query(
        "INSERT INTO sys_outbox (tenant_id, kind, payload) VALUES ($1, 'notification.fanout', $2)",
    )
    .bind(tenant)
    .bind(&payload)
    .execute(&mut **tx)
    .await
    .map_err(Error::internal)?;
    Ok(())
}

/// `POST /api/notifications/dispatch` — system-triggered fan-out (also the
/// testable entry point). Enqueues a `notification.fanout` row; the drain
/// delivers it. With `recipient_strategy: "record_readers"` the recipients are
/// resolved as "everyone who can read this record" (ADR-0013 materialization)
/// instead of being listed explicitly.
#[derive(Debug, Deserialize)]
struct DispatchBody {
    type_key: String,
    #[serde(default)]
    recipients: Vec<Uuid>,
    /// `explicit` (default — use `recipients`) | `record_readers` (resolve
    /// everyone who can read `entity`/`record_id`).
    #[serde(default)]
    recipient_strategy: Option<String>,
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
    // Resolve the effective recipient set up-front (before the dispatch tx) so
    // the resolution reads (record + sec graph) don't hold the outbox tx open.
    let recipients = match body.recipient_strategy.as_deref() {
        Some("record_readers") => {
            let entity = body
                .entity
                .as_deref()
                .ok_or_else(|| Error::Invalid("record_readers strategy needs `entity`".into()))?;
            let record_id = body.record_id.ok_or_else(|| {
                Error::Invalid("record_readers strategy needs `record_id`".into())
            })?;
            let entity_id =
                mda_meta::loader::entity_id_by_name(&st.pool, user.tenant_id, entity).await?;
            let def = mda_meta::loader::load_entity_definition(&st.pool, user.tenant_id, entity_id)
                .await?;
            // read under a superuser scope to learn the owner (the resolution
            // itself re-applies record-level visibility per recipient).
            let rec = mda_data::read(
                &st.pool,
                user.tenant_id,
                &def,
                record_id,
                &mda_data::RecordScope::superuser(user.user_id),
            )
            .await?;
            let owner = rec["owner_id"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .ok_or_else(|| Error::internal(anyhow::anyhow!("record missing owner_id")))?;
            resolve_record_readers(&st.pool, user.tenant_id, entity, owner, record_id).await?
        }
        _ => body.recipients.clone(),
    };

    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    dispatch(
        &mut tx,
        user.tenant_id,
        &body.type_key,
        &recipients,
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
///
/// The per-tenant work runs under the tenant GUC: `sys_notification` carries no
/// RLS (the sweep enumerates candidate tenants from it directly), but the
/// digestible-type join hits `meta.md_notification_type`, which is ENABLE+FORCE
/// RLS — under the non-superuser app role (every production deployment) a
/// tenant-less join sees ZERO rows and the sweep would silently never fire
/// (tests passed only because they run as the table owner; same class as the
/// `int.flow_step` loader bug fixed in HARDENING pass 3).
pub async fn digest_once(pool: &PgPool) -> Result<u64> {
    // Tenants that have any undigested, unread notification at all.
    let tenants: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT DISTINCT tenant_id FROM sys_notification
          WHERE read_at IS NULL AND digested_at IS NULL",
    )
    .fetch_all(pool)
    .await
    .map_err(Error::internal)?;

    let mut rolled = 0u64;
    for (tenant,) in tenants {
        rolled += digest_tenant(pool, tenant).await?;
    }
    Ok(rolled)
}

/// Digest one tenant's roll-up-able groups. Everything (grouping, summary
/// insert, digested_at stamp) happens in ONE transaction under the tenant GUC,
/// so a group is never half-rolled (summary without stamp or vice versa).
async fn digest_tenant(pool: &PgPool, tenant: Uuid) -> Result<u64> {
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;

    // Digestible groups with more than one undelivered (unread, not yet
    // digested) notification older than the window.
    let groups: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT n.user_id, n.type
           FROM sys_notification n
           JOIN meta.md_notification_type t
             ON t.tenant_id = n.tenant_id AND t.key = n.type
          WHERE n.tenant_id = $1
            AND n.read_at IS NULL AND n.digested_at IS NULL
            AND t.digestible = TRUE
            AND n.created_at < now() - ($2 || '')::interval
          GROUP BY n.user_id, n.type
         HAVING count(*) > 1",
    )
    .bind(tenant)
    .bind(format!("{} seconds", DIGEST_WINDOW_SECS))
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;

    let mut rolled = 0u64;
    for (user, type_key) in groups {
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
        .fetch_all(&mut *tx)
        .await
        .map_err(Error::internal)?;
        if ids.len() < 2 {
            continue;
        }
        // CLAIM the batch by stamping first, and only roll it up if we flipped
        // every row: the stamp carries `AND digested_at IS NULL`, so a
        // concurrent sweep that committed first leaves rows_affected < len and
        // the loser skips — one summary per batch even with two sweepers
        // racing (two replicas, or an ops-invoked digest_once against the
        // background loop).
        let claimed = sqlx::query(
            "UPDATE sys_notification SET digested_at = now()
              WHERE id = ANY($1) AND digested_at IS NULL",
        )
        .bind(&ids)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
        if claimed.rows_affected() != ids.len() as u64 {
            continue; // lost the race — the other sweeper owns this batch
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
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
        rolled += ids.len() as u64;
    }
    tx.commit().await.map_err(Error::internal)?;
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
