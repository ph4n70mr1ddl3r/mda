//! UI definitions API (PLAN Phase 6): forms, views, dashboards, navigation.
//!
//! These are the *presentation* half of the metadata-driven runtime — the
//! renderable JSON the Runtime UI interprets. Two structural rules:
//!
//! 1. **A UI definition can never widen access.** Render endpoints resolve the
//!    stored definition against the ACTIVE model and the CALLER's security:
//!    form/view fields the caller cannot read are dropped, dashboard tiles run
//!    their reports under the requesting identity (§5.17), and navigation
//!    entity items are permission-filtered. Authoring stores only the shape.
//! 2. **The model is the source of truth.** A stored definition referencing a
//!    retired/renamed field simply drops it at render time (never a 500); an
//!    entity with no stored form/view renders from a synthesized default built
//!    from the field registry (order = definition order, widget inferred from
//!    the field type), so the runtime works with zero authored UI metadata.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use mda_core::Error;
use mda_security::{Access, Identity};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::data::entity_def;
use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/forms/:entity", get(render_form).post(upsert_form))
        .route(
            "/api/forms/:entity/:name",
            axum::routing::delete(delete_form),
        )
        .route("/api/views/:entity", get(render_view).post(upsert_view))
        .route(
            "/api/views/:entity/:name",
            axum::routing::delete(delete_view),
        )
        .route(
            "/api/dashboards",
            get(list_dashboards).post(upsert_dashboard),
        )
        .route(
            "/api/dashboards/:id",
            get(render_dashboard).delete(delete_dashboard),
        )
        .route(
            "/api/navigation",
            get(render_navigation).post(upsert_navigation),
        )
        .route(
            "/api/navigation/:name",
            axum::routing::delete(delete_navigation),
        )
}

#[derive(Deserialize)]
struct NameQuery {
    name: Option<String>,
}

// ===== forms =====

/// `GET /api/forms/:entity[?name=default]` — renderable form definition.
async fn render_form(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(entity): Path<String>,
    Query(q): Query<NameQuery>,
) -> ApiResult<Json<Value>> {
    let name = q.name.unwrap_or_else(|| "default".to_string());
    let def = entity_def(&st, user.tenant_id, &entity).await?;
    let stored = load_ui_row(&st, user.tenant_id, "meta.md_form", &entity, &name).await?;
    // reference targets (FK field name -> target entity name) so the client can
    // offer a picker: it lists the target entity's records itself, under its own
    // identity (object read on the target required — never a leak).
    let mut ref_targets: std::collections::HashMap<String, String> = Default::default();
    for r in &def.relationships {
        if let Ok(t) =
            mda_meta::loader::load_entity_definition(&st.pool, user.tenant_id, r.target_entity_id)
                .await
        {
            ref_targets.insert(r.source_field_name.clone(), t.entity.name.clone());
        }
    }
    let mut sections = match stored
        .as_ref()
        .and_then(|r| r.get("layout"))
        .and_then(|l| l.get("sections"))
        .and_then(|s| s.as_array())
    {
        Some(secs) => secs.clone(),
        None => vec![json!({"title": null, "fields": default_fields(&def)})],
    };
    // resolve every section's fields against the model + caller's FLS
    for sec in sections.iter_mut() {
        let fields = sec
            .get("fields")
            .and_then(|f| f.as_array())
            .cloned()
            .unwrap_or_default();
        let mut resolved = Vec::new();
        for f in fields {
            let fname = f.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if let Some(rf) = resolve_field_spec(&user, &entity, &def, fname, f.get("widget")) {
                // keep author overrides (label/widget) on top of the model truth
                let mut out = rf;
                if let Some(label) = f.get("label").and_then(|l| l.as_str()) {
                    out["label"] = json!(label);
                }
                resolved.push(out);
            }
        }
        sec["fields"] = json!(resolved);
    }
    // attach target entities to reference widgets (both stored + default forms)
    for sec in sections.iter_mut() {
        if let Some(fields) = sec.get_mut("fields").and_then(|f| f.as_array_mut()) {
            for f in fields.iter_mut() {
                if f.get("widget").and_then(|w| w.as_str()) == Some("reference") {
                    if let Some(name) = f.get("name").and_then(|n| n.as_str()) {
                        if let Some(target) = ref_targets.get(name) {
                            f["target_entity"] = json!(target);
                        }
                    }
                }
            }
        }
    }
    Ok(Json(json!({
        "entity": entity,
        "name": name,
        "label": stored.as_ref().and_then(|r| r.get("label")).cloned()
            .unwrap_or(json!(def.entity.label.clone().unwrap_or_else(|| entity.clone()))),
        "sections": sections,
    })))
}

