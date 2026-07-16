//! Runtime data API (PLAN §7) with Phase-3 security: object RBAC, field-level
//! projection/rejection, record-level ownership/OWD scope, and audit logging.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use mda_core::Error;
use mda_data::{self, ListParams, RecordScope};
use mda_meta::{loader, EntityDefinition};
use mda_security::{Access, Identity, Owd};
use serde::Deserialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/data/:entity", get(list_records).post(create_record))
        .route(
            "/api/data/:entity/:id",
            get(read_record).patch(update_record).delete(delete_record),
        )
        .route(
            "/api/data/:entity/:id/:transition",
            axum::routing::post(transition_record),
        )
}

#[derive(Deserialize, Default)]
struct ListQuery {
    #[serde(default, deserialize_with = "string_or_seq")]
    filter: Vec<String>,
    #[serde(default, deserialize_with = "string_or_seq")]
    sort: Vec<String>,
    #[serde(default)]
    page: Option<u64>,
    #[serde(default)]
    page_size: Option<u64>,
}

async fn list_records(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(entity): Path<String>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<mda_data::ListResult>> {
    authorize(&user, &entity, "read")?;
    let def = entity_def(&st, user.tenant_id, &entity).await?;
    let scope = scope_for(&st, &user, &entity).await?;
    let params = parse_list_params(q)?;
    let mut res = mda_data::list(&st.pool, user.tenant_id, &def, &params, &scope).await?;
    for item in res.items.iter_mut() {
        *item = project(&user, &entity, &def, item.clone());
    }
    Ok(Json(res))
}

async fn create_record(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(entity): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorize(&user, &entity, "create")?;
    let def = entity_def(&st, user.tenant_id, &entity).await?;
    let mut ctx = into_object(body)?;
    assert_writable(&user, &entity, &def, &ctx)?;
    // Phase 4: fire set-field rules + calculated fields (synchronous, in-write).
    let reg = mda_rules::Registry::new();
    let rules = mda_rules::load_active(&st.pool, user.tenant_id, &entity).await?;
    mda_rules::fire(&rules, "after_create", &mut ctx, &reg)?;
    mda_rules::compute_calculated(&def, &mut ctx, &reg)?;
    let rec = mda_data::create(&st.pool, user.tenant_id, &def, ctx, user.user_id).await?;
    audit(
        &st,
        user.tenant_id,
        user.user_id,
        &entity,
        rec["id"].as_str().unwrap_or("").parse::<Uuid>().ok(),
        "create",
        None,
        Some(rec.clone()),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(project(&user, &entity, &def, rec)),
    ))
}

async fn read_record(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((entity, id)): Path<(String, Uuid)>,
) -> ApiResult<Json<Value>> {
    authorize(&user, &entity, "read")?;
    let def = entity_def(&st, user.tenant_id, &entity).await?;
    let scope = scope_for(&st, &user, &entity).await?;
    let rec = mda_data::read(&st.pool, user.tenant_id, &def, id, &scope).await?;
    Ok(Json(project(&user, &entity, &def, rec)))
}

