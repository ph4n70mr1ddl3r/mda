//! Templating API (PLAN §5.19): author templates and render them under the
//! caller's identity.
//!
//! Render context is **AuthZ-filtered by construction**: when rendering against
//! a live record the engine loads it through the same record-scope + field-level
//! projection the data API uses (§5.11), so a template can never emit a field
//! the running user cannot read (same structural rule as reports, §5.17).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use mda_core::Error;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::data::{entity_def, project, scope_for};
use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/templates", post(create_template).get(list_templates))
        .route(
            "/api/templates/:name",
            get(get_template).delete(delete_template),
        )
        .route("/api/templates/:name/render", post(render_template))
}

#[derive(Debug, Deserialize)]
struct CreateBody {
    name: String,
    #[serde(default = "default_kind")]
    kind: String,
    body: String,
    #[serde(default = "default_content_type")]
    content_type: String,
    locale: Option<String>,
}

fn default_kind() -> String {
    "message".to_string()
}
fn default_content_type() -> String {
    "text/plain".to_string()
}

#[derive(Debug, Serialize)]
struct TemplateOut {
    name: String,
    kind: String,
    body: String,
    content_type: String,
    locale: Option<String>,
}

fn row_to_out(row: (String, String, String, String, Option<String>)) -> TemplateOut {
    TemplateOut {
        name: row.0,
        kind: row.1,
        body: row.2,
        content_type: row.3,
        locale: row.4,
    }
}

async fn create_template(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateBody>,
) -> ApiResult<(StatusCode, Json<TemplateOut>)> {
    if body.name.trim().is_empty() || body.body.trim().is_empty() {
        return Err(Error::Invalid("name and body are required".into()).into());
    }
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let row: Option<(String, String, String, String, Option<String>)> = sqlx::query_as(
        "INSERT INTO meta.md_template (tenant_id, name, kind, body, content_type, locale)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (tenant_id, name, locale) DO NOTHING
         RETURNING name, kind, body, content_type, locale",
    )
    .bind(user.tenant_id)
    .bind(&body.name)
    .bind(&body.kind)
    .bind(&body.body)
    .bind(&body.content_type)
    .bind(&body.locale)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    let row = row.ok_or_else(|| {
        Error::Conflict(format!(
            "template {} locale {:?} exists",
            body.name, body.locale
        ))
    })?;
    Ok((StatusCode::CREATED, Json(row_to_out(row))))
}

async fn list_templates(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<TemplateOut>>> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let rows: Vec<(String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT name, kind, body, content_type, locale FROM meta.md_template
          WHERE tenant_id = $1 ORDER BY name, locale NULLS FIRST",
    )
    .bind(user.tenant_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(rows.into_iter().map(row_to_out).collect()))
}

async fn get_template(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(name): Path<String>,
    Query(q): Query<LocaleQuery>,
) -> ApiResult<Json<TemplateOut>> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let row = find_template_tx(&mut tx, user.tenant_id, &name, q.locale.as_deref()).await?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(row_to_out(row)))
}