/// `POST /api/forms/:entity` — create/replace a form definition (upsert by
/// `(entity, name)`).
#[derive(Deserialize)]
struct UpsertUiBody {
    #[serde(default = "default_name")]
    name: String,
    label: Option<String>,
    layout: Option<Value>,
    // view fields
    columns: Option<Value>,
    filters: Option<Value>,
    sort: Option<Value>,
    page_size: Option<i64>,
    // dashboard / navigation fields
    items: Option<Value>,
}
fn default_name() -> String {
    "default".to_string()
}

async fn upsert_form(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(entity): Path<String>,
    Json(body): Json<UpsertUiBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let layout = body.layout.unwrap_or_else(|| json!({"sections": []}));
    validate_layout(&layout)?;
    upsert_ui_row(
        &st,
        user.tenant_id,
        "meta.md_form",
        &entity,
        &body.name,
        body.label.as_deref(),
        "layout",
        &layout,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"entity": entity, "name": body.name})),
    ))
}

/// `DELETE /api/forms/:entity/:name` — remove a form definition.
async fn delete_form(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((entity, name)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    delete_ui_row(&st, user.tenant_id, "meta.md_form", &entity, &name).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ===== views =====

/// `GET /api/views/:entity[?name=default]` — renderable list-view definition
/// (columns + default filters/sort/page size), FLS-projected.
async fn render_view(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(entity): Path<String>,
    Query(q): Query<NameQuery>,
) -> ApiResult<Json<Value>> {
    let name = q.name.unwrap_or_else(|| "default".to_string());
    let def = entity_def(&st, user.tenant_id, &entity).await?;
    let stored = load_ui_row(&st, user.tenant_id, "meta.md_view", &entity, &name).await?;
    let columns = match stored
        .as_ref()
        .and_then(|r| r.get("columns"))
        .and_then(|c| c.as_array())
    {
        Some(cols) if !cols.is_empty() => cols.clone(),
        _ => default_columns(&def),
    };
    // resolve: keep only fields that exist AND are readable
    let columns: Vec<Value> = columns
        .into_iter()
        .filter_map(|c| {
            let fname = c.get("field").and_then(|f| f.as_str()).unwrap_or("");
            if user.field_access(&entity, fname) == Access::None {
                return None;
            }
            let fdef = def.fields.iter().find(|f| f.name == fname)?;
            let mut out = json!({
                "field": fname,
                "label": c.get("label").and_then(|l| l.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| fdef.label.clone().unwrap_or_else(|| fname.to_string())),
                "type": fdef.field_type,
            });
            if let Some(w) = c.get("width").and_then(|w| w.as_i64()) {
                out["width"] = json!(w);
            }
            Some(out)
        })
        .collect();
    Ok(Json(json!({
        "entity": entity,
        "name": name,
        "label": stored.as_ref().and_then(|r| r.get("label")).cloned()
            .unwrap_or(json!(def.entity.label.clone().unwrap_or_else(|| entity.clone()))),
        "columns": columns,
        "filters": stored.as_ref().and_then(|r| r.get("filters")).cloned().unwrap_or(json!([])),
        "sort": stored.as_ref().and_then(|r| r.get("sort")).cloned().unwrap_or(json!([])),
        "page_size": stored.as_ref().and_then(|r| r.get("page_size")).cloned().unwrap_or(Value::Null),
    })))
}

async fn upsert_view(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(entity): Path<String>,
    Json(body): Json<UpsertUiBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let columns = body.columns.unwrap_or_else(|| json!([]));
    if !columns.is_array() {
        return Err(Error::Invalid("columns must be an array".into()).into());
    }
    // shape-check each column against the model so a typo fails at author time
    let def = entity_def(&st, user.tenant_id, &entity).await?;
    for c in columns.as_array().unwrap() {
        let f = c.get("field").and_then(|f| f.as_str()).unwrap_or("");
        if !def.fields.iter().any(|x| x.name == f) {
            return Err(Error::Invalid(format!("unknown field '{f}' in view columns")).into());
        }
    }
    upsert_ui_row_multi(
        &st,
        user.tenant_id,
        "meta.md_view",
        &entity,
        &body.name,
        body.label.as_deref(),
        &[
            ("columns", &columns),
            ("filters", &body.filters.clone().unwrap_or(json!([]))),
            ("sort", &body.sort.clone().unwrap_or(json!([]))),
        ],
        body.page_size,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"entity": entity, "name": body.name})),
    ))
}

