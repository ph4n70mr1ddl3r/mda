//! Scheduled-job management (PLAN §14).
//!
//! Modeler-defined schedules — cron-driven, tenant-scoped — that fire due jobs
//! and record each run. Closes the remaining §14 "scheduled-job management" gap:
//! a generic scheduler exposing next-run / last-run / last-status / failure state
//! plus a per-run history, surfaced both as a REST management API and in the
//! observability console.
//!
//! Design:
//! - `sys_schedule` holds the cron, the scheduled target, and the *running user*
//!   whose AuthZ context is captured at run time (mirrors `md_report_schedule`
//!   semantics — a revoked running user correctly stops the schedule).
//! - The worker claims due rows with `FOR UPDATE SKIP LOCKED` (multi-instance
//!   safe, like the outbox drain), advances `next_run` *before* dispatch so a
//!   transient failure never blocks the schedule, then runs the job and records
//!   `sys_schedule_run` + `last_status`/`last_error`.
//! - Dispatch is by `kind`: `report` runs a saved report under the running user;
//!   `custom` is a no-op hook (extensibility + the scheduler test). Additional
//!   kinds (`integration` pull, scheduled `rule`) follow the same shape.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use cron::Schedule;
use mda_core::{Error, Result};
use mda_security::{load_identity, set_tenant};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::str::FromStr;
use std::time::Duration;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/schedules", get(list_schedules).post(create_schedule))
        .route(
            "/api/schedules/:id",
            get(get_schedule)
                .patch(update_schedule)
                .delete(delete_schedule),
        )
        .route("/api/schedules/:id/run", post(trigger_schedule))
        .route("/api/schedules/:id/runs", get(list_runs))
}

// ===== models =====

#[derive(Debug, sqlx::FromRow, Serialize)]
struct ScheduleRow {
    id: Uuid,
    tenant_id: Uuid,
    name: String,
    kind: String,
    target_id: Uuid,
    cron: String,
    enabled: bool,
    running_user_id: Option<Uuid>,
    next_run: Option<DateTime<Utc>>,
    last_run: Option<DateTime<Utc>>,
    last_status: Option<String>,
    last_error: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct CreateSchedule {
    name: String,
    /// `report` (run a saved report) | `custom` (extensibility hook).
    kind: String,
    target_id: Uuid,
    /// 6-field cron (UTC): `sec min hour dom month dow`.
    cron: String,
    #[serde(default)]
    running_user_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, Default)]
struct UpdateSchedule {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    cron: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    running_user_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    kind: Option<String>,
}

// ===== handlers =====

/// `GET /api/schedules[?kind=]`
async fn list_schedules(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<ScheduleRow>>> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let rows = match q.kind.as_deref() {
        Some(kind) => sqlx::query_as::<_, ScheduleRow>(
            "SELECT * FROM sys_schedule WHERE tenant_id = $1 AND kind = $2 ORDER BY name",
        )
        .bind(user.tenant_id)
        .bind(kind)
        .fetch_all(&mut *tx)
        .await
        .map_err(Error::internal)?,
        None => sqlx::query_as::<_, ScheduleRow>(
            "SELECT * FROM sys_schedule WHERE tenant_id = $1 ORDER BY name",
        )
        .bind(user.tenant_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(Error::internal)?,
    };
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(rows))
}

/// `GET /api/schedules/:id`
async fn get_schedule(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<ScheduleRow>> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let row: ScheduleRow = sqlx::query_as("SELECT * FROM sys_schedule WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Error::internal)?
        .ok_or_else(|| Error::NotFound(format!("schedule {id}")))?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(row))
}

