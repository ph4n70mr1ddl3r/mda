//! Studio API — draft → validate → publish lifecycle (PLAN §5.8, Phase 1).
//!
//! Phase 1 supports **additive ops only**: a publish may add modules, entities,
//! fields, and relationships; it may not remove, rename, or retype anything
//! already active (transforms/destructive arrive in Phase 2). `biz.*` table
//! generation also arrives in Phase 2 — here publish only updates the `meta`
//! model and bumps `md_active_version`.
//!
//! Editing is document-style: `PUT /drafts/:id/model` replaces the whole draft
//! model under an `If-Match` etag (optimistic concurrency → 409 on conflict).

use std::collections::HashSet;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use mda_core::{Error, Result};
use mda_data::ddl;
use mda_meta::draft::{diff, AdditionSummary, DiffReport, DraftModel, RetirementSummary};
use mda_meta::loader;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use mda_security::Identity;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/studio/drafts", post(create_draft))
        .route("/api/studio/drafts/:id", get(get_draft))
        .route(
            "/api/studio/drafts/:id/model",
            axum::routing::put(put_model),
        )
        .route("/api/studio/drafts/:id/validate", post(validate_draft))
        .route("/api/studio/drafts/:id/publish", post(publish_draft))
        .route("/api/studio/model", get(get_active_model))
        .route("/api/studio/export", get(get_active_model))
        .route("/api/studio/import", post(import_model))
        .route("/api/studio/snapshots", get(list_snapshots))
        .route("/api/studio/entities/:id", get(get_entity_definition))
}

fn require_studio(id: &Identity) -> ApiResult<()> {
    if !id.is_superuser {
        return Err(Error::Forbidden("studio access requires an admin role".into()).into());
    }
    Ok(())
}

// ===== DTOs =====

#[derive(sqlx::FromRow, Serialize)]
struct Draft {
    id: Uuid,
    tenant_id: Uuid,
    name: String,
    status: String,
    version_etag: Uuid,
    model: serde_json::Value,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct EtagResp {
    version_etag: Uuid,
    validation: DiffReport,
}

#[derive(Deserialize, Default)]
struct CreateDraftReq {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Serialize)]
struct PublishResult {
    draft_id: Uuid,
    version: i64,
    snapshot_id: Uuid,
    additions: AdditionSummary,
    retirements: RetirementSummary,
}

#[derive(sqlx::FromRow, Serialize)]
struct SnapshotRow {
    id: Uuid,
    version: i64,
    created_at: DateTime<Utc>,
}

// ===== handlers =====

async fn create_draft(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(req): Json<CreateDraftReq>,
) -> ApiResult<Json<Draft>> {
    let tenant = user.tenant_id;
    require_studio(&user)?;
    let name = req.name.unwrap_or_else(|| "draft".to_string());
    let active = loader::load_active_model(&st.pool, tenant).await?;
    let model_json = serde_json::to_value(&active).map_err(Error::internal)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    let draft: Draft = sqlx::query_as::<_, Draft>(
        "INSERT INTO meta.md_draft (tenant_id, name, model, status)
         VALUES ($1, $2, $3, 'draft')
         RETURNING id, tenant_id, name, status, version_etag, model, created_at, updated_at",
    )
    .bind(tenant)
    .bind(&name)
    .bind(&model_json)
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(draft))
}