async fn delete_view(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((entity, name)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    delete_ui_row(&st, user.tenant_id, "meta.md_view", &entity, &name).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ===== dashboards =====

/// `GET /api/dashboards` — list definitions (no results).
async fn list_dashboards(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<Value>>> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let rows: Vec<(Uuid, String, Option<String>, Value)> = sqlx::query_as(
        "SELECT id, name, label, items FROM meta.md_dashboard WHERE active ORDER BY name",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, name, label, items)| {
                json!({"id": id, "name": name, "label": label.unwrap_or_else(|| name.clone()), "items": items})
            })
            .collect(),
    ))
}

/// `GET /api/dashboards/:id` — the definition with every tile's report **run
/// under the caller's identity** (object/field/record security applies per
/// run, §5.17 — a dashboard is a saved lens, not a stored result set).
async fn render_dashboard(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let row: Option<(String, Option<String>, Value)> =
        sqlx::query_as("SELECT name, label, items FROM meta.md_dashboard WHERE id = $1 AND active")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    let (name, label, items) = row.ok_or_else(|| Error::NotFound(format!("dashboard {id}")))?;
    let mut tiles = Vec::new();
    for item in items.as_array().cloned().unwrap_or_default() {
        let report_id = item
            .get("report_id")
            .and_then(|r| r.as_str())
            .and_then(|r| Uuid::parse_str(r).ok());
        let title = item
            .get("title")
            .cloned()
            .unwrap_or_else(|| json!("report"));
        let mut tile = json!({"title": title});
        if let Some(span) = item.get("span").and_then(|s| s.as_i64()) {
            tile["span"] = json!(span);
        }
        match report_id {
            None => tile["error"] = json!("tile has no report_id"),
            Some(rid) => match load_report(&st, user.tenant_id, rid).await {
                Err(_) => tile["error"] = json!("report not found"),
                Ok((rname, dataset)) => match mda_reports::run(&st.pool, &user, &dataset).await {
                    Ok(res) => {
                        tile["report"] = json!({"id": rid, "name": rname});
                        tile["result"] = serde_json::to_value(&res).map_err(Error::internal)?;
                    }
                    Err(e) => tile["error"] = json!(e.to_string()),
                },
            },
        }
        tiles.push(tile);
    }
    Ok(Json(json!({
        "id": id,
        "name": name,
        "label": label.unwrap_or_else(|| name.clone()),
        "items": tiles,
    })))
}

async fn upsert_dashboard(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<UpsertUiBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let name = body.name.clone();
    if name.trim().is_empty() || name == "default" {
        return Err(Error::Invalid("a dashboard needs an explicit name".into()).into());
    }
    let items = body.items.unwrap_or_else(|| json!([]));
    if !items.is_array() {
        return Err(Error::Invalid("items must be an array".into()).into());
    }
    for item in items.as_array().unwrap() {
        // trim + empty check: an empty-string report_id passes a bare
        // `is_none()` check, gets stored, and the tile can never resolve.
        if item
            .get("report_id")
            .and_then(|r| r.as_str())
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            return Err(Error::Invalid("every dashboard item needs a report_id".into()).into());
        }
    }
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO meta.md_dashboard (tenant_id, name, label, items) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (tenant_id, name) DO UPDATE \
            SET label = EXCLUDED.label, items = EXCLUDED.items, updated_at = now() \
         RETURNING id",
    )
    .bind(user.tenant_id)
    .bind(&name)
    .bind(&body.label)
    .bind(&items)
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok((StatusCode::CREATED, Json(json!({"id": id, "name": name}))))
}