/// `POST /api/schedules`
async fn create_schedule(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateSchedule>,
) -> ApiResult<(StatusCode, Json<ScheduleRow>)> {
    validate_kind(&body.kind)?;
    let cron = parse_cron(&body.cron)?;
    let running_user = body.running_user_id.unwrap_or(user.user_id);
    // Arm the first run.
    let next_run = cron.after(&Utc::now()).next();
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let row: ScheduleRow = sqlx::query_as(
        "INSERT INTO sys_schedule
            (tenant_id, name, kind, target_id, cron, enabled, running_user_id, next_run)
         VALUES ($1, $2, $3, $4, $5, TRUE, $6, $7)
         RETURNING *",
    )
    .bind(user.tenant_id)
    .bind(&body.name)
    .bind(&body.kind)
    .bind(body.target_id)
    .bind(&body.cron)
    .bind(running_user)
    .bind(next_run)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref d) if d.is_unique_violation() => {
            Error::Invalid("schedule name already exists".into())
        }
        other => Error::internal(other),
    })?;
    tx.commit().await.map_err(Error::internal)?;
    Ok((StatusCode::CREATED, Json(row)))
}

/// `PATCH /api/schedules/:id` — rename, re-cron, enable/disable, or re-arm.
async fn update_schedule(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateSchedule>,
) -> ApiResult<Json<ScheduleRow>> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    // Fetch under the tenant GUC so a wrong-tenant id is a 404.
    let existing: ScheduleRow = sqlx::query_as("SELECT * FROM sys_schedule WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Error::internal)?
        .ok_or_else(|| Error::NotFound(format!("schedule {id}")))?;

    let name = body.name.unwrap_or(existing.name);
    let cron_str = body.cron.clone().unwrap_or_else(|| existing.cron.clone());
    let cron = parse_cron(&cron_str)?;
    let enabled = body.enabled.unwrap_or(existing.enabled);
    let running_user = body.running_user_id.or(existing.running_user_id);
    // Re-arm on cron change or (re-)enable; a cron-only edit keeps cadence honest.
    let rearm = body.cron.is_some() || (enabled && existing.next_run.is_none());
    let next_run = if enabled {
        Some(if rearm {
            cron.after(&Utc::now()).next()
        } else {
            existing.next_run
        })
    } else {
        None
    };

    let row: ScheduleRow = sqlx::query_as(
        "UPDATE sys_schedule
            SET name = $3, cron = $4, enabled = $5, running_user_id = $6,
                next_run = $7, updated_at = now()
          WHERE id = $1 AND tenant_id = $2
         RETURNING *",
    )
    .bind(id)
    .bind(user.tenant_id)
    .bind(&name)
    .bind(&cron_str)
    .bind(enabled)
    .bind(running_user)
    .bind(next_run)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?
    .ok_or_else(|| Error::NotFound(format!("schedule {id}")))?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(row))
}

