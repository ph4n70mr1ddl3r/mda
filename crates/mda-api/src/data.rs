//! Runtime data API (PLAN §7) with Phase-3 security: object RBAC, field-level
//! projection/rejection, record-level ownership/OWD scope, and audit logging.

use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use mda_core::Error;
use mda_data::{self, Filter, ListParams, RecordScope};
use mda_meta::{loader, EntityDefinition};
use mda_security::{Access, Identity, Owd};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

/// Counter for failed audit-log writes. Surfaces as a metric in the health
/// endpoint and can be scraped for alerting (audit integrity is a compliance
/// requirement).
static AUDIT_WRITE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Hard cap on rows produced by one CSV/JSON export (bounded memory + response;
/// larger exports belong in the async job path, §5.13).
const EXPORT_MAX_ROWS: u64 = 100_000;

/// Snapshot the current audit-failure counter (for the health endpoint).
pub fn audit_failure_count() -> u64 {
    AUDIT_WRITE_FAILURES.load(Ordering::Relaxed)
}

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
        .route(
            "/api/data/:entity/:id/restore",
            axum::routing::post(restore_record),
        )
        .route(
            "/api/data/:entity/mass-update",
            axum::routing::post(mass_update_record),
        )
        .route(
            "/api/data/:entity/mass-delete",
            axum::routing::post(mass_delete_record),
        )
        .route(
            "/api/impex/:entity/import",
            axum::routing::post(import_records),
        )
        .route("/api/impex/:entity/export", get(export_records))
        .route(
            "/api/shares/:entity/:id",
            axum::routing::post(create_share).get(list_shares),
        )
        .route(
            "/api/shares/:entity/:id/:principal_id",
            axum::routing::delete(delete_share),
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
    let rec = create_record_service(&st, &user, &entity, body).await?;
    Ok((StatusCode::CREATED, Json(rec)))
}

/// Shared create write-path used by both REST and GraphQL (ADR-0010 mutations
/// reach REST parity by construction): RBAC + FLS write-check + rules +
/// calculated fields + audit, then FLS read-projection of the result.
pub(crate) async fn create_record_service(
    st: &AppState,
    user: &Identity,
    entity: &str,
    body: Value,
) -> ApiResult<Value> {
    authorize(user, entity, "create")?;
    let def = entity_def(st, user.tenant_id, entity).await?;
    let mut ctx = into_object(body)?;
    assert_writable(user, entity, &def, &ctx)?;
    // Phase 4: fire set-field rules + calculated fields (synchronous, in-write).
    let reg = mda_rules::Registry::new();
    let rules = mda_rules::load_active(&st.pool, user.tenant_id, entity).await?;
    mda_rules::fire(&rules, "after_create", &mut ctx, &reg)?;
    mda_rules::compute_calculated(&def, &mut ctx, &reg)?;
    let rec = mda_data::create(&st.pool, user.tenant_id, &def, ctx, user.user_id).await?;
    audit(
        st,
        user.tenant_id,
        user.user_id,
        entity,
        rec["id"].as_str().unwrap_or("").parse::<Uuid>().ok(),
        "create",
        None,
        Some(rec.clone()),
    )
    .await;
    Ok(project(user, entity, &def, rec))
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
    let expected = version_from_headers(&headers)?;
    let rec = update_record_service(&st, &user, &entity, id, expected, body).await?;
    Ok(Json(rec))
}

/// Shared update write-path (REST + GraphQL). Carries the OCC `version`, merges
/// the before-image for rule/condition context, fires rules + calculated fields,
/// and audits before/after — identical to a hand-typed REST PATCH.
pub(crate) async fn update_record_service(
    st: &AppState,
    user: &Identity,
    entity: &str,
    id: Uuid,
    expected_version: i64,
    body: Value,
) -> ApiResult<Value> {
    authorize(user, entity, "update")?;
    let def = entity_def(st, user.tenant_id, entity).await?;
    let mut ctx = into_object(body)?;
    assert_writable(user, entity, &def, &ctx)?;
    let scope = scope_for(st, user, entity).await?;
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
    let rules = mda_rules::load_active(&st.pool, user.tenant_id, entity).await?;
    mda_rules::fire(&rules, "after_update", &mut ctx, &reg)?;
    mda_rules::compute_calculated(&def, &mut ctx, &reg)?;
    let after = mda_data::update(
        &st.pool,
        user.tenant_id,
        &def,
        id,
        expected_version,
        ctx,
        &scope,
        None,
    )
    .await?;
    audit(
        st,
        user.tenant_id,
        user.user_id,
        entity,
        Some(id),
        "update",
        before,
        Some(after.clone()),
    )
    .await;
    Ok(project(user, entity, &def, after))
}

// ===== restore from archive (ADR-0006 / ADR-0015) =====

/// `POST /api/data/:entity/:id/restore` — re-insert the most recently archived
/// copy of a hard-deleted record (admin undo). Single-record scope; batch /
/// cascade restore is the full ADR-0015 design and a follow-up. The caller's
/// record-level write scope is enforced against the archived row — the entity
/// `create` verb alone must not resurrect a record the caller could never
/// write (e.g. a private-OWD record owned by someone else).
async fn restore_record(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((entity, id)): Path<(String, Uuid)>,
) -> ApiResult<Json<Value>> {
    authorize(&user, &entity, "create")?;
    let def = entity_def(&st, user.tenant_id, &entity).await?;
    let scope = scope_for(&st, &user, &entity).await?;
    let rec = mda_data::restore(&st.pool, user.tenant_id, &def, id, &scope).await?;
    audit(
        &st,
        user.tenant_id,
        user.user_id,
        &entity,
        Some(id),
        "create",
        None,
        Some(rec.clone()),
    )
    .await;
    Ok(Json(project(&user, &entity, &def, rec)))
}

async fn delete_record(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((entity, id)): Path<(String, Uuid)>,
) -> ApiResult<Response> {
    delete_record_service(&st, &user, &entity, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Shared delete write-path (REST + GraphQL): RBAC + record-scope delete + audit
/// before-image. Returns the before-image so a GraphQL mutation can echo it back.
pub(crate) async fn delete_record_service(
    st: &AppState,
    user: &Identity,
    entity: &str,
    id: Uuid,
) -> ApiResult<()> {
    authorize(user, entity, "delete")?;
    let def = entity_def(st, user.tenant_id, entity).await?;
    let scope = scope_for(st, user, entity).await?;
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
        st,
        user.tenant_id,
        user.user_id,
        entity,
        Some(id),
        "delete",
        before,
        None,
    )
    .await;
    Ok(())
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

// ===== mass actions (PLAN §9 deferral) =====
//
// Bulk update / delete *by filter* — distinct from the §5.13 file import
// (which is row-by-row from a file). Mass actions reuse the single-record
// write pipeline *per affected record* (RBAC + FLS write-check + rules +
// calculated fields + OCC + audit + event log), so a mass update is
// indistinguishable from N hand-typed PATCHes and respects record-level
// security on every row. A hard cap bounds the blast radius; `dry_run`
// returns the candidate id set without mutating. (Interacts with cascade
// ADR-0006 and sharing recompute ADR-0013 by construction, since each row
// goes through the normal delete/update path.)

/// Maximum number of records one mass action may touch. Bounds the blast
/// radius of a broad filter and the per-record write-loop cost.
const MAX_MASS_BATCH: u64 = 5000;

#[derive(serde::Deserialize)]
struct MassUpdateBody {
    #[serde(default, deserialize_with = "string_or_seq")]
    filter: Vec<String>,
    /// The field patch to apply (same shape as a single-record PATCH body).
    #[serde(rename = "set")]
    patch: Value,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    limit: Option<u64>,
}

#[derive(serde::Deserialize)]
struct MassDeleteBody {
    #[serde(default, deserialize_with = "string_or_seq")]
    filter: Vec<String>,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    limit: Option<u64>,
}

/// Resolve the candidate record ids for a mass action (write-scoped, capped).
/// Shares the filter grammar + write predicate with single-record writes.
async fn mass_targets(
    st: &AppState,
    user: &Identity,
    entity: &str,
    filters: &[String],
    limit: Option<u64>,
) -> ApiResult<Vec<Uuid>> {
    let def = entity_def(st, user.tenant_id, entity).await?;
    let scope = scope_for(st, user, entity).await?;
    let params = ListParams {
        filters: filters_from_strings(filters)?,
        sort: Vec::new(),
        page: 1,
        page_size: 0,
    };
    let cap = limit.unwrap_or(MAX_MASS_BATCH).clamp(1, MAX_MASS_BATCH);
    Ok(mda_data::mass_target_ids(&st.pool, user.tenant_id, &def, &params, &scope, cap).await?)
}

/// `POST /api/data/:entity/mass-update` — apply a patch to every record matching
/// the filter that the caller may write. Returns `{ affected, ids, errors[], dry_run }`.
async fn mass_update_record(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(entity): Path<String>,
    Json(body): Json<MassUpdateBody>,
) -> ApiResult<Json<Value>> {
    authorize(&user, &entity, "update")?;
    // FLS write-check on the patch UPFRONT: a mass update touching a field the
    // caller may not write is rejected before any record is resolved/touched —
    // identical to a single-record PATCH (which checks the same thing first).
    let patch = into_object(body.patch)?;
    let def = entity_def(&st, user.tenant_id, &entity).await?;
    assert_writable(&user, &entity, &def, &patch)?;
    let ids = mass_targets(&st, &user, &entity, &body.filter, body.limit).await?;
    if body.dry_run {
        return Ok(Json(json!({
            "dry_run": true,
            "affected": ids.len(),
            "ids": ids,
            "errors": Vec::<Value>::new(),
        })));
    }
    let mut affected: u64 = 0;
    let mut errors: Vec<Value> = Vec::new();
    let mut done: Vec<Uuid> = Vec::with_capacity(ids.len());
    for id in ids {
        // Read the current version (superuser) so OCC still holds per record:
        // a row changed between target-resolution and the write is skipped with
        // a conflict rather than clobbered.
        let def = entity_def(&st, user.tenant_id, &entity).await?;
        let before = mda_data::read(
            &st.pool,
            user.tenant_id,
            &def,
            id,
            &RecordScope::superuser(user.user_id),
        )
        .await
        .ok();
        let version = before.as_ref().and_then(version_of).unwrap_or(0);
        match update_record_service(
            &st,
            &user,
            &entity,
            id,
            version,
            Value::Object(patch.clone()),
        )
        .await
        {
            Ok(_) => {
                affected += 1;
                done.push(id);
            }
            Err(e) => {
                errors.push(json!({ "id": id, "error": e.0.to_string(), "code": e.0.code() }))
            }
        }
    }
    Ok(Json(json!({
        "dry_run": false,
        "affected": affected,
        "ids": done,
        "errors": errors,
    })))
}

/// `POST /api/data/:entity/mass-delete` — delete every record matching the filter
/// that the caller may delete. Returns `{ affected, ids[], dry_run }`.
async fn mass_delete_record(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(entity): Path<String>,
    Json(body): Json<MassDeleteBody>,
) -> ApiResult<Json<Value>> {
    authorize(&user, &entity, "delete")?;
    let ids = mass_targets(&st, &user, &entity, &body.filter, body.limit).await?;
    if body.dry_run {
        return Ok(Json(json!({
            "dry_run": true,
            "affected": ids.len(),
            "ids": ids,
            "errors": Vec::<Value>::new(),
        })));
    }
    let mut affected: u64 = 0;
    let mut errors: Vec<Value> = Vec::new();
    let mut done: Vec<Uuid> = Vec::with_capacity(ids.len());
    for id in ids {
        match delete_record_service(&st, &user, &entity, id).await {
            Ok(_) => {
                affected += 1;
                done.push(id);
            }
            Err(e) => {
                errors.push(json!({ "id": id, "error": e.0.to_string(), "code": e.0.code() }))
            }
        }
    }
    Ok(Json(json!({
        "dry_run": false,
        "affected": affected,
        "ids": done,
        "errors": errors,
    })))
}

// ===== security helpers =====

pub(crate) fn authorize(id: &Identity, entity: &str, verb: &str) -> ApiResult<()> {
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
pub(crate) fn project(
    id: &Identity,
    entity: &str,
    def: &EntityDefinition,
    mut rec: Value,
) -> Value {
    if let Some(obj) = rec.as_object_mut() {
        for f in &def.fields {
            if id.field_access(entity, &f.name) == Access::None {
                obj.remove(&f.name);
            }
        }
    }
    rec
}

pub(crate) async fn scope_for(
    st: &AppState,
    user: &Identity,
    entity: &str,
) -> ApiResult<RecordScope> {
    let owd: Owd = mda_security::resolve_owd(&st.pool, user.tenant_id, entity).await?;
    Ok(RecordScope {
        user_id: user.user_id,
        public_read: owd.allows_read_for_all(),
        public_write: owd.allows_write_for_all(),
        bypass: user.is_superuser,
        team_owd: owd == Owd::Team,
        team_id: user.team_id,
    })
}

pub(crate) async fn entity_def(
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
    // Both sys_audit_log and sys_event_log are tenant-RLS-gated, so the inserts
    // must run under the tenant GUC — otherwise a non-superuser app role (the
    // normal case) fails the WITH CHECK and the row is dropped. They share one
    // short transaction so the two side-effects land together (best-effort).
    let (etype, payload) = event_for(op, &before, &after);
    match st.pool.begin().await {
        Ok(mut tx) => {
            if let Err(e) = mda_security::set_tenant(&mut tx, tenant).await {
                tracing::warn!(?e, "audit: set_tenant failed");
                return;
            }
            let audit_ok = sqlx::query(
                "INSERT INTO sys_audit_log (tenant_id, actor_id, entity, record_id, op, before, after)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(tenant)
            .bind(actor)
            .bind(entity)
            .bind(record_id.unwrap_or_else(Uuid::nil))
            .bind(op)
            .bind(&before)
            .bind(&after)
            .execute(&mut *tx)
            .await
            .is_ok();
            let event_ok = if let (Some(etype), Some(rid)) = (etype, record_id) {
                sqlx::query(
                    "INSERT INTO sys_event_log (tenant_id, type, entity, record_id, actor_id, payload)
                     VALUES ($1, $2, $3, $4, $5, $6)",
                )
                .bind(tenant)
                .bind(etype)
                .bind(entity)
                .bind(rid)
                .bind(actor)
                .bind(&payload)
                .execute(&mut *tx)
                .await
                .is_ok()
            } else {
                true
            };
            if audit_ok {
                // Commit even when only the event-log insert failed: the audit
                // row is the integrity record and must not vanish with it —
                // the degraded event path is counted + logged instead.
                if let Err(e) = tx.commit().await {
                    tracing::warn!(?e, "audit tx commit failed");
                }
                if !event_ok {
                    let n = AUDIT_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
                    tracing::error!(
                        failures = n,
                        "event log insert failed (audit row committed)"
                    );
                }
            } else {
                let n = AUDIT_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::error!(failures = n, "audit log insert failed");
            }
        }
        Err(e) => tracing::warn!(?e, "audit: begin tx failed"),
    }
}

/// Map an audit op to a domain event type + a conservative payload. The payload
/// carries only field *names* that changed (never values) so the SSE relay can
/// notify a viewer that a record changed without leaking field-level data (the
/// relay additionally AuthZ-filters; §5.10.6 full field filtering is a follow-up).
fn event_for(
    op: &str,
    before: &Option<Value>,
    after: &Option<Value>,
) -> (Option<&'static str>, Value) {
    let changed: Vec<String> = match (before, after) {
        (Some(Value::Object(b)), Some(Value::Object(a))) => {
            let core: &[&str] = &["id", "version", "updated_at", "created_at"];
            a.iter()
                .filter(|(k, v)| !core.contains(&k.as_str()) && b.get(*k) != Some(*v))
                .map(|(k, _)| k.clone())
                .collect()
        }
        (_, Some(Value::Object(a))) => {
            let core: &[&str] = &["id", "version", "updated_at", "created_at"];
            a.keys()
                .filter(|k| !core.contains(&k.as_str()))
                .cloned()
                .collect()
        }
        _ => Vec::new(),
    };
    let (etype, from_v, to_v) = match op {
        "create" => (
            Some("record.created"),
            None,
            after.as_ref().and_then(version_of),
        ),
        "update" => (
            Some("record.updated"),
            before.as_ref().and_then(version_of),
            after.as_ref().and_then(version_of),
        ),
        "delete" => (
            Some("record.deleted"),
            before.as_ref().and_then(version_of),
            None,
        ),
        _ => (None, None, None),
    };
    let payload = json!({
        "changed_fields": changed,
        "from_version": from_v,
        "to_version": to_v,
    });
    (etype, payload)
}

fn version_of(v: &Value) -> Option<i64> {
    v.get("version").and_then(|x| x.as_i64())
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
    let filters = filters_from_strings(&q.filter)?;
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

/// Parse the shared `field:op:value` filter strings into [`mda_data::Filter`]s.
/// Used by both the list query param and the mass-action request body so the
/// filter grammar is identical everywhere (PLAN §7).
fn filters_from_strings(fs: &[String]) -> ApiResult<Vec<mda_data::Filter>> {
    let mut filters = Vec::new();
    for f in fs {
        let mut parts = f.splitn(3, ':');
        let field = parts.next().unwrap_or("").trim().to_string();
        let op = parts.next().unwrap_or("").trim().to_string();
        let value = parts.next().unwrap_or("").to_string();
        if field.is_empty() || op.is_empty() {
            return Err(Error::Invalid(format!("bad filter: {f}")).into());
        }
        filters.push(mda_data::Filter { field, op, value });
    }
    Ok(filters)
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

// ===== bulk import / export (PLAN §5.13) =====
//
// The synchronous impex contract: an import is *batched, mapped writes* that
// reuse the runtime write pipeline, so an imported row is indistinguishable
// from one typed by hand (no second set of rules to drift). Supports CSV
// (symmetric with the CSV export) and JSON; `mode` (create|update|upsert) with a
// `key` field for update/upsert matching; `dry_run` (validate only, nothing
// written); and `on_error` (abort = all-or-nothing validate-then-commit,
// continue = best-effort per row). Large/streaming imports as an async job
// (sys_impex_job) are the documented follow-up; this is the v1 synchronous
// surface the plan scopes for the runtime API.

#[derive(Deserialize, Default)]
struct ImportQuery {
    /// `csv` | `json` — overrides Content-Type sniffing.
    #[serde(default)]
    format: Option<String>,
    /// `create` | `update` | `upsert` (default create).
    #[serde(default)]
    mode: Option<String>,
    /// Key field for update/upsert matching (a known field, or `id`).
    #[serde(default)]
    key: Option<String>,
    /// Validate only — nothing is written; returns the would-create/would-update
    /// counts + per-row errors.
    #[serde(default)]
    dry_run: Option<bool>,
    /// `abort` (validate-then-commit: any error writes nothing) | `continue`
    /// (best-effort per row; default).
    #[serde(default)]
    on_error: Option<String>,
}

/// `POST /api/impex/:entity/import` — mapped, validated, safe record import
/// (CSV or JSON), reusing the runtime write pipeline per row.
async fn import_records(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(entity): Path<String>,
    headers: HeaderMap,
    Query(q): Query<ImportQuery>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let mode = q.mode.as_deref().unwrap_or("create").to_string();
    if !matches!(mode.as_str(), "create" | "update" | "upsert") {
        return Err(Error::Invalid("mode must be one of create|update|upsert".into()).into());
    }
    let on_error = q.on_error.as_deref().unwrap_or("continue").to_string();
    if !matches!(on_error.as_str(), "abort" | "continue") {
        return Err(Error::Invalid("on_error must be abort|continue".into()).into());
    }
    let dry_run = q.dry_run.unwrap_or(false);

    let def = entity_def(&st, user.tenant_id, &entity).await?;

    // Authorize by mode (upsert needs both verbs — it can do either per row).
    match mode.as_str() {
        "create" => authorize(&user, &entity, "create")?,
        "update" => authorize(&user, &entity, "update")?,
        "upsert" => {
            authorize(&user, &entity, "create")?;
            authorize(&user, &entity, "update")?;
        }
        _ => unreachable!(),
    }

    // Key field is required for update/upsert and must name a known column.
    let key = q.key.clone();
    if mode != "create" {
        let k = key
            .as_deref()
            .ok_or_else(|| Error::Invalid("mode update/upsert requires a `key` field".into()))?;
        if !key_is_known(&def, k) {
            return Err(Error::Invalid(format!(
                "key field '{k}' is not a known field of {entity}"
            ))
            .into());
        }
    }

    // ---- parse rows (CSV or JSON) ----
    let format = q.format.as_deref().unwrap_or_else(|| {
        let ct = headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if ct.contains("text/csv") || ct.contains("application/csv") {
            "csv"
        } else {
            "json"
        }
    });
    // Parse into (source columns, rows). For CSV the columns are the header
    // (so an all-blank unmapped column is still caught); for JSON they are the
    // union of every row's keys.
    let (src_cols, mut rows): (Vec<String>, Vec<Map<String, Value>>) = match format {
        "csv" => {
            let text = std::str::from_utf8(&body)
                .map_err(|_| Error::Invalid("CSV body is not valid UTF-8".into()))?;
            let res = mda_reports::from_csv(text)?;
            (res.columns, res.rows)
        }
        "json" => {
            let v: Vec<Value> = serde_json::from_slice(&body).map_err(|e| {
                Error::Invalid(format!("JSON body must be an array of objects: {e}"))
            })?;
            let rows: Vec<Map<String, Value>> = v
                .into_iter()
                .map(into_object)
                .collect::<ApiResult<Vec<_>>>()?;
            let mut cols = std::collections::HashSet::new();
            for r in &rows {
                for k in r.keys() {
                    cols.insert(k.clone());
                }
            }
            (cols.into_iter().collect(), rows)
        }
        other => return Err(Error::Invalid(format!("unknown format '{other}'")).into()),
    };

    // ---- column mapping: every source column must map to a known field or a
    // recognized system column (id/owner_id/state/...). Unknowns abort up front
    // (§5.13 “Map source columns → entity fields”). System columns are stripped
    // before the write so validate_record never sees them. ----
    let unmapped: Vec<String> = src_cols
        .iter()
        .filter(|c| !is_mappable_column(&def, c))
        .cloned()
        .collect();
    if !unmapped.is_empty() {
        return Err(Error::Invalid(format!(
            "unmapped source columns (no matching field): {}",
            unmapped.join(", ")
        ))
        .into());
    }

    let write_scope = scope_for(&st, &user, &entity).await?;

    // For CSV, a blank cell means "not provided" (absent), matching JSON
    // semantics — so a required field left blank fails required instead of being
    // silently accepted as an empty string. (JSON rows already express absence
    // by omitting the key.)
    if format == "csv" {
        for m in rows.iter_mut() {
            m.retain(|_, v| !matches!(v, Value::String(s) if s.is_empty()));
        }
    }

    // ---- validation pass: per-row field-write AuthZ + record validation +
    // (update/upsert) key resolution. Nothing is written here. ----
    struct Planned {
        index: usize,
        body: Map<String, Value>,    // system columns stripped
        target: Option<(Uuid, i64)>, // Some for an existing record (update)
        errors: Vec<Value>,
    }
    let mut planned: Vec<Planned> = Vec::with_capacity(rows.len());
    for (i, raw) in rows.into_iter().enumerate() {
        let mut errors: Vec<Value> = Vec::new();

        // Field-level write AuthZ (a row may not write a field the caller can't).
        if let Err(e) = assert_writable(&user, &entity, &def, &raw) {
            errors.push(json!({"row": i, "error": e.to_string()}));
        }

        let body = strip_system_columns(&raw);

        // Declarative validation (type / required / unknown) — same check the
        // write path runs, so dry-run reflects what commit would reject.
        if let Err(e) = mda_data::crud::validate_record(&def, &body) {
            errors.push(json!({"row": i, "error": e.to_string()}));
        }

        // Resolve the target record for update/upsert (write-scoped: a user can
        // only import-update a record they may write).
        let target = if mode == "create" {
            None
        } else {
            match resolve_import_target(
                &st.pool,
                &user,
                &def,
                key.as_deref().unwrap(),
                &raw,
                &write_scope,
            )
            .await
            {
                Ok(Some(tv)) => Some(tv),
                Ok(None) => {
                    if mode == "update" {
                        errors.push(json!({
                            "row": i,
                            "error": format!(
                                "no existing record matches key '{}'",
                                key.as_deref().unwrap()
                            )
                        }));
                    }
                    None // upsert → falls through to create
                }
                Err(e) => {
                    errors.push(json!({"row": i, "error": e.to_string()}));
                    None
                }
            }
        };

        planned.push(Planned {
            index: i,
            body,
            target,
            errors,
        });
    }

    let validation_errors: usize = planned.iter().map(|p| p.errors.len()).sum::<usize>();
    let would_create = planned
        .iter()
        .filter(|p| p.target.is_none() && p.errors.is_empty())
        .count();
    let would_update = planned
        .iter()
        .filter(|p| p.target.is_some() && p.errors.is_empty())
        .count();

    let mut errors: Vec<Value> = planned.iter().flat_map(|p| p.errors.clone()).collect();
    let mut created = 0u64;
    let mut updated = 0u64;

    // Commit: never on dry_run; under `abort`, only if the validation pass was
    // fully clean (validate-then-commit ⇒ all-or-nothing).
    let do_write = !dry_run && (on_error == "continue" || validation_errors == 0);
    if do_write {
        for p in &planned {
            if !p.errors.is_empty() {
                continue;
            }
            let res = match p.target {
                Some((id, ver)) => update_record_service(
                    &st,
                    &user,
                    &entity,
                    id,
                    ver,
                    Value::Object(p.body.clone()),
                )
                .await
                .map(|_| ()),
                None => create_record_service(&st, &user, &entity, Value::Object(p.body.clone()))
                    .await
                    .map(|_| ()),
            };
            match res {
                Ok(()) => {
                    if p.target.is_some() {
                        updated += 1;
                    } else {
                        created += 1;
                    }
                }
                Err(e) => errors.push(json!({"row": p.index, "error": e.to_string()})),
            }
        }
    }

    Ok(Json(json!({
        "mode": mode,
        "format": format,
        "dry_run": dry_run,
        "on_error": on_error,
        "created": created,
        "updated": updated,
        "imported": created + updated,
        "would_create": would_create,
        "would_update": would_update,
        "errors": errors,
    })))
}

/// Is `k` a known field, relationship, or the record id? (Eligible as an
/// import key.)
fn key_is_known(def: &EntityDefinition, k: &str) -> bool {
    k == "id"
        || def.fields.iter().any(|f| f.name == k)
        || def.relationships.iter().any(|r| r.source_field_name == k)
}

/// Is `k` a mappable source column? (A known field/relationship, or a
/// recognized system column that is silently ignored on write.)
fn is_mappable_column(def: &EntityDefinition, k: &str) -> bool {
    key_is_known(def, k)
        || matches!(
            k,
            "owner_id" | "state" | "version" | "created_at" | "updated_at"
        )
}

/// Remove system columns from a row so the write pipeline (which validates
/// against the field set) never sees them. The key field, if it is a real
/// field, is retained.
fn strip_system_columns(m: &Map<String, Value>) -> Map<String, Value> {
    m.iter()
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "id" | "owner_id" | "state" | "version" | "created_at" | "updated_at"
            )
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Resolve an existing record by the import key under the caller's **write**
/// scope. Returns `Ok(Some((id, version)))` when a unique match exists,
/// `Ok(None)` when none does (update ⇒ row error, upsert ⇒ create), and an
/// error on an ambiguous match or a malformed key value.
async fn resolve_import_target(
    pool: &sqlx::PgPool,
    user: &Identity,
    def: &EntityDefinition,
    key: &str,
    raw: &Map<String, Value>,
    write_scope: &RecordScope,
) -> ApiResult<Option<(Uuid, i64)>> {
    let keyval = raw.get(key).and_then(|v| v.as_str()).ok_or_else(|| {
        Error::Invalid(format!(
            "key field '{key}' is missing or not a string in this row"
        ))
    })?;

    if key == "id" {
        let id = Uuid::parse_str(keyval)
            .map_err(|_| Error::Invalid("key 'id' is not a valid UUID".into()))?;
        return match mda_data::read(pool, user.tenant_id, def, id, &write_scope.clone()).await {
            Ok(rec) => {
                let ver = version_of(&rec).unwrap_or(1);
                Ok(Some((id, ver)))
            }
            Err(_) => Ok(None),
        };
    }

    // Write-scoped lookup (same predicate a single-record PATCH enforces).
    let ids = mda_data::mass_target_ids(
        pool,
        user.tenant_id,
        def,
        &ListParams {
            filters: vec![Filter {
                field: key.to_string(),
                op: "eq".to_string(),
                value: keyval.to_string(),
            }],
            sort: Vec::new(),
            page: 1,
            page_size: 0,
        },
        write_scope,
        2, // 2 is enough to detect ambiguity
    )
    .await?;

    match ids.len() {
        0 => Ok(None),
        1 => {
            let id = ids[0];
            let rec = mda_data::read(
                pool,
                user.tenant_id,
                def,
                id,
                &RecordScope::superuser(user.user_id),
            )
            .await?;
            let ver = version_of(&rec).unwrap_or(1);
            Ok(Some((id, ver)))
        }
        _ => Err(Error::Invalid(format!(
            "multiple records match key '{key}' — the key must be unique"
        ))
        .into()),
    }
}

/// `GET /api/impex/:entity/export` — list (filtered) as CSV, respecting field
/// read security. Pages through the full filtered set (the list surface caps
/// page_size at 200 — an export must not silently truncate there), bounded by
/// [`EXPORT_MAX_ROWS`].
async fn export_records(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(entity): Path<String>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Response> {
    authorize(&user, &entity, "read")?;
    let def = entity_def(&st, user.tenant_id, &entity).await?;
    let scope = scope_for(&st, &user, &entity).await?;
    let mut params = parse_list_params(q)?;
    // stable pagination: order by id unless the caller picked a sort
    if params.sort.is_empty() {
        params.sort.push(mda_data::Sort {
            field: "id".to_string(),
            asc: true,
        });
    }
    // export page size: the caller's explicit ask, clamped to the list cap
    // (0 = the cap — an export walks the whole filtered set anyway).
    params.page_size = if params.page_size == 0 {
        mda_data::MAX_PAGE_SIZE
    } else {
        params.page_size.clamp(1, mda_data::MAX_PAGE_SIZE)
    };
    let mut items: Vec<Value> = Vec::new();
    loop {
        let res = mda_data::list(&st.pool, user.tenant_id, &def, &params, &scope).await?;
        let n = res.items.len() as u64;
        items.extend(res.items);
        if n < params.page_size || items.len() as u64 >= EXPORT_MAX_ROWS {
            break;
        }
        params.page += 1;
    }
    items.truncate(EXPORT_MAX_ROWS as usize);
    let res_items = items;
    // columns: id + readable data fields + relationship columns
    let mut columns = vec!["id".to_string()];
    for f in &def.fields {
        if user.field_access(&entity, &f.name) != Access::None {
            columns.push(f.name.clone());
        }
    }
    for r in &def.relationships {
        columns.push(r.source_field_name.clone());
    }
    let rows: Vec<Map<String, Value>> = res_items
        .into_iter()
        .map(|v| project(&user, &entity, &def, v))
        .map(|v| match v {
            Value::Object(m) => m,
            other => {
                let mut m = Map::new();
                m.insert("value".into(), other);
                m
            }
        })
        .collect();
    let body = mda_reports::to_csv(&mda_reports::ReportResult { columns, rows });
    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8")],
        body,
    )
        .into_response())
}

/// `POST /api/shares/:entity/:id` — share a record with a user (manual share).
/// Only the record owner (or superuser) may share.
#[derive(serde::Deserialize)]
struct ShareReq {
    principal_id: Uuid,
    access: String, // read | write
}

/// Only the record owner (or a superuser) may manage a record's shares. Reads
/// the record under a superuser scope so a non-reading owner can still manage
/// their own shares (the AuthZ gate is ownership, not read).
async fn require_owner(st: &AppState, user: &Identity, entity: &str, id: Uuid) -> ApiResult<()> {
    let def = entity_def(st, user.tenant_id, entity).await?;
    let rec = mda_data::read(
        &st.pool,
        user.tenant_id,
        &def,
        id,
        &mda_data::RecordScope::superuser(user.user_id),
    )
    .await?;
    let owner = rec["owner_id"]
        .as_str()
        .and_then(|s| s.parse::<Uuid>().ok());
    if !user.is_superuser && owner != Some(user.user_id) {
        return Err(Error::Forbidden("only the owner can share".into()).into());
    }
    Ok(())
}

async fn create_share(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((entity, id)): Path<(String, Uuid)>,
    Json(req): Json<ShareReq>,
) -> ApiResult<StatusCode> {
    require_owner(&st, &user, &entity, id).await?;
    if !matches!(req.access.as_str(), "read" | "write") {
        return Err(Error::Invalid("access must be 'read' or 'write'".into()).into());
    }
    // sec_user + sec_record_share are both RLS-gated by tenant → run both
    // checks under the tenant GUC in one transaction.
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    // the principal must be an active user in the same tenant (no cross-tenant shares)
    let principal_ok: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM sec.sec_user WHERE id = $1 AND tenant_id = $2 AND active = TRUE",
    )
    .bind(req.principal_id)
    .bind(user.tenant_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    if principal_ok.is_none() {
        return Err(Error::Invalid("principal is not an active user in this tenant".into()).into());
    }
    // A rule-derived share (rule_id IS NOT NULL) must never be overwritten by a
    // manual one: it would keep pointing at the rule (unrevocable here) while
    // carrying the manual access — and the next rule recompute would silently
    // revert it. Surface the collision instead.
    let rule_owned: Option<(Option<Uuid>,)> = sqlx::query_as(
        "SELECT rule_id FROM sec.sec_record_share
          WHERE tenant_id = $1 AND record_id = $2 AND principal_id = $3",
    )
    .bind(user.tenant_id)
    .bind(id)
    .bind(req.principal_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    if let Some((Some(rule_id),)) = rule_owned {
        return Err(Error::Conflict(format!(
            "principal's access is managed by sharing rule {rule_id}; update the rule instead"
        ))
        .into());
    }
    sqlx::query(
        "INSERT INTO sec.sec_record_share (tenant_id, entity, record_id, principal_id, access)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (tenant_id, record_id, principal_id) DO UPDATE SET access = $5",
    )
    .bind(user.tenant_id)
    .bind(&entity)
    .bind(id)
    .bind(req.principal_id)
    .bind(&req.access)
    .execute(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(StatusCode::CREATED)
}

/// `GET /api/shares/:entity/:id` — list the record's **manual** shares
/// (`rule_id IS NULL`; rule-derived shares are managed via their rule, §5.11),
/// with the principal's name/email for usability. Owner/superuser only.
async fn list_shares(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((entity, id)): Path<(String, Uuid)>,
) -> ApiResult<Json<Vec<Value>>> {
    require_owner(&st, &user, &entity, id).await?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    #[derive(sqlx::FromRow)]
    struct ShareRow {
        principal_id: Uuid,
        access: String,
        name: Option<String>,
        email: Option<String>,
        created_at: String,
    }
    let rows: Vec<ShareRow> = sqlx::query_as(
        "SELECT rs.principal_id, rs.access, u.name, u.email, rs.created_at::text
           FROM sec.sec_record_share rs
           JOIN sec.sec_user u ON u.id = rs.principal_id
          WHERE rs.tenant_id = $1 AND rs.entity = $2 AND rs.record_id = $3
            AND rs.rule_id IS NULL
          ORDER BY rs.created_at",
    )
    .bind(user.tenant_id)
    .bind(&entity)
    .bind(id)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    let out: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "principal_id": r.principal_id,
                "access": r.access,
                "name": r.name,
                "email": r.email,
                "created_at": r.created_at,
            })
        })
        .collect();
    Ok(Json(out))
}

/// `DELETE /api/shares/:entity/:id/:principal_id` — revoke a manual share.
/// Owner/superuser only. Only manual shares (`rule_id IS NULL`) are revocable
/// here; a rule-derived share is revoked by updating/deactivating its rule
/// (which re-materializes through the epoch machinery, ADR-0013).
async fn delete_share(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((entity, id, principal_id)): Path<(String, Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    require_owner(&st, &user, &entity, id).await?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let res = sqlx::query(
        "DELETE FROM sec.sec_record_share
          WHERE tenant_id = $1 AND entity = $2 AND record_id = $3
            AND principal_id = $4 AND rule_id IS NULL",
    )
    .bind(user.tenant_id)
    .bind(&entity)
    .bind(id)
    .bind(principal_id)
    .execute(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    if res.rows_affected() == 0 {
        return Err(
            Error::NotFound(format!("no manual share for principal {principal_id}")).into(),
        );
    }
    Ok(StatusCode::NO_CONTENT)
}