async fn get_draft(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Draft>> {
    let tenant = user.tenant_id;
    require_studio(&user)?;
    let draft = fetch_draft(&st.pool, id, tenant).await?;
    ensure_tenant(&draft, user.tenant_id)?;
    Ok(Json(draft))
}

async fn put_model(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(model): Json<DraftModel>,
) -> ApiResult<Json<EtagResp>> {
    let tenant = user.tenant_id;
    require_studio(&user)?;
    let if_match = etag_from_headers(&headers)?;
    let model_json = serde_json::to_value(&model).map_err(Error::internal)?;

    // Ensure the draft belongs to this tenant before mutating.
    let existing = fetch_draft(&st.pool, id, tenant).await?;
    ensure_tenant(&existing, tenant)?;
    if existing.status != "draft" {
        return Err(Error::Conflict(format!("draft is {} (not editable)", existing.status)).into());
    }

    // Eagerly validate against the active model so the editor gets immediate
    // feedback (the publish step repeats this, but early feedback is critical).
    let active = loader::load_active_model(&st.pool, tenant).await?;
    let report = diff(&active, &model);

    let q = sqlx::query_as::<_, (Uuid,)>(
        "UPDATE meta.md_draft
            SET model = $3, version_etag = gen_random_uuid(), updated_at = now()
          WHERE id = $1 AND version_etag = $2
          RETURNING version_etag",
    )
    .bind(id)
    .bind(if_match)
    .bind(&model_json);
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    let updated: Option<(Uuid,)> = q.fetch_optional(&mut *tx).await.map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;

    match updated {
        Some((etag,)) => Ok(Json(EtagResp {
            version_etag: etag,
            validation: report,
        })),
        None => Err(Error::Conflict(
            "version_etag mismatch — draft was modified by another editor".into(),
        )
        .into()),
    }
}

async fn validate_draft(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<DiffReport>> {
    let tenant = user.tenant_id;
    require_studio(&user)?;
    let draft = fetch_draft(&st.pool, id, tenant).await?;
    ensure_tenant(&draft, tenant)?;
    let model: DraftModel = serde_json::from_value(draft.model.clone()).map_err(Error::internal)?;
    let active = loader::load_active_model(&st.pool, tenant).await?;
    Ok(Json(diff(&active, &model)))
}

async fn publish_draft(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<PublishResult>> {
    let tenant = user.tenant_id;
    require_studio(&user)?;
    let draft = fetch_draft(&st.pool, id, tenant).await?;
    ensure_tenant(&draft, tenant)?;
    if draft.status == "published" {
        return Err(Error::Conflict("draft already published".into()).into());
    }
    if draft.status == "publishing" {
        return Err(Error::Conflict("draft is currently publishing".into()).into());
    }

    let model: DraftModel = serde_json::from_value(draft.model.clone()).map_err(Error::internal)?;
    let active = loader::load_active_model(&st.pool, tenant).await?;
    let report = diff(&active, &model);
    if !report.valid {
        return Err(Error::Invalid(format!(
            "draft is not publishable (Phase 1 = additive only): {}",
            summarize(&report)
        ))
        .into());
    }

    let result = apply_additive_publish(&st.pool, tenant, id, &active, &model).await?;

    // Notify other instances + drop the local cache (fast path + eager).
    let _ = sqlx::query("SELECT pg_notify('meta_changed', $1)")
        .bind(tenant.to_string())
        .execute(&st.pool)
        .await;
    st.cache.invalidate_all();

    Ok(Json(result))
}

async fn get_active_model(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<DraftModel>> {
    let tenant = user.tenant_id;
    require_studio(&user)?;
    let model = loader::load_active_model(&st.pool, tenant).await?;
    Ok(Json(model))
}

async fn import_model(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(model): Json<DraftModel>,
) -> ApiResult<Json<Draft>> {
    let tenant = user.tenant_id;
    require_studio(&user)?;
    // Branch from active, then overlay the imported model as the draft content.
    let active = loader::load_active_model(&st.pool, tenant).await?;
    let _ = active; // branched model is the starting point; we replace with imported
    let model_json = serde_json::to_value(&model).map_err(Error::internal)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    let draft: Draft = sqlx::query_as::<_, Draft>(
        "INSERT INTO meta.md_draft (tenant_id, name, model, status)
         VALUES ($1, 'imported', $2, 'draft')
         RETURNING id, tenant_id, name, status, version_etag, model, created_at, updated_at",
    )
    .bind(tenant)
    .bind(&model_json)
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(draft))
}

async fn list_snapshots(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<SnapshotRow>>> {
    let tenant = user.tenant_id;
    require_studio(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    let rows: Vec<SnapshotRow> = sqlx::query_as::<_, SnapshotRow>(
        "SELECT id, version, created_at FROM meta.md_snapshot
          WHERE tenant_id = $1 ORDER BY version DESC",
    )
    .bind(tenant)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(rows))
}

/// Read an entity definition **through the cache** (exercises the loader +
/// invalidation; the runtime data layer in Phase 2 will use the same path).
async fn get_entity_definition(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<mda_meta::EntityDefinition>> {
    let tenant = user.tenant_id;
    require_studio(&user)?;
    let def = st.cache.get_entity(&st.pool, tenant, id).await?;
    Ok(Json((*def).clone()))
}

// ===== helpers =====

/// Load a draft by id under the caller's tenant GUC (md_draft is RLS-gated, so a
/// GUC-less lookup returns nothing). `ensure_tenant` becomes a belt-and-suspenders
/// check — RLS already guaranteed the row is this tenant's.
async fn fetch_draft(pool: &PgPool, id: Uuid, tenant: Uuid) -> Result<Draft> {
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    let row = sqlx::query_as::<_, Draft>(
        "SELECT id, tenant_id, name, status, version_etag, model, created_at, updated_at
           FROM meta.md_draft WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    row.ok_or_else(|| Error::NotFound(format!("draft {id}")))
}

fn ensure_tenant(draft: &Draft, tenant: Uuid) -> Result<()> {
    if draft.tenant_id != tenant {
        return Err(Error::NotFound(format!("draft {}", draft.id)));
    }
    Ok(())
}

fn etag_from_headers(headers: &HeaderMap) -> Result<Uuid> {
    headers
        .get("if-match")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| Error::Invalid("If-Match version_etag header required".into()))
}

fn summarize(report: &DiffReport) -> String {
    let mut parts = Vec::new();
    if !report.violations.is_empty() {
        parts.push(format!("violations=[{}]", report.violations.join("; ")));
    }
    if !report.errors.is_empty() {
        parts.push(format!("errors=[{}]", report.errors.join("; ")));
    }
    parts.join(", ")
}

/// Apply an additive-only publish in a single transaction: archive the prior
/// active model to a snapshot, INSERT the new artifacts, bump the version, mark
/// the draft published.
async fn apply_additive_publish(
    pool: &PgPool,
    tenant: Uuid,
    draft_id: Uuid,
    active: &DraftModel,
    draft: &DraftModel,
) -> Result<PublishResult> {
    let active_module_ids: HashSet<Uuid> = active.modules.iter().map(|m| m.id).collect();
    let active_entity_ids: HashSet<Uuid> = active.entities.iter().map(|e| e.id).collect();
    let active_field_ids: HashSet<Uuid> = active
        .entities
        .iter()
        .flat_map(|e| e.fields.iter().map(|f| f.id))
        .collect();
    let active_rel_ids: HashSet<Uuid> = active
        .entities
        .iter()
        .flat_map(|e| e.relationships.iter().map(|r| r.id))
        .collect();

    let mut tx = pool.begin().await.map_err(Error::internal)?;
    // All meta.md_* writes below are RLS-gated → set the tenant GUC for the txn.
    mda_security::set_tenant(&mut tx, tenant).await?;

    // 1) archive the current active model
    let active_json = serde_json::to_value(active).map_err(Error::internal)?;
    let (snapshot_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO meta.md_snapshot (tenant_id, version, model, manifest)
         VALUES ($1, COALESCE((SELECT version FROM meta.md_active_version WHERE tenant_id = $1), 0), $2, $3)
         RETURNING id",
    )
    .bind(tenant)
    .bind(&active_json)
    .bind(serde_json::json!({"reason":"publish","draft_id":draft_id.to_string()}))
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::internal)?;

    // 2) INSERT additions: modules
    let mut additions = AdditionSummary::default();
    for m in &draft.modules {
        if active_module_ids.contains(&m.id) {
            continue;
        }
        additions.modules += 1;
        sqlx::query(
            "INSERT INTO meta.md_module (id, tenant_id, name, label)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(m.id)
        .bind(tenant)
        .bind(&m.name)
        .bind(&m.label)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
    }

    // 3) INSERT additions: entities
    for e in &draft.entities {
        if active_entity_ids.contains(&e.id) {
            continue;
        }
        additions.entities += 1;
        sqlx::query(
            "INSERT INTO meta.md_entity
                (id, tenant_id, module_id, table_name, name, label, description, status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'active')",
        )
        .bind(e.id)
        .bind(tenant)
        .bind(e.module_id)
        .bind(&e.table_name)
        .bind(&e.name)
        .bind(&e.label)
        .bind(&e.description)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
    }

    // 4) INSERT additions: fields (entity must exist — either active or just inserted)
    for e in &draft.entities {
        for f in &e.fields {
            if active_field_ids.contains(&f.id) {
                continue;
            }
            additions.fields += 1;
            sqlx::query(
                "INSERT INTO meta.md_field
                    (id, tenant_id, entity_id, name, label, field_type, required,
                     is_unique, is_indexed, default_expr, config, status)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,'active')",
            )
            .bind(f.id)
            .bind(tenant)
            .bind(e.id)
            .bind(&f.name)
            .bind(&f.label)
            .bind(&f.field_type)
            .bind(f.required)
            .bind(f.is_unique)
            .bind(f.is_indexed)
            .bind(&f.default_expr)
            .bind(&f.config)
            .execute(&mut *tx)
            .await
            .map_err(Error::internal)?;
        }
    }

    // 5) INSERT additions: relationships
    for e in &draft.entities {
        for r in &e.relationships {
            if active_rel_ids.contains(&r.id) {
                continue;
            }
            additions.relationships += 1;
            sqlx::query(
                "INSERT INTO meta.md_relationship
                    (id, tenant_id, source_entity_id, source_field_name, target_entity_id,
                     cardinality, strength, on_delete, required, reference_qualifier, rollup_summary)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
            )
            .bind(r.id)
            .bind(tenant)
            .bind(e.id)
            .bind(&r.source_field_name)
            .bind(r.target_entity_id)
            .bind(&r.cardinality)
            .bind(&r.strength)
            .bind(&r.on_delete)
            .bind(r.required)
            .bind(&r.reference_qualifier)
            .bind(&r.rollup_summary)
            .execute(&mut *tx)
            .await
            .map_err(Error::internal)?;
        }
    }

    // 6) biz DDL + retire (Phase 2): materialize `biz.<table>` for new entities,
    //    add columns/FKs for new fields/relationships, and retire removed
    //    entities/fields (two-phase: status='retired' + md_retirement; live data
    //    is kept until a purge job drops it). All DDL is transactional in PG.
    let mut retirements = RetirementSummary::default();
    let draft_entity_ids: HashSet<Uuid> = draft.entities.iter().map(|e| e.id).collect();
    let draft_field_ids: HashSet<Uuid> = draft
        .entities
        .iter()
        .flat_map(|e| e.fields.iter().map(|f| f.id))
        .collect();
    // entity_id -> table_name (active + draft) for FK target resolution
    let mut table_of: std::collections::HashMap<Uuid, &str> = std::collections::HashMap::new();
    for e in &active.entities {
        table_of.insert(e.id, e.table_name.as_str());
    }
    for e in &draft.entities {
        table_of.insert(e.id, e.table_name.as_str());
    }

    exec_stmts(&mut tx, &ddl::ensure_schema()).await?;
    // new entities -> CREATE TABLE (their fields are included as columns)
    for e in &draft.entities {
        if active_entity_ids.contains(&e.id) {
            continue;
        }
        exec_stmts(&mut tx, &ddl::create_table(&e.table_name, e)?).await?;
    }
    // added fields on EXISTING entities -> ALTER ADD COLUMN
    for e in &draft.entities {
        if !active_entity_ids.contains(&e.id) {
            continue;
        }
        for f in &e.fields {
            if active_field_ids.contains(&f.id) {
                continue;
            }
            exec_stmts(&mut tx, &ddl::add_field(&e.table_name, f)?).await?;
        }
    }
    // added relationships -> ALTER ADD FK column + constraint (after tables exist)
    for e in &draft.entities {
        for r in &e.relationships {
            if active_rel_ids.contains(&r.id) {
                continue;
            }
            let target = table_of.get(&r.target_entity_id).ok_or_else(|| {
                Error::Invalid(format!(
                    "relationship {} targets unknown entity {}",
                    r.source_field_name, r.target_entity_id
                ))
            })?;
            exec_stmts(&mut tx, &ddl::add_relationship(&e.table_name, r, target)?).await?;
        }
    }
    // retire removed entities / fields (two-phase)
    for ae in &active.entities {
        if !draft_entity_ids.contains(&ae.id) {
            retirements.entities += 1;
            retire_entity(&mut tx, tenant, ae.id).await?;
        }
        for af in &ae.fields {
            if !draft_field_ids.contains(&af.id) {
                retirements.fields += 1;
                retire_field(&mut tx, tenant, af.id).await?;
            }
        }
    }

    // 7) bump the active version (create-or-increment)
    sqlx::query(
        "INSERT INTO meta.md_active_version (tenant_id, version, snapshot_id, updated_at)
         VALUES ($1, 1, $2, now())
         ON CONFLICT (tenant_id) DO UPDATE
            SET version = meta.md_active_version.version + 1,
                snapshot_id = EXCLUDED.snapshot_id,
                updated_at = now()",
    )
    .bind(tenant)
    .bind(snapshot_id)
    .execute(&mut *tx)
    .await
    .map_err(Error::internal)?;

    // 7) mark draft published
    sqlx::query("UPDATE meta.md_draft SET status = 'published', updated_at = now() WHERE id = $1")
        .bind(draft_id)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;

    tx.commit().await.map_err(Error::internal)?;

    let version = loader::active_version(pool, tenant).await?;
    Ok(PublishResult {
        draft_id,
        version,
        snapshot_id,
        additions,
        retirements,
    })
}

/// Execute a batch of (identifier-only) DDL statements inside the publish txn.
async fn exec_stmts(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    stmts: &[String],
) -> Result<()> {
    for s in stmts {
        sqlx::query(s)
            .execute(&mut **tx)
            .await
            .map_err(Error::internal)?;
    }
    Ok(())
}

const RETIRE_GRACE: &str = "14 days";

/// Two-phase retire of an entity: status -> retired + a pending-purge row.
/// Live `biz.<table>` data is kept until a purge job drops it (PLAN §5.8).
async fn retire_entity(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    entity_id: Uuid,
) -> Result<()> {
    sqlx::query("UPDATE meta.md_entity SET status = 'retired', updated_at = now() WHERE id = $1 AND tenant_id = $2")
        .bind(entity_id)
        .bind(tenant)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
    sqlx::query(
        "INSERT INTO meta.md_retirement (tenant_id, kind, target_id, purge_after)
         VALUES ($1, 'entity', $2, now() + ($3)::interval)",
    )
    .bind(tenant)
    .bind(entity_id)
    .bind(RETIRE_GRACE)
    .execute(&mut **tx)
    .await
    .map_err(Error::internal)?;
    Ok(())
}

/// Two-phase retire of a field (its biz column stays until purge).
async fn retire_field(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    field_id: Uuid,
) -> Result<()> {
    sqlx::query("UPDATE meta.md_field SET status = 'retired', updated_at = now() WHERE id = $1 AND tenant_id = $2")
        .bind(field_id)
        .bind(tenant)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
    sqlx::query(
        "INSERT INTO meta.md_retirement (tenant_id, kind, target_id, purge_after)
         VALUES ($1, 'field', $2, now() + ($3)::interval)",
    )
    .bind(tenant)
    .bind(field_id)
    .bind(RETIRE_GRACE)
    .execute(&mut **tx)
    .await
    .map_err(Error::internal)?;
    Ok(())
}