/// `DELETE /api/schedules/:id`
async fn delete_schedule(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let n = sqlx::query("DELETE FROM sys_schedule WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(user.tenant_id)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?
        .rows_affected();
    tx.commit().await.map_err(Error::internal)?;
    if n == 0 {
        Err(Error::NotFound(format!("schedule {id}")).into())
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}

/// `POST /api/schedules/:id/run` — fire immediately (out of cadence), recording
/// a run row. Returns the run result.
async fn trigger_schedule(
    State(st): State<AppState>,
    AuthUser(_user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let sched = fetch_for_dispatch(&st.pool, id).await?;
    let res = dispatch(&st.pool, &sched).await;
    // A manual trigger records history too, so the run surface is the same
    // whether the job fired on cadence or by hand.
    record_run(&st.pool, sched.id, sched.tenant, &res).await;
    Ok(Json(run_envelope(&res)))
}

/// `GET /api/schedules/:id/runs` — run history (newest first).
async fn list_runs(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<serde_json::Value>>> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let rows: Vec<(serde_json::Value,)> = sqlx::query_as(
        "SELECT to_jsonb(r.*) FROM sys_schedule_run r
          WHERE r.tenant_id = $1 AND r.schedule_id = $2
          ORDER BY r.started_at DESC LIMIT 100",
    )
    .bind(user.tenant_id)
    .bind(id)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(rows.into_iter().map(|(v,)| v).collect()))
}

// ===== worker =====

/// How often the worker scans for due schedules.
const TICK: Duration = Duration::from_secs(5);

/// Spawn the scheduler worker. Multi-instance safe: due rows are claimed with
/// `FOR UPDATE SKIP LOCKED`. Like the outbox drain, it drains *first* then sleeps
/// so a freshly-armed schedule is picked up on the next tick, not a full interval
/// later.
pub fn spawn_scheduler(pool: PgPool) {
    tokio::spawn(async move {
        tracing::info!("scheduler worker started");
        loop {
            if let Err(e) = tick(&pool).await {
                tracing::warn!(?e, "scheduler tick failed");
            }
            tokio::time::sleep(TICK).await;
        }
    });
}

#[derive(Debug, sqlx::FromRow)]
struct DueRow {
    id: Uuid,
    tenant_id: Uuid,
    name: String,
    target_id: Uuid,
    running_user_id: Option<Uuid>,
    kind: String,
    cron: String,
    next_run: DateTime<Utc>,
}

/// One scheduling pass: claim all due rows, dispatch each, record the outcome.
async fn tick(pool: &PgPool) -> Result<(), sqlx::Error> {
    loop {
        let mut tx = pool.begin().await?;
        // Claim one due row and advance its next_run inside this transaction so
        // the lock is held only across the bookkeeping, not the (possibly slow)
        // job execution.
        let claimed: Option<DueRow> = sqlx::query_as(
            "SELECT id, tenant_id, name, target_id, running_user_id, kind, cron, next_run
               FROM sys_schedule
              WHERE enabled AND next_run IS NOT NULL AND next_run <= now()
              ORDER BY next_run
              LIMIT 1
              FOR UPDATE SKIP LOCKED",
        )
        .fetch_optional(&mut *tx)
        .await?;

        let Some(due_row) = claimed else {
            tx.commit().await?;
            break; // nothing due
        };

        // Compute the next firing (strictly after the one we're running) and
        // stamp last_run/last_status before releasing the row.
        let next = Schedule::from_str(&due_row.cron)
            .ok()
            .and_then(|s| s.after(&due_row.next_run).next());
        sqlx::query(
            "UPDATE sys_schedule
                SET last_run = now(), last_status = 'running',
                    next_run = $2, updated_at = now()
              WHERE id = $1",
        )
        .bind(due_row.id)
        .bind(next)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        let sched = DispatchTarget {
            id: due_row.id,
            tenant: due_row.tenant_id,
            name: due_row.name,
            kind: due_row.kind,
            target_id: due_row.target_id,
            running_user_id: due_row.running_user_id,
        };
        let res = dispatch(pool, &sched).await;
        record_run(pool, sched.id, sched.tenant, &res).await;
    }
    Ok(())
}

#[derive(Clone)]
struct DispatchTarget {
    id: Uuid,
    tenant: Uuid,
    name: String,
    kind: String,
    target_id: Uuid,
    running_user_id: Option<Uuid>,
}

/// Fetch a schedule by id for dispatch (used by the manual trigger).
async fn fetch_for_dispatch(pool: &PgPool, id: Uuid) -> Result<DispatchTarget> {
    let row: Option<(Uuid, Uuid, String, Uuid, Option<Uuid>, String)> = sqlx::query_as(
        "SELECT id, tenant_id, name, target_id, running_user_id, kind
           FROM sys_schedule WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(Error::internal)?;
    let (id, tenant, name, target_id, running_user_id, kind) =
        row.ok_or_else(|| Error::NotFound(format!("schedule {id}")))?;
    Ok(DispatchTarget {
        id,
        tenant,
        name,
        kind,
        target_id,
        running_user_id,
    })
}

/// The outcome of one dispatch.
struct RunOutcome {
    status: &'static str, // ok | failed
    rows: i64,
    error: Option<String>,
}

/// Execute the scheduled job under its running user's AuthZ.
async fn dispatch(pool: &PgPool, sched: &DispatchTarget) -> RunOutcome {
    let span = tracing::info_span!(
        "schedule_dispatch",
        schedule = %sched.id,
        kind = %sched.kind,
        name = %sched.name
    );
    let _enter = span.enter();
    let Some(running_user_id) = sched.running_user_id else {
        return RunOutcome {
            status: "failed",
            rows: 0,
            error: Some("schedule has no running user".into()),
        };
    };
    let identity = match load_identity(pool, running_user_id, sched.tenant).await {
        Ok(id) => id,
        Err(e) => {
            return RunOutcome {
                status: "failed",
                rows: 0,
                error: Some(format!("running user not resolvable: {e}")),
            }
        }
    };

    let result = match sched.kind.as_str() {
        "report" => run_report(pool, &identity, sched.target_id).await,
        // `custom` is an extensibility hook: it succeeds with no rows, letting
        // operators test the scheduler end-to-end and external integrations
        // observe the run via sys_schedule_run.
        "custom" => Ok(0),
        other => Err(Error::Invalid(format!("unknown schedule kind {other}"))),
    };

    match result {
        Ok(rows) => RunOutcome {
            status: "ok",
            rows,
            error: None,
        },
        Err(e) => RunOutcome {
            status: "failed",
            rows: 0,
            error: Some(e.to_string()),
        },
    }
}

/// Run a saved report under `identity` and return its row count.
async fn run_report(
    pool: &PgPool,
    identity: &mda_security::Identity,
    report_id: Uuid,
) -> Result<i64> {
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, identity.tenant_id).await?;
    let (dataset,): (serde_json::Value,) =
        sqlx::query_as("SELECT dataset FROM meta.md_report WHERE id = $1 AND tenant_id = $2")
            .bind(report_id)
            .bind(identity.tenant_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Error::internal)?
            .ok_or_else(|| Error::NotFound(format!("report {report_id}")))?;
    tx.commit().await.map_err(Error::internal)?;

    let ds: mda_reports::Dataset =
        serde_json::from_value(dataset).map_err(|e| Error::Invalid(format!("bad dataset: {e}")))?;
    let res = mda_reports::run(pool, identity, &ds).await?;
    Ok(res.rows.len() as i64)
}

/// Record a run row and update the schedule's last_status/last_error.
async fn record_run(pool: &PgPool, id: Uuid, tenant: Uuid, res: &RunOutcome) {
    let started = Utc::now();
    if let Err(e) = sqlx::query(
        "INSERT INTO sys_schedule_run (tenant_id, schedule_id, status, rows_affected, error, finished_at)
         VALUES ($1, $2, $3, $4, $5, now())",
    )
    .bind(tenant)
    .bind(id)
    .bind(res.status)
    .bind(res.rows)
    .bind(&res.error)
    .execute(pool)
    .await
    {
        tracing::warn!(?e, "sys_schedule_run insert failed");
    }
    if let Err(e) = sqlx::query(
        "UPDATE sys_schedule
            SET last_status = $2, last_error = $3, updated_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(res.status)
    .bind(&res.error)
    .execute(pool)
    .await
    {
        tracing::warn!(?e, "sys_schedule status update failed");
    }
    let _ = started;
}

fn run_envelope(res: &RunOutcome) -> serde_json::Value {
    serde_json::json!({
        "status": res.status,
        "rows": res.rows,
        "error": res.error,
    })
}

// ===== cron helpers =====

/// Validate a 6-field cron expression and return the parsed schedule.
fn parse_cron(expr: &str) -> Result<Schedule> {
    Schedule::from_str(expr).map_err(|e| Error::Invalid(format!("invalid cron '{expr}': {e}")))
}

fn validate_kind(kind: &str) -> Result<()> {
    match kind {
        "report" | "custom" => Ok(()),
        other => Err(Error::Invalid(format!(
            "unknown schedule kind '{other}' (supported: report, custom)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_parses_and_advances() {
        // The cron crate uses [sec min hour dom month dow] (6 fields). A 5-field
        // expression is interpreted with an implicit leading seconds field.
        for expr in ["* * * * * *", "*/2 * * * * *", "0 * * * * *"] {
            let s = parse_cron(expr).unwrap_or_else(|e| panic!("{expr}: {e}"));
            let now = Utc::now();
            let next = s
                .after(&now)
                .next()
                .unwrap_or_else(|| panic!("no next for {expr}"));
            assert!(next > now);
        }
    }

    #[test]
    fn invalid_cron_rejected() {
        assert!(parse_cron("not a cron").is_err());
    }

    #[test]
    fn kind_validation() {
        assert!(validate_kind("report").is_ok());
        assert!(validate_kind("custom").is_ok());
        assert!(validate_kind("bogus").is_err());
    }
}
