//! Tenant configuration export/import (PLAN §14: the backup half of
//! "tenant-scoped backup/restore").
//!
//! `GET  /api/tenants/export` produces a portable JSON snapshot of a tenant's
//! *configuration* — the active model plus every tenant-scoped definition table
//! (reports, rules, templates, notification types, schedules, the security
//! graph, and integration definitions). This is the part of a tenant that is
//! painful to recreate by hand and the natural unit of backup, audit, and
//! cross-environment migration.
//!
//! `POST /api/tenants/import` restores such a bundle into the caller's tenant:
//! the model becomes a reviewable Studio draft (publish to materialize the biz
//! tables), and every config table is **merged by natural key** — a role /
//! connector / report that already exists under the same name is updated in
//! place (and the bundle's ids are remapped to it), so an import is safe into a
//! tenant that already carries bootstrap config (e.g. the `admin` role), and is
//! idempotent on re-import. A fresh tenant is seeded verbatim.
//!
//! **Scope & honesty:** full tenant-scoped *data* export/restore + regional
//! placement remain tied to the deferred tenant lifecycle (§5.4) and HA (U9).
//! The merge is keyed on natural business keys (role/connector/report name,
//! notification-type key, OWD entity); two bundles whose *different* ids collide
//! on the same key resolve to the existing row, with FK references (permissions
//! → role, flows → connector, schedules → report/flow) rewritten through the
//! id map. Users, sessions, and record data are never touched.
//!
//! Superuser-only: the bundle aggregates every entity's definitions (incl. the
//! security graph) regardless of the caller's per-entity grants.

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use mda_core::{Error, Result};
use mda_meta::DraftModel;
use mda_security::set_tenant;
use serde_json::{json, Value};
use std::collections::HashMap;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/tenants/export", get(export_tenant))
        .route("/api/tenants/import", post(import_tenant))
}

/// `GET /api/tenants/export` — a JSON snapshot of the caller's tenant config.
async fn export_tenant(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Value>> {
    if !user.is_superuser {
        return Err(Error::Forbidden("tenant export requires an admin role".into()).into());
    }
    let tenant = user.tenant_id;

    // The active model (entities / fields / relationships / modules) — the same
    // shape Studio accepts, so a restore round-trips through the publish API.
    let model = mda_meta::loader::load_active_model(&st.pool, tenant).await?;

    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, tenant).await?;

    let reports = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM meta.md_report t").await;
    let rules = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM meta.md_rule t").await;
    let templates = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM meta.md_template t").await;
    let notification_types = table_json(
        &mut tx,
        "SELECT to_jsonb(t.*) FROM meta.md_notification_type t",
    )
    .await;
    let schedules = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM sys_schedule t").await;
    let translations = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM meta.md_translation t").await;

    // Security graph (definitions only — never users, sessions, or shares).
    let roles = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM sec.sec_role t").await;
    let permissions = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM sec.sec_permission t").await;
    let field_permissions = table_json(
        &mut tx,
        "SELECT to_jsonb(t.*) FROM sec.sec_field_permission t",
    )
    .await;
    let owd = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM sec.sec_owd t").await;
    let teams = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM sec.sec_team t").await;
    let share_rules = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM sec.sec_share_rule t").await;
    let role_hierarchy = table_json(
        &mut tx,
        "SELECT to_jsonb(t.*) FROM sec.sec_role_hierarchy t",
    )
    .await;

    // UI definitions (Phase 6): forms, views, dashboards, navigation.
    let forms = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM meta.md_form t").await;
    let views = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM meta.md_view t").await;
    let dashboards = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM meta.md_dashboard t").await;
    let navigation = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM meta.md_navigation t").await;

    // Integration definitions.
    let connectors = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM int.connector t").await;
    let flows = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM int.flow t").await;
    let value_maps = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM int.value_map t").await;

    tx.commit().await.map_err(Error::internal)?;

    Ok(Json(json!({
        "schema_version": 1,
        "tenant_id": tenant,
        "exported_at": chrono::Utc::now(),
        "model": model,
        "reports": reports,
        "rules": rules,
        "templates": templates,
        "notification_types": notification_types,
        "schedules": schedules,
        "translations": translations,
        "security": {
            "roles": roles,
            "permissions": permissions,
            "field_permissions": field_permissions,
            "owd": owd,
            "teams": teams,
            "share_rules": share_rules,
            "role_hierarchy": role_hierarchy,
        },
        "integrations": {
            "connectors": connectors,
            "flows": flows,
            "value_maps": value_maps,
        },
        "ui": {
            "forms": forms,
            "views": views,
            "dashboards": dashboards,
            "navigation": navigation,
        },
    })))
}

/// Read every row of `query` (already tenant-GUC-scoped) as a JSON array. Errors
/// are logged and degrade to an empty array rather than failing the whole export
/// — a missing/renamed optional table shouldn't abort a backup.
async fn table_json(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, query: &str) -> Vec<Value> {
    match sqlx::query_as::<_, (Value,)>(query)
        .fetch_all(&mut **tx)
        .await
    {
        Ok(rows) => rows.into_iter().map(|(v,)| v).collect(),
        Err(e) => {
            tracing::warn!(query = %query, error = %e, "export: table skipped");
            Vec::new()
        }
    }
}

