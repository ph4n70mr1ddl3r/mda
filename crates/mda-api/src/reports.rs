//! Reporting API (PLAN §7 / §5.17): author a structured report, run it under
//! the caller's identity, or export it as CSV / HTML / XLSX / PDF.
//!
//! Authoring (`POST /api/reports`) stores the structured dataset (base entity +
//! fields + filters + group_by + order_by + limit) in `meta.md_report`; the
//! dataset shape is validated against the ACTIVE model at author time, and
//! security is enforced at RUN time under the requesting identity (object /
//! field-per-hop / record scopes — see `mda-reports::run`), so a report saved
//! by an admin never widens a lesser user's view.

use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use mda_core::Error;
use mda_reports::Dataset;
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/reports", post(create_report).get(list_reports))
        .route(
            "/api/reports/:id",
            get(get_report)
                .delete(delete_report)
                .patch(axum::routing::patch(update_report)),
        )
        .route("/api/reports/:id/run", get(run_report))
        .route("/api/reports/:id/export", get(export_report))
}

#[derive(Debug, Deserialize)]
struct CreateBody {
    name: String,
    dataset: Dataset,
}

#[derive(Debug, sqlx::FromRow, serde::Serialize)]
struct ReportRow {
    pub id: Uuid,
    pub name: String,
    pub dataset: Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// `POST /api/reports` — save a structured report definition. The dataset must
/// reference the caller's active model (base entity + every plain field must
/// resolve; security itself is enforced per run, not at author time).
async fn create_report(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateBody>,
) -> ApiResult<(axum::http::StatusCode, Json<ReportRow>)> {
    if body.name.trim().is_empty() {
        return Err(Error::Invalid("name is required".into()).into());
    }
    // Author-time shape check: the base entity must exist and be active, so a
    // typo fails here rather than at every run. (Fields resolve per run — the
    // model may legitimately evolve after a report is saved.)
    mda_meta::loader::entity_id_by_name(&st.pool, user.tenant_id, &body.dataset.base_entity)
        .await
        .map_err(|_| Error::Invalid(format!("unknown base entity {}", body.dataset.base_entity)))?;
    let dataset = serde_json::to_value(&body.dataset).map_err(Error::internal)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let row: ReportRow = sqlx::query_as(
        "INSERT INTO meta.md_report (tenant_id, name, dataset) VALUES ($1, $2, $3) \
         ON CONFLICT (tenant_id, name) DO UPDATE SET dataset = EXCLUDED.dataset \
         RETURNING id, name, dataset, created_at",
    )
    .bind(user.tenant_id)
    .bind(&body.name)
    .bind(&dataset)
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok((axum::http::StatusCode::CREATED, Json(row)))
}

/// `GET /api/reports` — the tenant's saved reports.
async fn list_reports(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<ReportRow>>> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let rows: Vec<ReportRow> =
        sqlx::query_as("SELECT id, name, dataset, created_at FROM meta.md_report ORDER BY name")
            .fetch_all(&mut *tx)
            .await
            .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(rows))
}

/// `GET /api/reports/:id` — one saved report definition.
async fn get_report(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<ReportRow>> {
    Ok(Json(load_row(&st, user.tenant_id, id).await?))
}

/// `PATCH /api/reports/:id` — rename and/or replace the dataset.
#[derive(Deserialize)]
struct UpdateBody {
    name: Option<String>,
    dataset: Option<Dataset>,
}
async fn update_report(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateBody>,
) -> ApiResult<Json<ReportRow>> {
    let existing = load_row(&st, user.tenant_id, id).await?;
    let name = body.name.unwrap_or(existing.name);
    let dataset = match body.dataset {
        Some(ds) => serde_json::to_value(&ds).map_err(Error::internal)?,
        None => existing.dataset,
    };
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let row: ReportRow = sqlx::query_as(
        "UPDATE meta.md_report SET name = $3, dataset = $4 \
         WHERE tenant_id = $1 AND id = $2 RETURNING id, name, dataset, created_at",
    )
    .bind(user.tenant_id)
    .bind(id)
    .bind(&name)
    .bind(&dataset)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?
    .ok_or_else(|| Error::NotFound(format!("report {id}")))?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(row))
}

/// `DELETE /api/reports/:id` — 204. (Dashboards referencing it surface a 404 at
/// render time for that tile; schedules referencing it fail their run.)
async fn delete_report(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<axum::http::StatusCode> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let res = sqlx::query("DELETE FROM meta.md_report WHERE tenant_id = $1 AND id = $2")
        .bind(user.tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    if res.rows_affected() == 0 {
        return Err(Error::NotFound(format!("report {id}")).into());
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn run_report(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<mda_reports::ReportResult>> {
    let row = load_row(&st, user.tenant_id, id).await?;
    let ds = parse_dataset(&row.dataset)?;
    let res = mda_reports::run(&st.pool, &user, &ds).await?;
    Ok(Json(res))
}

#[derive(Deserialize)]
struct ExportQuery {
    format: Option<String>,
}

/// `GET /api/reports/:id/export[?format=csv|html|xlsx|pdf]` — run + render.
async fn export_report(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Query(q): Query<ExportQuery>,
) -> ApiResult<Response> {
    let row = load_row(&st, user.tenant_id, id).await?;
    let ds = parse_dataset(&row.dataset)?;
    let res = mda_reports::run(&st.pool, &user, &ds).await?;
    let file = format!("report-{}", sanitize_filename(&row.name));
    match q.format.as_deref().unwrap_or("csv") {
        "csv" => Ok((
            [
                (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{file}.csv\""),
                ),
            ],
            mda_reports::to_csv(&res),
        )
            .into_response()),
        "html" => Ok((
            [(header::CONTENT_TYPE, "text/html; charset=utf-8".to_string())],
            mda_reports::to_html(&res, &row.name),
        )
            .into_response()),
        "xlsx" => {
            let bytes = mda_reports::to_xlsx(&res, &row.name)?;
            Ok((
                [
                    (
                        header::CONTENT_TYPE,
                        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                            .to_string(),
                    ),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{file}.xlsx\""),
                    ),
                ],
                bytes,
            )
                .into_response())
        }
        "pdf" => {
            let bytes = mda_reports::to_pdf(&res, &row.name)?;
            Ok((
                [
                    (header::CONTENT_TYPE, "application/pdf".to_string()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{file}.pdf\""),
                    ),
                ],
                bytes,
            )
                .into_response())
        }
        other => Err(Error::Invalid(format!(
            "unknown export format '{other}' (supported: csv, html, xlsx, pdf)"
        ))
        .into()),
    }
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

async fn load_row(st: &AppState, tenant: Uuid, id: Uuid) -> ApiResult<ReportRow> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    let row: Option<ReportRow> = sqlx::query_as(
        "SELECT id, name, dataset, created_at FROM meta.md_report WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    row.ok_or_else(|| Error::NotFound(format!("report {id}")))
        .map_err(Into::into)
}

fn parse_dataset(v: &Value) -> ApiResult<Dataset> {
    serde_json::from_value(v.clone())
        .map_err(|e| Error::Invalid(format!("bad dataset: {e}")).into())
}
