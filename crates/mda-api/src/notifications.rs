//! Notifications API (PLAN §5.18): the authenticated user's in-app inbox.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch};
use axum::{Json, Router};
use mda_core::Error;
use serde_json::Value;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/notifications", get(list_notifications))
        .route("/api/notifications/:id", patch(mark_read))
}

async fn list_notifications(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<Value>>> {
    let rows: Vec<(Value,)> = sqlx::query_as(
        "SELECT to_jsonb(n.*) AS doc FROM sys_notification n
          WHERE tenant_id = $1 AND user_id = $2
          ORDER BY read_at NULLS FIRST, created_at DESC LIMIT 50",
    )
    .bind(user.tenant_id)
    .bind(user.user_id)
    .fetch_all(&st.pool)
    .await
    .map_err(Error::internal)?;
    Ok(Json(rows.into_iter().map(|(v,)| v).collect()))
}

#[derive(serde::Deserialize)]
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