async fn delete_dashboard(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let res = sqlx::query("DELETE FROM meta.md_dashboard WHERE tenant_id = $1 AND id = $2")
        .bind(user.tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    if res.rows_affected() == 0 {
        return Err(Error::NotFound(format!("dashboard {id}")).into());
    }
    Ok(StatusCode::NO_CONTENT)
}

// ===== navigation =====

/// `GET /api/navigation` — the caller's navigation tree. Entity items are
/// filtered to entities the caller may **read**; an unreadable entity never
/// appears in anyone's menu. With no stored navigation the caller gets the
/// default menu: every readable entity, model order.
async fn render_navigation(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Value>> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let stored: Option<Value> = sqlx::query_scalar(
        "SELECT items FROM meta.md_navigation WHERE tenant_id = $1 AND name = 'default' AND active",
    )
    .bind(user.tenant_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;

    let readable = readable_entities(&st, &user).await?;
    let items: Vec<Value> = match stored.and_then(|s| s.as_array().cloned()) {
        Some(authored) => authored
            .into_iter()
            .filter_map(|it| match it.get("type").and_then(|t| t.as_str()) {
                Some("entity") => {
                    let e = it.get("entity").and_then(|e| e.as_str())?;
                    let (name, model_label) = readable.iter().find(|(name, _)| name == e)?;
                    // the authored label wins when present; else the model's
                    let label = it
                        .get("label")
                        .and_then(|l| l.as_str())
                        .unwrap_or(model_label);
                    Some(json!({"type": "entity", "entity": name, "label": label}))
                }
                Some("link") => {
                    let url = it.get("url").and_then(|u| u.as_str())?;
                    if !(url.starts_with("http://") || url.starts_with("https://")) {
                        return None; // only external http(s) links are permitted
                    }
                    Some(json!({
                        "type": "link",
                        "url": url,
                        "label": it.get("label").and_then(|l| l.as_str()).unwrap_or(url),
                    }))
                }
                _ => None,
            })
            .collect(),
        None => readable
            .into_iter()
            .map(|(name, label)| json!({"type": "entity", "entity": name, "label": label}))
            .collect(),
    };
    Ok(Json(json!({"items": items})))
}

/// `POST /api/navigation` — replace the default navigation item list.
async fn upsert_navigation(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<UpsertUiBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let items = body.items.unwrap_or_else(|| json!([]));
    if !items.is_array() {
        return Err(Error::Invalid("items must be an array".into()).into());
    }
    for it in items.as_array().unwrap() {
        match it.get("type").and_then(|t| t.as_str()) {
            Some("entity") => {
                // trim + empty check: an empty-string entity passes a bare
                // `is_none()` check, gets stored, then silently vanishes from
                // every menu (it is never readable).
                if it
                    .get("entity")
                    .and_then(|e| e.as_str())
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
                {
                    return Err(Error::Invalid("entity nav item needs an entity".into()).into());
                }
            }
            Some("link") => {
                let url = it.get("url").and_then(|u| u.as_str()).unwrap_or_default();
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(Error::Invalid("link nav item needs an http(s) url".into()).into());
                }
            }
            other => {
                return Err(Error::Invalid(format!(
                    "unknown nav item type {other:?} (supported: entity, link)"
                ))
                .into())
            }
        }
    }
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    sqlx::query(
        "INSERT INTO meta.md_navigation (tenant_id, name, label, items) VALUES ($1, 'default', $2, $3) \
         ON CONFLICT (tenant_id, name) DO UPDATE \
            SET items = EXCLUDED.items, label = EXCLUDED.label, updated_at = now()",
    )
    .bind(user.tenant_id)
    .bind(&body.label)
    .bind(&items)
    .execute(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"name": "default", "items": items})),
    ))
}

async fn delete_navigation(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let res = sqlx::query("DELETE FROM meta.md_navigation WHERE tenant_id = $1 AND name = $2")
        .bind(user.tenant_id)
        .bind(&name)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    if res.rows_affected() == 0 {
        return Err(Error::NotFound(format!("navigation {name}")).into());
    }
    Ok(StatusCode::NO_CONTENT)
}

// ===== helpers =====