// ===== import (restore a bundle into the caller's tenant) =====
//
// Small typed extractors over the `to_jsonb(t.*)` row objects the export emits.
// JSONB renders a Postgres `uuid` as a string, `jsonb` as an object/array, and a
// `text[]` as a JSON array — so a uuid field is `.as_str()`→parse and a jsonb
// field is the value itself.

fn j_uuid(v: &Value, key: &str) -> Result<Uuid> {
    v.get(key)
        .and_then(|x| x.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| Error::Invalid(format!("import: missing/invalid uuid field `{key}`")))
}

fn j_str<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(|x| x.as_str())
        .ok_or_else(|| Error::Invalid(format!("import: missing string field `{key}`")))
}

fn j_opt_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn j_value(v: &Value, key: &str) -> Value {
    v.get(key).cloned().unwrap_or(Value::Null)
}

fn j_bool(v: &Value, key: &str, default: bool) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(default)
}

fn j_int(v: &Value, key: &str, default: i32) -> Result<i32> {
    // Checked, not `as i32` — an out-of-range value in a (tampered) bundle
    // would wrap and silently reorder rule firing priority.
    v.get(key)
        .and_then(|x| x.as_i64())
        .map(|n| {
            i32::try_from(n)
                .map_err(|_| Error::Invalid(format!("import: field `{key}` out of range: {n}")))
        })
        .transpose()
        .map(|n| n.unwrap_or(default))
}

fn j_opt_uuid(v: &Value, key: &str) -> Option<Uuid> {
    v.get(key)
        .and_then(|x| x.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
}

/// Coerce a bundle field into a `&[Value]` array (missing → empty).
fn arr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[])
}

/// `POST /api/tenants/import` — restore an export bundle into the caller's
/// tenant. The model becomes a Studio draft (publish to materialize biz tables);
/// every config table is merged by natural key (see the module docs).
async fn import_tenant(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(bundle): Json<Value>,
) -> ApiResult<Json<Value>> {
    if !user.is_superuser {
        return Err(Error::Forbidden("tenant import requires an admin role".into()).into());
    }
    let tenant = user.tenant_id;
    let schema_version = bundle
        .get("schema_version")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if schema_version != 1 {
        return Err(Error::Invalid(format!(
            "unsupported bundle schema_version {schema_version} (expected 1)"
        ))
        .into());
    }

    // --- one transaction for the WHOLE import: the model draft and every
    // config table commit together, so a rejected bundle (bad model, cyclic
    // hierarchy, missing FK target, …) leaves nothing behind — not even a
    // 'restored' draft the caller would have to clean up by hand. ---
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, tenant).await?;

    // model → a reviewable Studio draft (publish to materialize biz.*)
    let draft_id: Option<Uuid> = if let Some(model_val) = bundle.get("model") {
        let model: DraftModel = serde_json::from_value(model_val.clone())
            .map_err(|e| Error::Invalid(format!("bundle `model` is not a valid model: {e}")))?;
        let model_json = serde_json::to_value(&model).map_err(Error::internal)?;
        let (id,): (Uuid,) = sqlx::query_as(
            "INSERT INTO meta.md_draft (tenant_id, name, model, status)
             VALUES ($1, 'restored', $2, 'draft') RETURNING id",
        )
        .bind(tenant)
        .bind(&model_json)
        .fetch_one(&mut *tx)
        .await
        .map_err(Error::internal)?;
        Some(id)
    } else {
        None
    };

    let security = bundle.get("security").cloned().unwrap_or(Value::Null);
    let integrations = bundle.get("integrations").cloned().unwrap_or(Value::Null);
    let ui = bundle.get("ui").cloned().unwrap_or(Value::Null);

    // FK-target id maps: bundle id → actual id in this tenant (existing row when
    // the natural key already existed, else the bundle id after a fresh insert).
    let mut role_map: HashMap<Uuid, Uuid> = HashMap::new();
    let mut report_map: HashMap<Uuid, Uuid> = HashMap::new();
    let mut flow_map: HashMap<Uuid, Uuid> = HashMap::new();
    let mut connector_map: HashMap<Uuid, Uuid> = HashMap::new();
    let mut team_map: HashMap<Uuid, Uuid> = HashMap::new();

    // order respects FK references: teams/roles first, then role-keyed perms;
    // connectors before flows; reports + flows before schedules (target_id).
    let n_teams = restore_teams(&mut tx, tenant, arr(&security, "teams"), &mut team_map).await?;
    let n_roles = restore_roles(&mut tx, tenant, arr(&security, "roles"), &mut role_map).await?;
    let n_perms = restore_permissions(&mut tx, arr(&security, "permissions"), &role_map).await?;
    let n_field_perms =
        restore_field_permissions(&mut tx, arr(&security, "field_permissions"), &role_map).await;
    let n_owd = restore_owd(&mut tx, tenant, arr(&security, "owd")).await?;

    let n_templates = restore_templates(&mut tx, tenant, arr(&bundle, "templates")).await?;
    let n_notif_types =
        restore_notification_types(&mut tx, tenant, arr(&bundle, "notification_types")).await?;
    let n_reports =
        restore_reports(&mut tx, tenant, arr(&bundle, "reports"), &mut report_map).await?;
    let n_rules = restore_rules(&mut tx, tenant, arr(&bundle, "rules")).await?;
    let n_translations =
        restore_translations(&mut tx, tenant, arr(&bundle, "translations")).await?;

    let n_connectors = restore_connectors(
        &mut tx,
        tenant,
        arr(&integrations, "connectors"),
        &mut connector_map,
    )
    .await?;
    let n_value_maps =
        restore_value_maps(&mut tx, tenant, arr(&integrations, "value_maps")).await?;
    let n_flows = restore_flows(
        &mut tx,
        tenant,
        arr(&integrations, "flows"),
        &connector_map,
        &mut flow_map,
    )
    .await?;

    let n_share_rules = restore_share_rules(&mut tx, tenant, arr(&security, "share_rules")).await?;
    let n_role_hierarchy =
        restore_role_hierarchy(&mut tx, tenant, arr(&security, "role_hierarchy"), &role_map)
            .await?;

    let n_forms =
        restore_ui_entity_rows(&mut tx, tenant, arr(&ui, "forms"), "meta.md_form").await?;
    let n_views =
        restore_ui_entity_rows(&mut tx, tenant, arr(&ui, "views"), "meta.md_view").await?;
    let n_dashboards =
        restore_dashboards(&mut tx, tenant, arr(&ui, "dashboards"), &report_map).await?;
    let n_navigation = restore_navigation(&mut tx, tenant, arr(&ui, "navigation")).await?;

    let n_schedules = restore_schedules(
        &mut tx,
        tenant,
        arr(&bundle, "schedules"),
        &report_map,
        &flow_map,
    )
    .await?;

    tx.commit().await.map_err(Error::internal)?;

    Ok(Json(json!({
        "tenant_id": tenant,
        "draft_id": draft_id,
        "restored": {
            "teams": n_teams,
            "roles": n_roles,
            "permissions": n_perms,
            "field_permissions": n_field_perms,
            "owd": n_owd,
            "templates": n_templates,
            "notification_types": n_notif_types,
            "reports": n_reports,
            "rules": n_rules,
            "schedules": n_schedules,
            "translations": n_translations,
            "connectors": n_connectors,
            "value_maps": n_value_maps,
            "share_rules": n_share_rules,
            "role_hierarchy": n_role_hierarchy,
            "forms": n_forms,
            "views": n_views,
            "dashboards": n_dashboards,
            "navigation": n_navigation,
            "flows": n_flows,
        },
        "note": "model staged as a Studio draft; POST /api/studio/drafts/:id/publish to materialize biz tables",
    })))
}

