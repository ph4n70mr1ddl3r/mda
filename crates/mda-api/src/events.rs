//! Real-time channel for the runtime UI (PLAN §5.10).
//!
//! `sys_event_log` is the canonical, sequence-numbered domain-event stream,
//! written in the same transaction as every data write (§5.9.3 step 7). This
//! module fans it out to SSE clients:
//!   - a background worker `LISTEN mda_event` (fired by an INSERT trigger on
//!     `sys_event_log`) reads new rows and broadcasts them on a
//!     `tokio::sync::broadcast` channel;
//!   - `GET /api/events` authenticates the JWT, replays `seq > Last-Event-ID`
//!     for the caller's tenant, then streams live events — AuthZ-filtered per
//!     client (§5.10.6). The payload carries changed field *names* only (never
//!     values), so a notification can't leak a field the viewer can't read;
//!     full field-level payload filtering is a follow-up.
//!
//! Transport is SSE (server→client push is ~90% of traffic; mutations go via
//! the REST API). `Last-Event-ID` gives at-least-once delivery to the client
//! within the event-log retention window (§5.10.5); beyond it a client must do
//! a hard full re-sync on next page load.

use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use axum::Router;
use mda_security::Identity;
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::AppState;

/// How many events the in-process broadcast buffer holds per instance. On
/// overflow a client's receiver lags and re-syncs via a DB replay (§5.10.5).
const BROADCAST_CAPACITY: usize = 1024;
/// A client that reconnects with no `Last-Event-ID` gets at most this many
/// recent events on replay (keeps a reconnect cheap).
const REPLAY_BATCH: i64 = 500;

/// One row of `sys_event_log`, broadcast to local SSE clients.
#[derive(Debug, Clone)]
pub struct EventRow {
    pub seq: i64,
    pub tenant_id: Uuid,
    pub typ: String,
    pub entity: Option<String>,
    pub record_id: Option<Uuid>,
    pub payload: serde_json::Value,
}

/// A fresh broadcast channel for [`AppState`].
pub fn channel() -> broadcast::Sender<EventRow> {
    broadcast::channel(BROADCAST_CAPACITY).0
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/events", get(stream))
}