/// A row of an entity-scoped UI table (md_form / md_view), as stored JSON.
async fn load_ui_row(
    st: &AppState,
    tenant: Uuid,
    table: &str,
    entity: &str,
    name: &str,
) -> ApiResult<Option<Value>> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    let row: Option<Value> = sqlx::query_scalar(&format!(
        "SELECT to_jsonb(t) FROM {table} t WHERE t.entity = $1 AND t.name = $2 AND t.active"
    ))
    .bind(entity)
    .bind(name)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(row)
}

/// Upsert a one-JSONB-column UI row (md_form.layout).
#[allow(clippy::too_many_arguments)]
async fn upsert_ui_row(
    st: &AppState,
    tenant: Uuid,
    table: &str,
    entity: &str,
    name: &str,
    label: Option<&str>,
    json_col: &str,
    value: &Value,
) -> ApiResult<()> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    sqlx::query(&format!(
        "INSERT INTO {table} (tenant_id, entity, name, label, {json_col}) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (tenant_id, entity, name) DO UPDATE \
            SET label = EXCLUDED.label, {json_col} = EXCLUDED.{json_col}, updated_at = now()"
    ))
    .bind(tenant)
    .bind(entity)
    .bind(name)
    .bind(label)
    .bind(value)
    .execute(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(())
}

/// Upsert a multi-JSONB-column UI row (md_view).
#[allow(clippy::too_many_arguments)]
async fn upsert_ui_row_multi(
    st: &AppState,
    tenant: Uuid,
    table: &str,
    entity: &str,
    name: &str,
    label: Option<&str>,
    cols: &[(&str, &Value)],
    page_size: Option<i64>,
) -> ApiResult<()> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    let sql = format!(
        "INSERT INTO {table} (tenant_id, entity, name, label, {cols}, page_size) \
         VALUES ($1, $2, $3, $4, {ph}, ${ps}) \
         ON CONFLICT (tenant_id, entity, name) DO UPDATE \
            SET label = EXCLUDED.label, {sets}, page_size = EXCLUDED.page_size, updated_at = now()",
        cols = cols.iter().map(|(c, _)| *c).collect::<Vec<_>>().join(", "),
        ph = (5..5 + cols.len())
            .map(|i| format!("${i}"))
            .collect::<Vec<_>>()
            .join(", "),
        sets = cols
            .iter()
            .map(|(c, _)| format!("{c} = EXCLUDED.{c}"))
            .collect::<Vec<_>>()
            .join(", "),
        ps = 5 + cols.len(),
    );
    let mut q = sqlx::query(&sql)
        .bind(tenant)
        .bind(entity)
        .bind(name)
        .bind(label);
    for (_, v) in cols {
        q = q.bind(v);
    }
    q = q.bind(page_size);
    q.execute(&mut *tx).await.map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(())
}

async fn delete_ui_row(
    st: &AppState,
    tenant: Uuid,
    table: &str,
    entity: &str,
    name: &str,
) -> ApiResult<()> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    let res = sqlx::query(&format!(
        "DELETE FROM {table} WHERE tenant_id = $1 AND entity = $2 AND name = $3"
    ))
    .bind(tenant)
    .bind(entity)
    .bind(name)
    .execute(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    if res.rows_affected() == 0 {
        return Err(Error::NotFound(format!("{table} {entity}/{name}")).into());
    }
    Ok(())
}