async fn update_record(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((entity, id)): Path<(String, Uuid)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    authorize(&user, &entity, "update")?;
    let expected = version_from_headers(&headers)?;
    let def = entity_def(&st, user.tenant_id, &entity).await?;
    let mut ctx = into_object(body)?;
    assert_writable(&user, &entity, &def, &ctx)?;
    let scope = scope_for(&st, &user, &entity).await?;
    // before-image for audit + condition context (existing merged with the patch).
    let before = mda_data::read(
        &st.pool,
        user.tenant_id,
        &def,
        id,
        &RecordScope::superuser(user.user_id),
    )
    .await
    .ok();
    if let Some(b) = &before {
        if let Some(obj) = b.as_object() {
            for (k, v) in obj {
                if matches!(
                    k.as_str(),
                    "id" | "version" | "owner_id" | "state" | "created_at" | "updated_at"
                ) {
                    continue;
                }
                ctx.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
    // Phase 4: fire set-field rules + calculated fields.
    let reg = mda_rules::Registry::new();
    let rules = mda_rules::load_active(&st.pool, user.tenant_id, &entity).await?;
    mda_rules::fire(&rules, "after_update", &mut ctx, &reg)?;
    mda_rules::compute_calculated(&def, &mut ctx, &reg)?;
    let after = mda_data::update(
        &st.pool,
        user.tenant_id,
        &def,
        id,
        expected,
        ctx,
        &scope,
        None,
    )
    .await?;
    audit(
        &st,
        user.tenant_id,
        user.user_id,
        &entity,
        Some(id),
        "update",
        before,
        Some(after.clone()),
    )
    .await;
    Ok(Json(project(&user, &entity, &def, after)))
}

async fn delete_record(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((entity, id)): Path<(String, Uuid)>,
) -> ApiResult<Response> {
    authorize(&user, &entity, "delete")?;
    let def = entity_def(&st, user.tenant_id, &entity).await?;
    let scope = scope_for(&st, &user, &entity).await?;
    let before = mda_data::read(
        &st.pool,
        user.tenant_id,
        &def,
        id,
        &RecordScope::superuser(user.user_id),
    )
    .await
    .ok();
    mda_data::delete(&st.pool, user.tenant_id, &def, id, &scope).await?;
    audit(
        &st,
        user.tenant_id,
        user.user_id,
        &entity,
        Some(id),
        "delete",
        before,
        None,
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /api/data/:entity/:id/:transition` — run a workflow transition
/// (PLAN §7). `If-Match` carries the record version (OCC).
async fn transition_record(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((entity, id, transition)): Path<(String, Uuid, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorize(&user, &entity, "update")?;
    let expected = version_from_headers(&headers)?;
    let def = entity_def(&st, user.tenant_id, &entity).await?;
    let scope = scope_for(&st, &user, &entity).await?;
    let after = mda_workflow::run_transition(
        &st.pool,
        user.tenant_id,
        user.user_id,
        &def,
        &entity,
        id,
        &transition,
        expected,
        &scope,
    )
    .await?;
    audit(
        &st,
        user.tenant_id,
        user.user_id,
        &entity,
        Some(id),
        "update",
        None,
        Some(after.clone()),
    )
    .await;
    Ok(Json(project(&user, &entity, &def, after)))
}

// ===== security helpers =====

fn authorize(id: &Identity, entity: &str, verb: &str) -> ApiResult<()> {
    if !id.can(entity, verb) {
        return Err(Error::Forbidden(format!("missing {verb} on {entity}")).into());
    }
    Ok(())
}

fn assert_writable(
    id: &Identity,
    entity: &str,
    def: &EntityDefinition,
    body: &Map<String, Value>,
) -> ApiResult<()> {
    for f in &def.fields {
        if body.contains_key(&f.name) && id.field_access(entity, &f.name) != Access::Write {
            return Err(Error::Forbidden(format!("no write access to {}", f.name)).into());
        }
    }
    Ok(())
}

/// Drop fields the caller may not read (FLS read projection).
fn project(id: &Identity, entity: &str, def: &EntityDefinition, mut rec: Value) -> Value {
    if let Some(obj) = rec.as_object_mut() {
        for f in &def.fields {
            if id.field_access(entity, &f.name) == Access::None {
                obj.remove(&f.name);
            }
        }
    }
    rec
}

async fn scope_for(st: &AppState, user: &Identity, entity: &str) -> ApiResult<RecordScope> {
    let owd: Owd = mda_security::resolve_owd(&st.pool, user.tenant_id, entity).await?;
    Ok(RecordScope {
        user_id: user.user_id,
        public_read: owd.allows_read_for_all(),
        public_write: owd.allows_write_for_all(),
        bypass: user.is_superuser,
    })
}

async fn entity_def(
    st: &AppState,
    tenant: Uuid,
    name: &str,
) -> ApiResult<std::sync::Arc<EntityDefinition>> {
    let id = loader::entity_id_by_name(&st.pool, tenant, name).await?;
    Ok(st.cache.get_entity(&st.pool, tenant, id).await?)
}

#[allow(clippy::too_many_arguments)]
async fn audit(
    st: &AppState,
    tenant: Uuid,
    actor: Uuid,
    entity: &str,
    record_id: Option<Uuid>,
    op: &str,
    before: Option<Value>,
    after: Option<Value>,
) {
    if let Err(e) = sqlx::query(
        "INSERT INTO sys_audit_log (tenant_id, actor_id, entity, record_id, op, before, after)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(tenant)
    .bind(actor)
    .bind(entity)
    .bind(record_id.unwrap_or_else(Uuid::nil))
    .bind(op)
    .bind(before)
    .bind(after)
    .execute(&st.pool)
    .await
    {
        tracing::error!(?e, "audit log insert failed");
    }
}

// ===== request parsing helpers =====

fn into_object(v: Value) -> ApiResult<Map<String, Value>> {
    match v {
        Value::Object(m) => Ok(m),
        _ => Err(Error::Invalid("request body must be a JSON object".into()).into()),
    }
}

fn version_from_headers(headers: &HeaderMap) -> ApiResult<i64> {
    headers
        .get("if-match")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.trim_matches('"').parse::<i64>().ok())
        .ok_or_else(|| Error::Invalid("If-Match version header required".into()).into())
}

fn parse_list_params(q: ListQuery) -> ApiResult<ListParams> {
    let mut filters = Vec::new();
    for f in q.filter {
        let mut parts = f.splitn(3, ':');
        let field = parts.next().unwrap_or("").trim().to_string();
        let op = parts.next().unwrap_or("").trim().to_string();
        let value = parts.next().unwrap_or("").to_string();
        if field.is_empty() || op.is_empty() {
            return Err(Error::Invalid(format!("bad filter: {f}")).into());
        }
        filters.push(mda_data::Filter { field, op, value });
    }
    let mut sort = Vec::new();
    for s in q.sort {
        let s = s.trim();
        if let Some(stripped) = s.strip_prefix('-') {
            sort.push(mda_data::Sort {
                field: stripped.to_string(),
                asc: false,
            });
        } else {
            sort.push(mda_data::Sort {
                field: s.to_string(),
                asc: true,
            });
        }
    }
    Ok(ListParams {
        filters,
        sort,
        page: q.page.unwrap_or(1),
        page_size: q.page_size.unwrap_or(0),
    })
}

/// Accept a single value or a repeated sequence (serde_urlencoded quirk).
fn string_or_seq<'de, D>(de: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Visitor;
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or a sequence of strings")
        }
        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(vec![v.to_string()])
        }
        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut out = Vec::new();
            while let Some(s) = seq.next_element::<String>()? {
                out.push(s);
            }
            Ok(out)
        }
    }
    de.deserialize_any(V)
}