/// Resolve a bundle id through a remap (defaults to the bundle id when the key
/// is absent — e.g. a permission whose role wasn't in the bundle).
fn remap(map: &HashMap<Uuid, Uuid>, id: Uuid) -> Uuid {
    *map.get(&id).unwrap_or(&id)
}

async fn restore_teams(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rows: &[Value],
    map: &mut HashMap<Uuid, Uuid>,
) -> Result<usize> {
    let mut n = 0;
    // Pass 1: insert/update every team by natural key (name) and record the
    // bundle-id → actual-id mapping. parent_id is left NULL here because a
    // team's parent may be declared later in the bundle (and the self-FK needs
    // the parent row to exist first).
    for r in rows {
        let id = j_uuid(r, "id")?;
        let name = j_str(r, "name")?;
        let existing: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM sec.sec_team WHERE tenant_id = $1 AND name = $2")
                .bind(tenant)
                .bind(name)
                .fetch_optional(&mut **tx)
                .await
                .map_err(Error::internal)?;
        let actual = match existing {
            Some((eid,)) => {
                sqlx::query("UPDATE sec.sec_team SET name = $2 WHERE id = $1")
                    .bind(eid)
                    .bind(name)
                    .execute(&mut **tx)
                    .await
                    .map_err(Error::internal)?;
                eid
            }
            None => {
                sqlx::query(
                    "INSERT INTO sec.sec_team (id, tenant_id, name) VALUES ($1, $2, $3)
                     ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name",
                )
                .bind(id)
                .bind(tenant)
                .bind(name)
                .execute(&mut **tx)
                .await
                .map_err(Error::internal)?;
                id
            }
        };
        map.insert(id, actual);
        n += 1;
    }
    // Pass 2: re-link the hierarchy. parent_id is remapped through the map (a
    // parent that already existed under a different id resolves correctly), and
    // is dropped if the bundle references a parent that wasn't in the bundle.
    // A link that would CLOSE A CYCLE is rejected: the admin API refuses
    // cyclic hierarchies (`would_cycle`), and every consumer of `parent_id`
    // walks the graph — the import path must not smuggle one in behind that
    // guard (a cyclic graph also makes several of those walks spin).
    for r in rows {
        let id = j_uuid(r, "id")?;
        let Some(actual) = map.get(&id).copied() else {
            continue;
        };
        let parent = r
            .get("parent_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok());
        let parent_actual = parent.and_then(|p| map.get(&p).copied());
        if let Some(p) = parent_actual {
            if p == actual {
                return Err(Error::Invalid(format!(
                    "team hierarchy cycle: team {} cannot be its own parent",
                    r.get("name").and_then(|v| v.as_str()).unwrap_or("?")
                )));
            }
            // Walking UP from the proposed parent must never reach the child.
            let hit: Option<(i32,)> = sqlx::query_as(
                "WITH RECURSIVE up(tid) AS (
                        SELECT $2::uuid
                        UNION
                        SELECT t.parent_id FROM sec.sec_team t JOIN up ON t.id = up.tid)
                 SELECT 1 WHERE $1::uuid IN (SELECT tid FROM up)",
            )
            .bind(actual)
            .bind(p)
            .fetch_optional(&mut **tx)
            .await
            .map_err(Error::internal)?;
            if hit.is_some() {
                return Err(Error::Invalid(
                    "import would create a team-hierarchy cycle (a team is already an ancestor of its proposed parent)".into(),
                ));
            }
        }
        sqlx::query("UPDATE sec.sec_team SET parent_id = $2 WHERE id = $1")
            .bind(actual)
            .bind(parent_actual)
            .execute(&mut **tx)
            .await
            .map_err(Error::internal)?;
    }
    Ok(n)
}

