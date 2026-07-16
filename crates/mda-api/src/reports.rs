//! Reporting API (PLAN §7 / §5.17): run a structured report under the caller's
//! identity, or export it as CSV.

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use mda_core::Error;
use mda_reports::Dataset;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/reports/:id/run", get(run_report))
        .route("/api/reports/:id/export", get(export_report))
}

async fn run_report(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<mda_reports::ReportResult>> {
    let ds = load_dataset(&st.pool, user.tenant_id, id).await?;
    let res = mda_reports::run(&st.pool, &user, &ds).await?;
    Ok(Json(res))
}

async fn export_report(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    let ds = load_dataset(&st.pool, user.tenant_id, id).await?;
    let res = mda_reports::run(&st.pool, &user, &ds).await?;
    let body = mda_reports::to_csv(&res);
    Ok(([(header::CONTENT_TYPE, "text/csv; charset=utf-8")], body).into_response())
}

async fn load_dataset(pool: &sqlx::PgPool, tenant: Uuid, id: Uuid) -> ApiResult<Dataset> {
    let (dataset,): (serde_json::Value,) =
        sqlx::query_as("SELECT dataset FROM meta.md_report WHERE id = $1 AND tenant_id = $2")
            .bind(id)
            .bind(tenant)
            .fetch_optional(pool)
            .await
            .map_err(Error::internal)?
            .ok_or_else(|| Error::NotFound(format!("report {id}")))?;
    let ds: Dataset =
        serde_json::from_value(dataset).map_err(|e| Error::Invalid(format!("bad dataset: {e}")))?;
    Ok(ds)
}