/// Spawn the per-instance fan-out worker: `LISTEN mda_event` (fired by the
/// `sys_event_log` INSERT trigger) → read new rows → broadcast to subscribers.
/// A 1 s backfill tick covers any missed NOTIFY (lossy across reconnects).
pub fn spawn_listen(pool: PgPool, tx: broadcast::Sender<EventRow>) {
    use sqlx::postgres::PgListener;
    tokio::spawn(async move {
        let mut listener = loop {
            match PgListener::connect_with(&pool).await {
                Ok(l) => break l,
                Err(e) => {
                    tracing::warn!(
                        ?e,
                        "event listener connect failed; backfill tick covers this"
                    );
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        };
        loop {
            if let Err(e) = listener.listen("mda_event").await {
                tracing::warn!(?e, "LISTEN mda_event failed; retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            tracing::info!("LISTEN mda_event (real-time relay)");
            // Seed `last_seen` from the current tail so we don't replay the
            // whole history on (re)start.
            let mut last_seen: i64 =
                sqlx::query_scalar("SELECT COALESCE(max(seq), 0) FROM sys_event_log")
                    .fetch_one(&pool)
                    .await
                    .unwrap_or(0);
            loop {
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    res = listener.recv() => {
                        if res.is_err() {
                            tracing::warn!("event listener recv error; reconnecting");
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            break;
                        }
                    }
                }
                last_seen = pump(&pool, last_seen, &tx).await;
            }
        }
    });
}

/// Read all event rows with `seq > last`, broadcast them, return the new high-water mark.
async fn pump(pool: &PgPool, last: i64, tx: &broadcast::Sender<EventRow>) -> i64 {
    let rows: Result<Vec<EventDbRow>, _> = sqlx::query_as(
        "SELECT seq, tenant_id, type, entity, record_id, payload
               FROM sys_event_log
              WHERE seq > $1
              ORDER BY seq
              LIMIT 500",
    )
    .bind(last)
    .fetch_all(pool)
    .await;
    let Ok(rows) = rows else { return last };
    let mut hi = last;
    for r in rows {
        hi = r.seq.max(hi);
        let _ = tx.send(EventRow {
            seq: r.seq,
            tenant_id: r.tenant_id,
            typ: r.typ,
            entity: r.entity,
            record_id: r.record_id,
            payload: r.payload,
        });
    }
    hi
}

/// `sys_event_log` row shape used by the relay's queries.
#[derive(sqlx::FromRow)]
struct EventDbRow {
    seq: i64,
    tenant_id: Uuid,
    typ: String,
    entity: Option<String>,
    record_id: Option<Uuid>,
    payload: serde_json::Value,
}

#[derive(Deserialize, Default)]
struct StreamQuery {
    /// Optional subscription channel, e.g. `entity:Customer`,
    /// `record:Customer:<id>`, `user:<id>:notifications`,
    /// `tenant:<id>:broadcast`. Omit to receive all readable events.
    #[serde(default)]
    channel: Option<String>,
}

/// `GET /api/events` — SSE stream. Replays from `Last-Event-ID`, then live.
async fn stream(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Query(q): Query<StreamQuery>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let last_seq = headers
        .get("Last-Event-ID")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);

    let mut rx = st.events.subscribe();
    let pool = st.pool.clone();
    let filter = ChannelFilter::parse(q.channel.as_deref());

    // Spawn the producer: replay → live, AuthZ-filtered, into an mpsc the SSE
    // body drains. When the client disconnects the SSE body (and thus this
    // sender) is dropped; the next `send` errors and the task exits.
    let (tx_sse, rx_sse) = tokio::sync::mpsc::channel::<Event>(64);
    tokio::spawn(async move {
        let mut last = last_seq;

        // 1) replay (catch-up on (re)connect)
        last = replay(&pool, last, &user, &filter, &tx_sse).await;

        // 2) live
        use broadcast::error::RecvError;
        loop {
            tokio::select! {
                biased;
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    // periodic backfill in case a NOTIFY was missed
                    last = replay(&pool, last, &user, &filter, &tx_sse).await.max(last);
                }
                recv = rx.recv() => match recv {
                    Ok(ev) => {
                        if ev.seq <= last { continue; }
                        last = ev.seq;
                        if let Some(evt) = allow(&ev, &user, &filter) {
                            if tx_sse.send(evt).await.is_err() { break; }
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::debug!(lagged = n, "sse client lagged; resyncing via replay");
                        last = replay(&pool, last, &user, &filter, &tx_sse).await.max(last);
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    });

    Sse::new(ReceiverStream::new(rx_sse).map(Ok::<_, std::convert::Infallible>))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// Replay up to [`REPLAY_BATCH`] events newer than `last` that the caller may see.
async fn replay(
    pool: &PgPool,
    last: i64,
    user: &Identity,
    filter: &ChannelFilter,
    tx: &tokio::sync::mpsc::Sender<Event>,
) -> i64 {
    let rows: Result<Vec<EventDbRow>, _> = sqlx::query_as(
        "SELECT seq, tenant_id, type, entity, record_id, payload
           FROM sys_event_log
          WHERE tenant_id = $1 AND seq > $2
          ORDER BY seq
          LIMIT $3",
    )
    .bind(user.tenant_id)
    .bind(last)
    .bind(REPLAY_BATCH)
    .fetch_all(pool)
    .await;
    let Ok(rows) = rows else { return last };
    let mut hi = last;
    for r in rows {
        hi = r.seq.max(hi);
        let ev = EventRow {
            seq: r.seq,
            tenant_id: user.tenant_id,
            typ: r.typ,
            entity: r.entity,
            record_id: r.record_id,
            payload: r.payload,
        };
        if let Some(evt) = allow(&ev, user, filter) {
            if tx.send(evt).await.is_err() {
                break;
            }
        }
    }
    hi
}

/// AuthZ + channel filter → SSE [`Event`] (or `None` to drop).
fn allow(ev: &EventRow, user: &Identity, filter: &ChannelFilter) -> Option<Event> {
    if !filter.matches(ev, user) {
        return None;
    }
    let mut e = Event::default()
        .id(ev.seq.to_string())
        .event(ev.typ.clone());
    let data = if let Some(ent) = &ev.entity {
        json!({ "entity": ent, "record_id": ev.record_id, "payload": ev.payload })
    } else {
        json!({ "payload": ev.payload })
    };
    // json_data serializes; json! never fails to serialize.
    e = e.json_data(data).expect("sse json_data");
    Some(e)
}

#[derive(Clone)]
enum ChannelFilter {
    All,
    Entity(String),
    Record(String, Uuid),
    UserNotifications(Uuid),
    TenantBroadcast(Uuid),
}

impl ChannelFilter {
    fn parse(s: Option<&str>) -> Self {
        let Some(s) = s.map(str::trim) else {
            return Self::All;
        };
        if let Some(rest) = s.strip_prefix("entity:") {
            return Self::Entity(rest.to_string());
        }
        if let Some(rest) = s.strip_prefix("record:") {
            let mut parts = rest.splitn(2, ':');
            let ent = parts.next().unwrap_or("").to_string();
            if let Ok(id) = parts.next().unwrap_or("").parse::<Uuid>() {
                return Self::Record(ent, id);
            }
            return Self::Entity(ent);
        }
        if let Some(rest) = s.strip_prefix("user:") {
            if let Some(rest) = rest.strip_suffix(":notifications") {
                if let Ok(uid) = rest.parse::<Uuid>() {
                    return Self::UserNotifications(uid);
                }
            }
        }
        if let Some(rest) = s.strip_prefix("tenant:") {
            if let Some(rest) = rest.strip_suffix(":broadcast") {
                if let Ok(tid) = rest.parse::<Uuid>() {
                    return Self::TenantBroadcast(tid);
                }
            }
        }
        Self::All
    }

    fn matches(&self, ev: &EventRow, user: &Identity) -> bool {
        let system = ev.entity.is_none();
        match self {
            ChannelFilter::All => {
                if system {
                    // metadata.published / tenant broadcasts only for own tenant
                    ev.tenant_id == user.tenant_id
                } else {
                    readable(ev, user)
                }
            }
            ChannelFilter::Entity(name) => {
                ev.entity.as_deref() == Some(name.as_str()) && readable(ev, user)
            }
            ChannelFilter::Record(name, id) => {
                ev.entity.as_deref() == Some(name.as_str())
                    && ev.record_id == Some(*id)
                    && readable(ev, user)
            }
            ChannelFilter::UserNotifications(uid) => {
                ev.tenant_id == user.tenant_id
                    && ev.typ.starts_with("notification.")
                    && uid == &user.user_id
            }
            ChannelFilter::TenantBroadcast(tid) => ev.tenant_id == *tid && system,
        }
    }
}

/// Object-level read check on the event's entity (record/field grain on the
/// payload is a follow-up; the payload carries field *names* only, so a name
/// can't leak a value the viewer lacks).
fn readable(ev: &EventRow, user: &Identity) -> bool {
    ev.tenant_id == user.tenant_id
        && ev
            .entity
            .as_deref()
            .map(|e| user.can(e, "read") || user.is_superuser)
            .unwrap_or(false)
}