async fn restore_roles(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rows: &[Value],
    map: &mut HashMap<Uuid, Uuid>,
) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        let id = j_uuid(r, "id")?;
        let name = j_str(r, "name")?;
        let existing: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM sec.sec_role WHERE tenant_id = $1 AND name = $2")
                .bind(tenant)
                .bind(name)
                .fetch_optional(&mut **tx)
                .await
                .map_err(Error::internal)?;
        let actual = match existing {
            Some((eid,)) => {
                sqlx::query("UPDATE sec.sec_role SET name = $2 WHERE id = $1")
                    .bind(eid)
                    .bind(name)
                    .execute(&mut **tx)
                    .await
                    .map_err(Error::internal)?;
                eid
            }
            None => {
                sqlx::query(
                    "INSERT INTO sec.sec_role (id, tenant_id, name) VALUES ($1, $2, $3)
                     ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name",
                )
                .bind(id)
                .bind(tenant)
                .bind(name)
                .execute(&mut **tx)
                .await
                .map_err(Error::internal)?;
                id
            }
        };
        map.insert(id, actual);
        n += 1;
    }
    Ok(n)
}

async fn restore_permissions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    rows: &[Value],
    role_map: &HashMap<Uuid, Uuid>,
) -> Result<usize> {
    // tenant_id is auto-filled from the role by the BEFORE INSERT trigger.
    let mut n = 0;
    for r in rows {
        let bundle_role = j_uuid(r, "role_id")?;
        let role_id = remap(role_map, bundle_role);
        let entity = j_str(r, "entity")?;
        let verb = j_str(r, "verb")?;
        sqlx::query(
            "INSERT INTO sec.sec_permission (role_id, entity, verb) VALUES ($1, $2, $3)
             ON CONFLICT (role_id, entity, verb) DO NOTHING",
        )
        .bind(role_id)
        .bind(entity)
        .bind(verb)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
        n += 1;
    }
    Ok(n)
}

async fn restore_field_permissions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    rows: &[Value],
    role_map: &HashMap<Uuid, Uuid>,
) -> usize {
    let mut n = 0;
    for r in rows {
        let bundle_role = match j_uuid(r, "role_id") {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(?e, "import: skipping malformed field_permission");
                continue;
            }
        };
        let role_id = remap(role_map, bundle_role);
        let entity = match j_str(r, "entity") {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(?e, "import: skipping malformed field_permission");
                continue;
            }
        };
        let field = match j_str(r, "field") {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(?e, "import: skipping malformed field_permission");
                continue;
            }
        };
        let access = match j_str(r, "access") {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(?e, "import: skipping malformed field_permission");
                continue;
            }
        };
        let res = sqlx::query(
            "INSERT INTO sec.sec_field_permission (role_id, entity, field, access)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (role_id, entity, field) DO UPDATE SET access = EXCLUDED.access",
        )
        .bind(role_id)
        .bind(entity)
        .bind(field)
        .bind(access)
        .execute(&mut **tx)
        .await;
        if let Err(e) = res {
            tracing::warn!(?e, "import: field_permission skipped");
            continue;
        }
        n += 1;
    }
    n
}

async fn restore_owd(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rows: &[Value],
) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        let entity = j_str(r, "entity")?;
        let access = j_str(r, "default_access")?;
        sqlx::query(
            "INSERT INTO sec.sec_owd (tenant_id, entity, default_access)
             VALUES ($1, $2, $3)
             ON CONFLICT (tenant_id, entity) DO UPDATE SET default_access = EXCLUDED.default_access",
        )
        .bind(tenant)
        .bind(entity)
        .bind(access)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
        n += 1;
    }
    Ok(n)
}