/// The default form fields: every declared field in definition order, with the
/// widget inferred from the field type. FK (relationship) fields are appended
/// as reference widgets.
fn default_fields(def: &mda_meta::EntityDefinition) -> Vec<Value> {
    let mut out: Vec<Value> = def
        .fields
        .iter()
        .map(|f| {
            json!({
                "name": f.name,
                "label": f.label.clone().unwrap_or_else(|| f.name.clone()),
                "type": f.field_type,
                "required": f.required,
                "widget": infer_widget(&f.field_type),
                "options": f.config.get("options").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    for r in &def.relationships {
        out.push(json!({
            "name": r.source_field_name,
            "label": r.source_field_name,
            "type": "reference",
            "required": r.required,
            "widget": "reference",
            "options": Value::Null,
        }));
    }
    out
}

fn default_columns(def: &mda_meta::EntityDefinition) -> Vec<Value> {
    def.fields
        .iter()
        .take(5)
        .map(|f| {
            json!({
                "field": f.name,
                "label": f.label.clone().unwrap_or_else(|| f.name.clone()),
                "type": f.field_type,
            })
        })
        .collect()
}

/// Resolve one authored field spec against the model + the caller's FLS.
/// Returns None when the field no longer exists or the caller cannot read it.
fn resolve_field_spec(
    user: &Identity,
    entity: &str,
    def: &mda_meta::EntityDefinition,
    name: &str,
    widget: Option<&Value>,
) -> Option<Value> {
    if let Some(r) = def
        .relationships
        .iter()
        .find(|r| r.source_field_name == name)
    {
        // FK column: readable unless explicitly denied
        if user.field_access(entity, name) == Access::None {
            return None;
        }
        return Some(json!({
            "name": name,
            "label": name,
            "type": "reference",
            "required": r.required,
            "widget": widget.and_then(|w| w.as_str()).unwrap_or("reference"),
            "options": Value::Null,
        }));
    }
    let f = def.fields.iter().find(|f| f.name == name)?;
    if user.field_access(entity, name) == Access::None {
        return None;
    }
    let mut spec = json!({
        "name": name,
        "label": f.label.clone().unwrap_or_else(|| name.to_string()),
        "type": f.field_type,
        "required": f.required,
        "widget": widget
            .and_then(|w| w.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| infer_widget(&f.field_type).to_string()),
        "options": f.config.get("options").cloned().unwrap_or(Value::Null),
    });
    if let Some(cfg) = f.config.as_object() {
        // pass through harmless authoring hints (currency code, precision…)
        for k in ["currency", "precision", "scale", "target_entity"] {
            if let Some(v) = cfg.get(k) {
                spec[k] = v.clone();
            }
        }
    }
    Some(spec)
}

fn infer_widget(field_type: &str) -> &'static str {
    match field_type {
        "bool" => "checkbox",
        "enum" => "select",
        "text" => "textarea",
        "date" => "date",
        "datetime" => "datetime",
        "integer" | "decimal" | "money" | "auto_number" => "number",
        "reference" => "reference",
        "attachment" => "attachment",
        _ => "text",
    }
}

/// Authoring validation for form layouts: `sections[].fields[].name` must be a
/// non-empty string (existence is checked at render; this catches shape bugs).
fn validate_layout(layout: &Value) -> ApiResult<()> {
    let Some(sections) = layout.get("sections").and_then(|s| s.as_array()) else {
        return Err(Error::Invalid("layout.sections must be an array".into()).into());
    };
    for sec in sections {
        match sec.get("fields").and_then(|f| f.as_array()) {
            Some(fields) => {
                for f in fields {
                    if f.get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .is_empty()
                    {
                        return Err(Error::Invalid("every layout field needs a name".into()).into());
                    }
                }
            }
            None => return Err(Error::Invalid("layout section needs a fields array".into()).into()),
        }
    }
    Ok(())
}

/// Every entity the caller can read: (name, label), model order.
async fn readable_entities(st: &AppState, user: &Identity) -> ApiResult<Vec<(String, String)>> {
    let model = mda_meta::loader::load_active_model(&st.pool, user.tenant_id).await?;
    Ok(model
        .entities
        .iter()
        .filter(|e| user.can(&e.name, "read"))
        .map(|e| {
            (
                e.name.clone(),
                e.label.clone().unwrap_or_else(|| e.name.clone()),
            )
        })
        .collect())
}

/// Load one saved report (name + dataset) — used by dashboard tiles.
async fn load_report(
    st: &AppState,
    tenant: Uuid,
    id: Uuid,
) -> ApiResult<(String, mda_reports::Dataset)> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    let row: Option<(String, Value)> =
        sqlx::query_as("SELECT name, dataset FROM meta.md_report WHERE tenant_id = $1 AND id = $2")
            .bind(tenant)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    let (name, dataset) = row.ok_or_else(|| Error::NotFound(format!("report {id}")))?;
    let ds: mda_reports::Dataset =
        serde_json::from_value(dataset).map_err(|e| Error::Invalid(format!("bad dataset: {e}")))?;
    Ok((name, ds))
}