async fn delete_template(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(name): Path<String>,
    Query(q): Query<LocaleQuery>,
) -> ApiResult<StatusCode> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let n = sqlx::query("DELETE FROM meta.md_template WHERE tenant_id = $1 AND name = $2")
        .bind(user.tenant_id)
        .bind(&name)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?
        .rows_affected();
    tx.commit().await.map_err(Error::internal)?;
    let _ = q; // locale filter on delete is a refinement; delete all locales for the name.
    if n == 0 {
        return Err(Error::NotFound(format!("template {name}")).into());
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize, Default)]
struct LocaleQuery {
    locale: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RenderQuery {
    /// Render against a live record (AuthZ-projected under the caller). Omit to
    /// render against an explicit `context` body instead.
    entity: Option<String>,
    id: Option<Uuid>,
    locale: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RenderBody {
    /// Extra variables merged into the render context (params). Ignored when
    /// rendering against a record unless both are supplied (record wins).
    context: Option<Value>,
}

/// `POST /api/templates/:name/render[?entity=&id=&locale=]`.
///
/// Two render modes (both AuthZ-bounded by the caller's identity):
/// 1. **Record mode** (`?entity=&id=`) — loads the record through the data API
///    (record-scope + field-level projection), so the template sees only what
///    the caller can read. Context = `{ record, actor, params }`.
/// 2. **Context mode** (no entity) — renders against the caller-supplied
///    `context` object. The caller is responsible for AuthZ-filtering it.
async fn render_template(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(name): Path<String>,
    Query(q): Query<RenderQuery>,
    body: Option<Json<RenderBody>>,
) -> ApiResult<Json<Value>> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let (n, kind, tbody, content_type, locale) =
        find_template_tx(&mut tx, user.tenant_id, &name, q.locale.as_deref()).await?;
    tx.commit().await.map_err(Error::internal)?;
    let _ = n;
    let template = mda_reports::Template {
        name,
        kind,
        body: tbody,
        content_type,
        locale,
    };

    let params = body.and_then(|Json(b)| b.context).unwrap_or(json!({}));
    // Resolve the localized string bundle (§9/Phase 11) for the render locale
    // and inject it under `i18n` so a template localizes with `{{ i18n.k }}`.
    // Pure strings only — AuthZ-by-construction is preserved (a translation can
    // never carry a record field value).
    let bundle = crate::i18n::resolve_bundle(
        &st.pool,
        user.tenant_id,
        q.locale.as_deref().unwrap_or(""),
        None,
    )
    .await?;
    // Build a NESTED `i18n[namespace][key]` object so a template localizes with
    // `{{ i18n.email.subject }}` (dotted path resolution). Pure strings only —
    // AuthZ-by-construction is preserved (a translation can never carry a
    // record field value).
    let mut i18n: serde_json::Map<String, Value> = serde_json::Map::new();
    for ((ns, key), value) in bundle {
        let ns_obj = i18n
            .entry(ns)
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(o) = ns_obj.as_object_mut() {
            o.insert(key, Value::String(value));
        }
    }
    let ctx = match (q.entity.as_deref(), q.id) {
        (Some(entity), Some(id)) => {
            // Build the context from the live record, AuthZ-projected.
            let def = entity_def(&st, user.tenant_id, entity).await?;
            let scope = scope_for(&st, &user, entity).await?;
            let rec = mda_data::read(&st.pool, user.tenant_id, &def, id, &scope).await?;
            let projected = project(&user, entity, &def, rec);
            json!({ "record": projected, "actor": { "id": user.user_id }, "params": params, "i18n": i18n })
        }
        _ => {
            let mut ctx = match params {
                Value::Object(m) => m,
                other => {
                    let mut m = serde_json::Map::new();
                    m.insert("context".into(), other);
                    m
                }
            };
            ctx.insert("i18n".into(), Value::Object(i18n));
            Value::Object(ctx)
        }
    };

    let reg = mda_expression::Registry::new();
    let rendered = mda_reports::render(&template, &ctx, &reg).map_err(Error::internal)?;
    Ok(Json(json!({
        "content_type": rendered.content_type,
        "body": rendered.body,
    })))
}

/// Best-match locale resolution for a template name (exact → language-prefix →
/// default NULL → any). Runs inside the caller's tenant GUC transaction.
async fn find_template_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    name: &str,
    locale: Option<&str>,
) -> Result<(String, String, String, String, Option<String>), Error> {
    // 1) exact locale match
    if let Some(loc) = locale {
        let row: Option<(String, String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT name, kind, body, content_type, locale FROM meta.md_template
              WHERE tenant_id = $1 AND name = $2 AND locale = $3",
        )
        .bind(tenant)
        .bind(name)
        .bind(loc)
        .fetch_optional(&mut **tx)
        .await
        .map_err(Error::internal)?;
        if let Some(r) = row {
            return Ok(r);
        }
        // 2) language-prefix match (en-US → en)
        if let Some(lang) = loc.split('-').next() {
            if lang != loc {
                let row: Option<(String, String, String, String, Option<String>)> = sqlx::query_as(
                    "SELECT name, kind, body, content_type, locale FROM meta.md_template
                      WHERE tenant_id = $1 AND name = $2 AND locale = $3",
                )
                .bind(tenant)
                .bind(name)
                .bind(lang)
                .fetch_optional(&mut **tx)
                .await
                .map_err(Error::internal)?;
                if let Some(r) = row {
                    return Ok(r);
                }
            }
        }
    }
    // 3) default (NULL locale)
    let row: Option<(String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT name, kind, body, content_type, locale FROM meta.md_template
          WHERE tenant_id = $1 AND name = $2 AND locale IS NULL",
    )
    .bind(tenant)
    .bind(name)
    .fetch_optional(&mut **tx)
    .await
    .map_err(Error::internal)?;
    if let Some(r) = row {
        return Ok(r);
    }
    // 4) any locale for the name
    let row: Option<(String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT name, kind, body, content_type, locale FROM meta.md_template
          WHERE tenant_id = $1 AND name = $2 ORDER BY locale NULLS FIRST LIMIT 1",
    )
    .bind(tenant)
    .bind(name)
    .fetch_optional(&mut **tx)
    .await
    .map_err(Error::internal)?;
    row.ok_or_else(|| Error::NotFound(format!("template {name}")))
}