async fn restore_templates(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rows: &[Value],
) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        let id = j_uuid(r, "id")?;
        let name = j_str(r, "name")?;
        let kind = j_str(r, "kind")?;
        let body = j_str(r, "body")?;
        let content_type = r
            .get("content_type")
            .and_then(|x| x.as_str())
            .unwrap_or("text/plain");
        let locale: Option<&str> = r.get("locale").and_then(|x| x.as_str());
        // natural key (tenant, name, locale)
        let existing: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM meta.md_template WHERE tenant_id = $1 AND name = $2 AND locale IS NOT DISTINCT FROM $3",
        )
        .bind(tenant)
        .bind(name)
        .bind(locale)
        .fetch_optional(&mut **tx)
        .await
        .map_err(Error::internal)?;
        match existing {
            Some((eid,)) => {
                sqlx::query(
                    "UPDATE meta.md_template SET name=$2, kind=$3, body=$4, content_type=$5, locale=$6
                     WHERE id = $1",
                )
                .bind(eid)
                .bind(name)
                .bind(kind)
                .bind(body)
                .bind(content_type)
                .bind(locale)
                .execute(&mut **tx)
                .await
                .map_err(Error::internal)?;
            }
            None => {
                sqlx::query(
                    "INSERT INTO meta.md_template (id, tenant_id, name, kind, body, content_type, locale)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)
                     ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, kind = EXCLUDED.kind,
                     body = EXCLUDED.body, content_type = EXCLUDED.content_type, locale = EXCLUDED.locale",
                )
                .bind(id)
                .bind(tenant)
                .bind(name)
                .bind(kind)
                .bind(body)
                .bind(content_type)
                .bind(locale)
                .execute(&mut **tx)
                .await
                .map_err(Error::internal)?;
            }
        }
        n += 1;
    }
    Ok(n)
}

async fn restore_notification_types(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rows: &[Value],
) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        let id = j_uuid(r, "id")?;
        let key = j_str(r, "key")?;
        let label = j_str(r, "label")?;
        let channels: Vec<String> = r
            .get("default_channels")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_else(|| vec!["in_app".to_string()]);
        let template_name: Option<String> = r
            .get("template_name")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let digestible = j_bool(r, "digestible", false);
        let existing: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM meta.md_notification_type WHERE tenant_id = $1 AND key = $2",
        )
        .bind(tenant)
        .bind(key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(Error::internal)?;
        match existing {
            Some((eid,)) => {
                sqlx::query(
                    "UPDATE meta.md_notification_type SET key=$2, label=$3, default_channels=$4,
                        template_name=$5, digestible=$6 WHERE id = $1",
                )
                .bind(eid)
                .bind(key)
                .bind(label)
                .bind(&channels)
                .bind(template_name)
                .bind(digestible)
                .execute(&mut **tx)
                .await
                .map_err(Error::internal)?;
            }
            None => {
                sqlx::query(
                    "INSERT INTO meta.md_notification_type
                        (id, tenant_id, key, label, default_channels, template_name, digestible)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)
                     ON CONFLICT (id) DO UPDATE SET key = EXCLUDED.key, label = EXCLUDED.label,
                        default_channels = EXCLUDED.default_channels,
                        template_name = EXCLUDED.template_name, digestible = EXCLUDED.digestible",
                )
                .bind(id)
                .bind(tenant)
                .bind(key)
                .bind(label)
                .bind(&channels)
                .bind(template_name)
                .bind(digestible)
                .execute(&mut **tx)
                .await
                .map_err(Error::internal)?;
            }
        }
        n += 1;
    }
    Ok(n)
}

async fn restore_reports(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rows: &[Value],
    map: &mut HashMap<Uuid, Uuid>,
) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        let id = j_uuid(r, "id")?;
        let name = j_str(r, "name")?;
        let dataset = j_value(r, "dataset");
        let existing: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM meta.md_report WHERE tenant_id = $1 AND name = $2")
                .bind(tenant)
                .bind(name)
                .fetch_optional(&mut **tx)
                .await
                .map_err(Error::internal)?;
        let actual = existing.map(|(id,)| id).unwrap_or(id);
        map.insert(id, actual);
        sqlx::query(
            "INSERT INTO meta.md_report (id, tenant_id, name, dataset) VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, dataset = EXCLUDED.dataset",
        )
        .bind(actual)
        .bind(tenant)
        .bind(name)
        .bind(dataset)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
        n += 1;
    }
    Ok(n)
}

async fn restore_rules(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rows: &[Value],
) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        let id = j_uuid(r, "id")?;
        let entity = j_str(r, "entity")?;
        let event = j_str(r, "event")?;
        let condition = j_value(r, "condition");
        let action_type = j_str(r, "action_type")?;
        let action_field: Option<String> = j_opt_str(r, "action_field");
        let action_value = j_value(r, "action_value");
        let active = j_bool(r, "active", true);
        let priority = j_int(r, "priority", 100)?;
        sqlx::query(
            "INSERT INTO meta.md_rule
                (id, tenant_id, entity, event, condition, action_type, action_field,
                 action_value, active, priority)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (id) DO UPDATE SET
                entity = EXCLUDED.entity, event = EXCLUDED.event, condition = EXCLUDED.condition,
                action_type = EXCLUDED.action_type, action_field = EXCLUDED.action_field,
                action_value = EXCLUDED.action_value, active = EXCLUDED.active,
                priority = EXCLUDED.priority",
        )
        .bind(id)
        .bind(tenant)
        .bind(entity)
        .bind(event)
        .bind(condition)
        .bind(action_type)
        .bind(action_field)
        .bind(action_value)
        .bind(active)
        .bind(priority)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
        n += 1;
    }
    Ok(n)
}

/// Merge translations by natural key `(locale, namespace, msg_key)` — a
/// same-key bundle row updates in place rather than colliding on id. Idempotent
/// and safe into a tenant already carrying its own translations.
async fn restore_translations(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rows: &[Value],
) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        let locale = j_opt_str(r, "locale").unwrap_or_default();
        let namespace = j_opt_str(r, "namespace").unwrap_or_else(|| "ui".to_string());
        let msg_key = j_str(r, "msg_key")?.to_string();
        let value = j_str(r, "value")?.to_string();
        sqlx::query(
            "INSERT INTO meta.md_translation (tenant_id, locale, namespace, msg_key, value)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (tenant_id, locale, namespace, msg_key)
             DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
        )
        .bind(tenant)
        .bind(locale)
        .bind(namespace)
        .bind(msg_key)
        .bind(value)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
        n += 1;
    }
    Ok(n)
}

async fn restore_connectors(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rows: &[Value],
    map: &mut HashMap<Uuid, Uuid>,
) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        let id = j_uuid(r, "id")?;
        let name = j_str(r, "name")?;
        let existing: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM int.connector WHERE tenant_id = $1 AND name = $2")
                .bind(tenant)
                .bind(name)
                .fetch_optional(&mut **tx)
                .await
                .map_err(Error::internal)?;
        let actual = match existing {
            Some((eid,)) => eid,
            None => id,
        };
        map.insert(id, actual);
        let transport = r
            .get("transport")
            .and_then(|x| x.as_str())
            .unwrap_or("http");
        let base_url = j_str(r, "base_url")?;
        let auth = j_value(r, "auth");
        sqlx::query(
            "INSERT INTO int.connector (id, tenant_id, name, transport, base_url, auth)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (id) DO UPDATE SET
                name = EXCLUDED.name, transport = EXCLUDED.transport,
                base_url = EXCLUDED.base_url, auth = EXCLUDED.auth",
        )
        .bind(actual)
        .bind(tenant)
        .bind(name)
        .bind(transport)
        .bind(base_url)
        .bind(auth)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
        n += 1;
    }
    Ok(n)
}

async fn restore_value_maps(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rows: &[Value],
) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        let id = j_uuid(r, "id")?;
        let name = j_str(r, "name")?;
        let entries = j_value(r, "entries");
        let existing: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM int.value_map WHERE tenant_id = $1 AND name = $2")
                .bind(tenant)
                .bind(name)
                .fetch_optional(&mut **tx)
                .await
                .map_err(Error::internal)?;
        let actual = existing.map(|(id,)| id).unwrap_or(id);
        sqlx::query(
            "INSERT INTO int.value_map (id, tenant_id, name, entries)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, entries = EXCLUDED.entries",
        )
        .bind(actual)
        .bind(tenant)
        .bind(name)
        .bind(entries)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
        n += 1;
    }
    Ok(n)
}

async fn restore_flows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rows: &[Value],
    connector_map: &HashMap<Uuid, Uuid>,
    flow_map: &mut HashMap<Uuid, Uuid>,
) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        let id = j_uuid(r, "id")?;
        let name = j_str(r, "name")?;
        let direction = j_str(r, "direction")?;
        let entity = j_str(r, "entity")?;
        // remap connector reference through the connector id map.
        let connector_id = j_opt_uuid(r, "connector_id").map(|c| remap(connector_map, c));
        let webhook_id = j_opt_uuid(r, "webhook_id");
        let endpoint_path: Option<String> = j_opt_str(r, "endpoint_path");
        let mapping = j_value(r, "mapping");
        let external_key_field = r
            .get("external_key_field")
            .and_then(|x| x.as_str())
            .unwrap_or("external_id");
        let conflict_policy = r
            .get("conflict_policy")
            .and_then(|x| x.as_str())
            .unwrap_or("last_write_wins");
        let system: Option<String> = j_opt_str(r, "system");
        let active = j_bool(r, "active", true);
        let running_user_id = j_opt_uuid(r, "running_user_id");

        let existing: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM int.flow WHERE tenant_id = $1 AND name = $2")
                .bind(tenant)
                .bind(name)
                .fetch_optional(&mut **tx)
                .await
                .map_err(Error::internal)?;
        let actual = existing.map(|(id,)| id).unwrap_or(id);
        flow_map.insert(id, actual);

        sqlx::query(
            "INSERT INTO int.flow
                (id, tenant_id, name, direction, entity, connector_id, webhook_id,
                 endpoint_path, mapping, external_key_field, conflict_policy, system,
                 active, running_user_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
             ON CONFLICT (id) DO UPDATE SET
                name = EXCLUDED.name, direction = EXCLUDED.direction, entity = EXCLUDED.entity,
                connector_id = EXCLUDED.connector_id, webhook_id = EXCLUDED.webhook_id,
                endpoint_path = EXCLUDED.endpoint_path, mapping = EXCLUDED.mapping,
                external_key_field = EXCLUDED.external_key_field,
                conflict_policy = EXCLUDED.conflict_policy, system = EXCLUDED.system,
                active = EXCLUDED.active, running_user_id = EXCLUDED.running_user_id",
        )
        .bind(actual)
        .bind(tenant)
        .bind(name)
        .bind(direction)
        .bind(entity)
        .bind(connector_id)
        .bind(webhook_id)
        .bind(endpoint_path)
        .bind(mapping)
        .bind(external_key_field)
        .bind(conflict_policy)
        .bind(system)
        .bind(active)
        .bind(running_user_id)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
        n += 1;
    }
    Ok(n)
}

async fn restore_schedules(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rows: &[Value],
    report_map: &HashMap<Uuid, Uuid>,
    flow_map: &HashMap<Uuid, Uuid>,
) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        let id = j_uuid(r, "id")?;
        let name = j_str(r, "name")?;
        let kind = j_str(r, "kind")?;
        let target_id = j_uuid(r, "target_id")?;
        // remap the scheduled object: a report target → report map, an
        // integration target → flow map; custom targets pass through unchanged.
        let target_id = match kind {
            "report" => remap(report_map, target_id),
            "integration" => remap(flow_map, target_id),
            _ => target_id,
        };
        let cron = j_str(r, "cron")?;
        let enabled = j_bool(r, "enabled", true);
        let running_user_id = j_opt_uuid(r, "running_user_id");
        let existing: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM sys_schedule WHERE tenant_id = $1 AND name = $2")
                .bind(tenant)
                .bind(name)
                .fetch_optional(&mut **tx)
                .await
                .map_err(Error::internal)?;
        let actual = existing.map(|(id,)| id).unwrap_or(id);
        sqlx::query(
            "INSERT INTO sys_schedule
                (id, tenant_id, name, kind, target_id, cron, enabled, running_user_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (id) DO UPDATE SET
                name = EXCLUDED.name, kind = EXCLUDED.kind, target_id = EXCLUDED.target_id,
                cron = EXCLUDED.cron, enabled = EXCLUDED.enabled,
                running_user_id = EXCLUDED.running_user_id",
        )
        .bind(actual)
        .bind(tenant)
        .bind(name)
        .bind(kind)
        .bind(target_id)
        .bind(cron)
        .bind(enabled)
        .bind(running_user_id)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
        n += 1;
    }
    Ok(n)
}

/// Restore sharing rules (ADR-0013). A rule is id-stable security config; the
/// principal (user or team) must already exist in the target tenant — a rule
/// naming a user from the source tenant is skipped (inert, never leaky) since
/// users are deliberately never imported.
async fn restore_share_rules(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rows: &[Value],
) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        let id = j_uuid(r, "id")?;
        let entity = j_str(r, "entity")?;
        let principal = j_uuid(r, "principal_id")?;
        let user: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM sec.sec_user WHERE id = $1")
            .bind(principal)
            .fetch_optional(&mut **tx)
            .await
            .map_err(Error::internal)?;
        let team: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM sec.sec_team WHERE id = $1")
            .bind(principal)
            .fetch_optional(&mut **tx)
            .await
            .map_err(Error::internal)?;
        if user.is_none() && team.is_none() {
            continue; // principal not present in this tenant — skip
        }
        let condition = r.get("condition").cloned().unwrap_or(Value::Null);
        let access = j_str(r, "access")?;
        let active = j_bool(r, "active", true);
        let epoch: i64 = r.get("epoch").and_then(|e| e.as_i64()).unwrap_or(1);
        sqlx::query(
            "INSERT INTO sec.sec_share_rule
                (id, tenant_id, entity, condition, principal_id, access, epoch, active)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (id) DO UPDATE SET entity = EXCLUDED.entity,
                 condition = EXCLUDED.condition, principal_id = EXCLUDED.principal_id,
                 access = EXCLUDED.access, epoch = EXCLUDED.epoch, active = EXCLUDED.active",
        )
        .bind(id)
        .bind(tenant)
        .bind(entity)
        .bind(&condition)
        .bind(principal)
        .bind(access)
        .bind(epoch)
        .bind(active)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
        n += 1;
    }
    Ok(n)
}

/// Restore the role hierarchy, remapping both role ids through the role map
/// (pairs naming a role absent from the bundle+target are skipped). A pair that
/// would close a cycle is rejected — same rule as the admin API's
/// role-hierarchy endpoint (the self-loop skip alone admits longer cycles,
/// which then hang the hierarchy walks).
async fn restore_role_hierarchy(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rows: &[Value],
    role_map: &HashMap<Uuid, Uuid>,
) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        let Some(role) = j_opt_uuid(r, "role_id").map(|id| remap(role_map, id)) else {
            continue;
        };
        let Some(parent) = j_opt_uuid(r, "parent_id").map(|id| remap(role_map, id)) else {
            continue;
        };
        if role == parent {
            continue;
        }
        // Walking UP from the proposed parent must never reach the child.
        let hit: Option<(i32,)> = sqlx::query_as(
            "WITH RECURSIVE up(rid) AS (
                    SELECT $3::uuid
                    UNION
                    SELECT h.parent_id FROM sec.sec_role_hierarchy h JOIN up ON h.role_id = up.rid
                     WHERE h.tenant_id = $1)
             SELECT 1 WHERE $2::uuid IN (SELECT rid FROM up)",
        )
        .bind(tenant)
        .bind(role)
        .bind(parent)
        .fetch_optional(&mut **tx)
        .await
        .map_err(Error::internal)?;
        if hit.is_some() {
            return Err(Error::Invalid(
                "import would create a role-hierarchy cycle (a role is already an ancestor of its proposed parent)"
                    .into(),
            ));
        }
        let res = sqlx::query(
            "INSERT INTO sec.sec_role_hierarchy (tenant_id, role_id, parent_id)
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(tenant)
        .bind(role)
        .bind(parent)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
        n += res.rows_affected() as usize;
    }
    Ok(n)
}

/// Restore md_form / md_view rows (natural key: entity + name).
async fn restore_ui_entity_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rows: &[Value],
    table: &str,
) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        let id = j_uuid(r, "id")?;
        let entity = j_str(r, "entity")?;
        let name = j_str(r, "name")?;
        let label: Option<&str> = r.get("label").and_then(|x| x.as_str());
        let active = j_bool(r, "active", true);
        // column sets differ per table (forms carry `layout`; views carry the
        // list shape) — table is a literal chosen here, never user input
        if table == "meta.md_form" {
            let layout = r.get("layout").cloned().unwrap_or(Value::Null);
            sqlx::query(
                "INSERT INTO meta.md_form (id, tenant_id, entity, name, label, layout, active) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) \
                 ON CONFLICT (tenant_id, entity, name) DO UPDATE SET \
                     label = EXCLUDED.label, layout = EXCLUDED.layout, \
                     active = EXCLUDED.active, updated_at = now()",
            )
            .bind(id)
            .bind(tenant)
            .bind(entity)
            .bind(name)
            .bind(label)
            .bind(&layout)
            .bind(active)
            .execute(&mut **tx)
            .await
            .map_err(Error::internal)?;
        } else {
            let columns = r.get("columns").cloned().unwrap_or(Value::Null);
            let filters = r.get("filters").cloned().unwrap_or(Value::Null);
            let sort = r.get("sort").cloned().unwrap_or(Value::Null);
            let page_size: Option<i32> = r
                .get("page_size")
                .and_then(|x| x.as_i64())
                .map(|v| v as i32);
            sqlx::query(
                "INSERT INTO meta.md_view (id, tenant_id, entity, name, label, columns, filters, sort, page_size, active) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
                 ON CONFLICT (tenant_id, entity, name) DO UPDATE SET \
                     label = EXCLUDED.label, columns = EXCLUDED.columns, \
                     filters = EXCLUDED.filters, sort = EXCLUDED.sort, \
                     page_size = EXCLUDED.page_size, active = EXCLUDED.active, \
                     updated_at = now()",
            )
            .bind(id)
            .bind(tenant)
            .bind(entity)
            .bind(name)
            .bind(label)
            .bind(&columns)
            .bind(&filters)
            .bind(&sort)
            .bind(page_size)
            .bind(active)
            .execute(&mut **tx)
            .await
            .map_err(Error::internal)?;
        }
        n += 1;
    }
    Ok(n)
}

/// Restore dashboards (natural key: name), remapping tile report ids through
/// the report map so a dashboard survives re-import into a tenant whose
/// reports were merged under new ids.
async fn restore_dashboards(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rows: &[Value],
    report_map: &HashMap<Uuid, Uuid>,
) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        let id = j_uuid(r, "id")?;
        let name = j_str(r, "name")?;
        let label: Option<&str> = r.get("label").and_then(|x| x.as_str());
        let active = j_bool(r, "active", true);
        let mut items = r.get("items").cloned().unwrap_or_else(|| json!([]));
        if let Some(tiles) = items.as_array_mut() {
            for tile in tiles {
                if let Some(rid) = tile.get("report_id").and_then(|x| x.as_str()) {
                    if let Ok(parsed) = Uuid::parse_str(rid) {
                        tile["report_id"] = json!(remap(report_map, parsed).to_string());
                    }
                }
            }
        }
        sqlx::query(
            "INSERT INTO meta.md_dashboard (id, tenant_id, name, label, items, active)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (tenant_id, name) DO UPDATE SET
                 label = EXCLUDED.label, items = EXCLUDED.items,
                 active = EXCLUDED.active, updated_at = now()",
        )
        .bind(id)
        .bind(tenant)
        .bind(name)
        .bind(label)
        .bind(&items)
        .bind(active)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
        n += 1;
    }
    Ok(n)
}

/// Restore navigation sets (natural key: name).
async fn restore_navigation(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rows: &[Value],
) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        let id = j_uuid(r, "id")?;
        let name = j_str(r, "name")?;
        let label: Option<&str> = r.get("label").and_then(|x| x.as_str());
        let active = j_bool(r, "active", true);
        let items = r.get("items").cloned().unwrap_or_else(|| json!([]));
        sqlx::query(
            "INSERT INTO meta.md_navigation (id, tenant_id, name, label, items, active)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (tenant_id, name) DO UPDATE SET
                 label = EXCLUDED.label, items = EXCLUDED.items,
                 active = EXCLUDED.active, updated_at = now()",
        )
        .bind(id)
        .bind(tenant)
        .bind(name)
        .bind(label)
        .bind(&items)
        .bind(active)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
        n += 1;
    }
    Ok(n)
}
