//! Generic CRUD + list over the generated `biz.<table>` tables.
//!
//! Record-level security (Phase 3/§5.11 + ADR-0013): a [`RecordScope`] injects
//! an ownership/OWD/sharing predicate into every query (never post-filtering).
//! Composition: owner ∨ manual/rule share (epoch-gated) ∨ team-OWD hierarchy
//! (ADR-0025) ∨ role hierarchy — see [`sharing`] for the materialization side.
//!
//! Writes go to `attributes JSONB` (scalars) + the hoisted reference (FK)
//! columns; GENERATED columns (unique/indexed) populate themselves. Reads use
//! `to_jsonb(t.*)` and reconstruct the record.

use std::collections::HashSet;

use crate::coerce;
use mda_core::{Error, Result};
use mda_meta::EntityDefinition;
use serde_json::{Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

const MAX_PAGE_SIZE: u64 = 200;
const DEFAULT_PAGE_SIZE: u64 = 50;

/// Set the per-transaction tenant context used by the `biz.*` RLS policies
/// (PLAN §5.4 / §5.11). `set_config(..., true)` is transaction-local; without it
/// the policy denies every row (fail-closed).
async fn set_tenant(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, tenant: Uuid) -> Result<()> {
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant.to_string())
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
    Ok(())
}

/// Resolved record-level scope for one request on one entity.
#[derive(Debug, Clone, Copy)]
pub struct RecordScope {
    pub user_id: Uuid,
    pub public_read: bool,
    pub public_write: bool,
    pub bypass: bool,
    /// Team-OWD: when true (and `team_id` is set) the user may read records
    /// owned by members of their team (ADR-0013 `owd_visible` — live, flat;
    /// sub-team hierarchy is the deeper refinement). Write stays owner-only,
    /// mirroring `PublicRead`.
    pub team_owd: bool,
    pub team_id: Option<Uuid>,
}

impl RecordScope {
    /// A superuser scope (no owner filter).
    pub fn superuser(user_id: Uuid) -> Self {
        Self {
            user_id,
            public_read: true,
            public_write: true,
            bypass: true,
            team_owd: false,
            team_id: None,
        }
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct Filter {
    pub field: String,
    pub op: String,
    pub value: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Sort {
    pub field: String,
    pub asc: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ListParams {
    pub filters: Vec<Filter>,
    pub sort: Vec<Sort>,
    pub page: u64,
    pub page_size: u64,
}

#[derive(Debug, serde::Serialize)]
pub struct ListResult {
    pub items: Vec<Value>,
    pub total: u64,
    pub page: u64,
    pub page_size: u64,
}

enum Bind {
    Json(Value),
    Uuid(Option<Uuid>),
    Text(String),
}

enum ListBind {
    Uuid(Uuid),
    Text(String),
}

/// Read-visibility predicate fragment. Uses placeholders `${u}` for the user
/// bind and, when the viewer has a team, `${t}` for the team bind. Composition
/// per ADR-0013 (enforcement layering):
///   owner ∨ manual/rule share ∨ team-OWD ∨ role hierarchy — all **live**.
/// Rule-derived shares carry an **epoch gate**: honored only while the row's
/// epoch equals the rule's current epoch and the rule is active, so bumping
/// `sec_share_rule.epoch` instantly revokes every share materialized under the
/// old epoch (revoke-safe invalidation). Share principals may be a user **or
/// the viewer's team**. The team-OWD clause walks the `sec_team.parent_id`
/// tree DOWNWARD from the viewer's team (ADR-0025), and the role-hierarchy
/// clause walks `sec_role_hierarchy` downward from the viewer's roles ("see
/// records below me", read-only — mirroring the team-OWD write rule).
/// Returns None when the scope grants broad read (bypass / public_read) — no
/// row filter needed.
pub fn read_predicate(s: &RecordScope) -> Option<String> {
    if s.bypass || s.public_read {
        return None;
    }
    let team_principal = if s.team_id.is_some() {
        " OR rs.principal_id = ${t}"
    } else {
        ""
    };
    let team_clause = if s.team_owd && s.team_id.is_some() {
        " OR EXISTS (\
           WITH RECURSIVE descendant_teams(tid) AS (\
                SELECT ${t} \
                UNION ALL \
                SELECT child.id FROM sec.sec_team child \
                  JOIN descendant_teams d ON child.parent_id = d.tid) \
           SELECT 1 FROM sec.sec_user u2 \
           WHERE u2.id = t.owner_id AND u2.tenant_id = t.tenant_id \
             AND u2.team_id IN (SELECT tid FROM descendant_teams))"
    } else {
        ""
    };
    Some(format!(
        "(t.owner_id = ${{u}} \
          OR EXISTS (SELECT 1 FROM sec.sec_record_share rs \
           WHERE rs.tenant_id = t.tenant_id AND rs.record_id = t.id \
             AND (rs.principal_id = ${{u}}{team_principal}) \
             AND rs.access IN ('read','write') \
             AND (rs.rule_id IS NULL \
                  OR rs.epoch = (SELECT r.epoch FROM sec.sec_share_rule r \
                                 WHERE r.id = rs.rule_id AND r.active))){team_clause} \
          OR EXISTS (\
           WITH RECURSIVE sub_roles(rid) AS (\
                SELECT a.role_id FROM sec.sec_role_assignment a \
                  JOIN sec.sec_role r ON r.id = a.role_id AND r.tenant_id = t.tenant_id \
                 WHERE a.user_id = ${{u}} \
                UNION \
                SELECT h.role_id FROM sec.sec_role_hierarchy h \
                  JOIN sub_roles d ON h.parent_id = d.rid \
                 WHERE h.tenant_id = t.tenant_id) \
           SELECT 1 FROM sec.sec_user o \
             JOIN sec.sec_role_assignment oa ON oa.user_id = o.id \
            WHERE o.id = t.owner_id AND o.tenant_id = t.tenant_id \
              AND oa.role_id IN (SELECT rid FROM sub_roles)))"
    ))
}

/// Write predicate: owner OR shared-with-write (manual or rule-derived under
/// the same epoch gate; the principal may be the viewer's team). Team-OWD and
/// role hierarchy grant **read only** (mirroring `PublicRead`), so neither adds
/// a write clause.
pub fn write_predicate(s: &RecordScope) -> Option<String> {
    if s.bypass || s.public_write {
        None
    } else {
        let team_principal = if s.team_id.is_some() {
            " OR rs.principal_id = ${t}"
        } else {
            ""
        };
        Some(
            "(t.owner_id = ${u} OR EXISTS (SELECT 1 FROM sec.sec_record_share rs \
              WHERE rs.tenant_id = t.tenant_id AND rs.record_id = t.id \
                AND (rs.principal_id = ${u}"
                .to_string()
                + team_principal
                + ") \
                AND rs.access = 'write' \
                AND (rs.rule_id IS NULL \
                     OR rs.epoch = (SELECT r.epoch FROM sec.sec_share_rule r \
                                    WHERE r.id = rs.rule_id AND r.active))))",
        )
    }
}

/// Substitute a predicate fragment's `{u}` (and optional `{t}`) placeholders
/// with absolute `$n` indices starting at `start`, returning the rendered SQL
/// and the ordered Uuid bind values. The team bind is added only when the
/// fragment carries a `{t}` placeholder and the scope has a team.
pub fn pred_render(pred: &str, scope: &RecordScope, start: usize) -> (String, Vec<Uuid>) {
    let mut binds = vec![scope.user_id];
    let mut out = pred.replace("{u}", &start.to_string());
    if out.contains("{t}") {
        if let Some(team) = scope.team_id {
            out = out.replace("{t}", &(start + 1).to_string());
            binds.push(team);
        }
    }
    (out, binds)
}

// ===== create =====

pub async fn create(
    pool: &PgPool,
    tenant: Uuid,
    def: &EntityDefinition,
    body: Map<String, Value>,
    owner: Uuid,
) -> Result<Value> {
    ensure_active(def)?;
    validate_record(def, &body)?;

    let mut attributes = Map::new();
    for f in &def.fields {
        let raw = body.get(&f.name).cloned();
        let raw = match (raw, &f.default_expr) {
            (Some(v), _) => Some(v),
            (None, Some(d)) if !is_expr_marker(d) => Some(d.clone()),
            _ => None,
        };
        match coerce::coerce(&f.field_type, raw)? {
            Some(v) => {
                attributes.insert(f.name.clone(), v);
            }
            None => {
                if f.required && f.field_type != "auto_number" {
                    return Err(Error::Invalid(format!("field {} is required", f.name)));
                }
            }
        }
    }

    let mut fk_cols: Vec<&str> = Vec::new();
    let mut fk_values: Vec<Option<Uuid>> = Vec::new();
    for r in &def.relationships {
        fk_cols.push(r.source_field_name.as_str());
        fk_values.push(uuid_or_null(
            body.get(&r.source_field_name),
            &r.source_field_name,
        )?);
        if r.required && fk_values.last().unwrap().is_none() {
            return Err(Error::Invalid(format!(
                "field {} is required",
                r.source_field_name
            )));
        }
    }

    let id = Uuid::from(mda_core::Id::new());

    // Run under the tenant GUC so the biz.<t> RLS WITH CHECK passes, and keep
    // the gapless auto_number UPSERT atomic with the INSERT.
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, tenant).await?;
    for f in &def.fields {
        if f.field_type == "auto_number" && !attributes.contains_key(&f.name) {
            let (n,): (i64,) = sqlx::query_as(
                "INSERT INTO meta.md_sequence (tenant_id, entity_id, field_name, next)
                 VALUES ($1, $2, $3, 1)
                 ON CONFLICT (tenant_id, entity_id, field_name)
                 DO UPDATE SET next = meta.md_sequence.next + 1
                 RETURNING next",
            )
            .bind(tenant)
            .bind(def.entity.id)
            .bind(&f.name)
            .fetch_one(&mut *tx)
            .await
            .map_err(Error::internal)?;
            attributes.insert(f.name.clone(), Value::from(n));
        }
    }

    let attrs_json = Value::Object(attributes);
    let mut cols: Vec<&str> = vec!["id", "tenant_id", "owner_id", "attributes"];
    cols.extend(fk_cols.iter().copied());
    let placeholders: Vec<String> = (1..=cols.len()).map(|i| format!("${i}")).collect();
    let sql = format!(
        "INSERT INTO biz.{} ({}) VALUES ({})",
        def.entity.table_name,
        cols.join(", "),
        placeholders.join(", ")
    );
    let mut q = sqlx::query(&sql)
        .bind(id)
        .bind(tenant)
        .bind(owner)
        .bind(attrs_json);
    for v in fk_values {
        q = q.bind(v);
    }
    q.execute(&mut *tx).await.map_err(Error::internal)?;
    // ADR-0013: materialize this record's criteria-sharing-rule grants
    // synchronously in the write transaction (the per-record recompute step).
    let (doc,): (Value,) = sqlx::query_as(&format!(
        "SELECT to_jsonb(t.*) FROM biz.{} t WHERE t.id = $1",
        def.entity.table_name
    ))
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::internal)?;
    crate::sharing::recompute_record(
        &mut tx,
        tenant,
        &def.entity.name,
        id,
        &reconstruct(def, doc),
    )
    .await?;
    tx.commit().await.map_err(Error::internal)?;

    read(pool, tenant, def, id, &RecordScope::superuser(owner)).await
}

// ===== read =====

pub async fn read(
    pool: &PgPool,
    tenant: Uuid,
    def: &EntityDefinition,
    id: Uuid,
    scope: &RecordScope,
) -> Result<Value> {
    ensure_active(def)?;
    let rp = read_predicate(scope);
    let mut sql = format!(
        "SELECT to_jsonb(t.*) AS doc FROM biz.{} t WHERE id = $1 AND tenant_id = $2",
        def.entity.table_name
    );
    let mut rp_binds: Vec<Uuid> = Vec::new();
    if let Some(ref p) = &rp {
        let (frag, binds) = pred_render(p, scope, 3);
        sql.push_str(&format!(" AND {}", frag));
        rp_binds = binds;
    }
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, tenant).await?;
    let mut q = sqlx::query_as::<_, (Value,)>(sql.as_str())
        .bind(id)
        .bind(tenant);
    for b in rp_binds {
        q = q.bind(b);
    }
    let row = q.fetch_optional(&mut *tx).await.map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    row.map(|(v,)| reconstruct(def, v))
        .ok_or_else(|| Error::NotFound(format!("record {id}")))
}

// ===== update (OCC + write scope) =====

#[allow(clippy::too_many_arguments)]
pub async fn update(
    pool: &PgPool,
    tenant: Uuid,
    def: &EntityDefinition,
    id: Uuid,
    expected_version: i64,
    body: Map<String, Value>,
    scope: &RecordScope,
    new_state: Option<String>,
) -> Result<Value> {
    ensure_active(def)?;
    validate_record(def, &body)?;

    let mut attributes_merge = Map::new();
    for f in &def.fields {
        if let Some(v) = body.get(&f.name) {
            if !v.is_null() {
                if let Some(cv) = coerce::coerce(&f.field_type, Some(v.clone()))? {
                    attributes_merge.insert(f.name.clone(), cv);
                }
            }
        }
    }

    let mut binds: Vec<Bind> = Vec::new();
    let mut sets: Vec<String> = vec!["version = version + 1".into(), "updated_at = now()".into()];
    if let Some(st) = new_state {
        sets.push(format!("state = ${}", binds.len() + 1));
        binds.push(Bind::Text(st));
    }
    if !attributes_merge.is_empty() {
        sets.push(format!("attributes = attributes || ${}", binds.len() + 1));
        binds.push(Bind::Json(Value::Object(attributes_merge)));
    }
    for r in &def.relationships {
        if let Some(v) = body.get(&r.source_field_name) {
            let u = uuid_or_null(Some(v), &r.source_field_name)?;
            if r.required && u.is_none() {
                return Err(Error::Invalid(format!(
                    "field {} is required",
                    r.source_field_name
                )));
            }
            sets.push(format!("{} = ${}", r.source_field_name, binds.len() + 1));
            binds.push(Bind::Uuid(u));
        }
    }
    let rp = write_predicate(scope);
    let mut where_parts = vec![
        format!("t.id = ${}", binds.len() + 1),
        format!("t.tenant_id = ${}", binds.len() + 2),
        format!("t.version = ${}", binds.len() + 3),
    ];
    let mut rp_binds: Vec<Uuid> = Vec::new();
    if let Some(ref p) = &rp {
        let (frag, pb) = pred_render(p, scope, binds.len() + 4);
        where_parts.push(frag);
        rp_binds = pb;
    }
    let sql = format!(
        "UPDATE biz.{} AS t SET {} WHERE {} RETURNING to_jsonb(t.*) AS doc",
        def.entity.table_name,
        sets.join(", "),
        where_parts.join(" AND ")
    );

    let mut q = sqlx::query_as::<_, (Value,)>(sql.as_str());
    for b in binds {
        q = match b {
            Bind::Json(v) => q.bind(v),
            Bind::Uuid(v) => q.bind(v),
            Bind::Text(v) => q.bind(v),
        };
    }
    q = q.bind(id).bind(tenant).bind(expected_version);
    for b in &rp_binds {
        q = q.bind(*b);
    }

    let mut tx = pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, tenant).await?;
    let result = match q.fetch_optional(&mut *tx).await.map_err(Error::internal)? {
        Some((doc,)) => {
            let rec = reconstruct(def, doc);
            // ADR-0013: synchronous per-record share recompute — inside the
            // write transaction, so a record's own shares are always fresh
            // immediately after its write (no per-record revocation lag).
            crate::sharing::recompute_record(&mut tx, tenant, &def.entity.name, id, &rec).await?;
            Ok(rec)
        }
        None => distinguish_not_found_or_conflict(&mut tx, def, tenant, id, scope).await,
    };
    tx.commit().await.map_err(Error::internal)?;
    result
}

async fn distinguish_not_found_or_conflict(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    def: &EntityDefinition,
    tenant: Uuid,
    id: Uuid,
    scope: &RecordScope,
) -> Result<Value> {
    // exists at all (ignoring scope)?
    let exists: Option<(i32,)> = sqlx::query_as(&format!(
        "SELECT 1 FROM biz.{} t WHERE t.id = $1 AND t.tenant_id = $2",
        def.entity.table_name
    ))
    .bind(id)
    .bind(tenant)
    .fetch_optional(&mut **tx)
    .await
    .map_err(Error::internal)?;
    match exists {
        None => Err(Error::NotFound(format!("record {id}"))),
        Some(_) => {
            let rp = write_predicate(scope);
            let mut sql = format!(
                "SELECT 1 FROM biz.{} t WHERE t.id = $1 AND t.tenant_id = $2",
                def.entity.table_name
            );
            let mut rp_binds: Vec<Uuid> = Vec::new();
            if let Some(ref p) = &rp {
                let (frag, pb) = pred_render(p, scope, 3);
                sql.push_str(&format!(" AND {}", frag));
                rp_binds = pb;
            }
            let mut q = sqlx::query_as::<_, (i32,)>(sql.as_str())
                .bind(id)
                .bind(tenant);
            for b in rp_binds {
                q = q.bind(b);
            }
            let writable: Option<(i32,)> =
                q.fetch_optional(&mut **tx).await.map_err(Error::internal)?;
            if writable.is_none() {
                Err(Error::NotFound(format!("record {id}"))) // not visible/writable -> 404 (no leak)
            } else {
                Err(Error::Conflict(
                    "version mismatch — record was modified by another writer".into(),
                ))
            }
        }
    }
}

// ===== delete (write scope) =====

pub async fn delete(
    pool: &PgPool,
    tenant: Uuid,
    def: &EntityDefinition,
    id: Uuid,
    scope: &RecordScope,
) -> Result<()> {
    ensure_active(def)?;
    let rp = write_predicate(scope);
    let mut sql = format!(
        "DELETE FROM biz.{} AS t WHERE t.id = $1 AND t.tenant_id = $2",
        def.entity.table_name
    );
    let mut rp_binds: Vec<Uuid> = Vec::new();
    if let Some(ref p) = &rp {
        let (frag, pb) = pred_render(p, scope, 3);
        sql.push_str(&format!(" AND {}", frag));
        rp_binds = pb;
    }
    let mut q = sqlx::query(&sql).bind(id).bind(tenant);
    for b in &rp_binds {
        q = q.bind(*b);
    }
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, tenant).await?;
    let res = q.execute(&mut *tx).await.map_err(Error::internal)?;
    if res.rows_affected() > 0 {
        // Hard delete (the row is archived by trigger, ADR-0006): drop the
        // materialized visibility rows too — sec_record_share has no FK to
        // dynamic biz.* tables, so cleanup is explicit.
        crate::sharing::drop_record_shares(&mut tx, tenant, id).await?;
    }
    tx.commit().await.map_err(Error::internal)?;
    if res.rows_affected() == 0 {
        return Err(Error::NotFound(format!("record {id}")));
    }
    Ok(())
}

// ===== list (read scope) =====

pub async fn list(
    pool: &PgPool,
    tenant: Uuid,
    def: &EntityDefinition,
    params: &ListParams,
    scope: &RecordScope,
) -> Result<ListResult> {
    ensure_active(def)?;
    let page = params.page.max(1);
    let page_size = if params.page_size == 0 {
        DEFAULT_PAGE_SIZE
    } else {
        params.page_size.clamp(1, MAX_PAGE_SIZE)
    };
    let offset = (page - 1) * page_size;

    let (where_sql, mut binds) = build_list_where(def, tenant, &params.filters)?;
    let rp = read_predicate(scope);
    let mut where_sql = where_sql;
    if let Some(p) = rp {
        let n = binds.len() + 1;
        let (frag, pb) = pred_render(&p, scope, n);
        where_sql.push_str(&format!(" AND {}", frag));
        for b in pb {
            binds.push(ListBind::Uuid(b));
        }
    }
    let order_sql = build_order(def, &params.sort);
    let table = &def.entity.table_name;

    // count
    let count_sql = format!("SELECT count(*)::int8 FROM biz.{table} t WHERE {where_sql}");
    let mut cq = sqlx::query_scalar::<_, i64>(count_sql.as_str());
    for b in &binds {
        cq = match b {
            ListBind::Uuid(v) => cq.bind(*v),
            ListBind::Text(v) => cq.bind(v.clone()),
        };
    }
    // page
    let list_sql = format!(
        "SELECT to_jsonb(t.*) AS doc FROM biz.{table} t WHERE {where_sql} ORDER BY {order_sql} \
         LIMIT ${lim} OFFSET ${off}",
        lim = binds.len() + 1,
        off = binds.len() + 2
    );
    let mut q = sqlx::query_as::<_, (Value,)>(list_sql.as_str());
    for b in &binds {
        q = match b {
            ListBind::Uuid(v) => q.bind(*v),
            ListBind::Text(v) => q.bind(v.clone()),
        };
    }
    q = q.bind(page_size as i64).bind(offset as i64);

    let mut tx = pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, tenant).await?;
    let total = cq.fetch_one(&mut *tx).await.map_err(Error::internal)?;
    let rows = q.fetch_all(&mut *tx).await.map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    let items = rows.into_iter().map(|(v,)| reconstruct(def, v)).collect();
    Ok(ListResult {
        items,
        total: total as u64,
        page,
        page_size,
    })
}

/// Resolve the IDs of records a writer may touch for a **mass action**
/// (PLAN §9 deferral — bulk update/delete by filter, distinct from the
/// §5.13 file import). Unlike [`list`] (read-scope), this injects the **write**
/// predicate so a mass update/delete can never reach a record the caller may
/// not write — exactly the same scope a single-record PATCH/DELETE enforces.
/// The result is capped at `limit` (the API layer bounds it) and ordered by id
/// for deterministic, resumable processing of large result sets.
pub async fn mass_target_ids(
    pool: &PgPool,
    tenant: Uuid,
    def: &EntityDefinition,
    params: &ListParams,
    scope: &RecordScope,
    limit: u64,
) -> Result<Vec<Uuid>> {
    ensure_active(def)?;
    let (mut where_sql, mut binds) = build_list_where(def, tenant, &params.filters)?;
    let rp = write_predicate(scope);
    if let Some(p) = rp {
        let n = binds.len() + 1;
        let (frag, pb) = pred_render(&p, scope, n);
        where_sql.push_str(&format!(" AND {}", frag));
        for b in pb {
            binds.push(ListBind::Uuid(b));
        }
    }
    let table = &def.entity.table_name;
    let lim = binds.len() + 1;
    let sql =
        format!("SELECT t.id FROM biz.{table} t WHERE {where_sql} ORDER BY t.id LIMIT ${lim}");
    let mut q = sqlx::query_scalar::<_, Uuid>(sql.as_str());
    for b in &binds {
        q = match b {
            ListBind::Uuid(v) => q.bind(*v),
            ListBind::Text(v) => q.bind(v.clone()),
        };
    }
    q = q.bind(limit as i64);
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, tenant).await?;
    let ids = q.fetch_all(&mut *tx).await.map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(ids)
}

fn build_list_where(
    def: &EntityDefinition,
    tenant: Uuid,
    filters: &[Filter],
) -> Result<(String, Vec<ListBind>)> {
    let mut parts: Vec<String> = vec!["tenant_id = $1".to_string()];
    let mut binds: Vec<ListBind> = vec![ListBind::Uuid(tenant)];
    let scalar: HashSet<&str> = def.fields.iter().map(|f| f.name.as_str()).collect();
    let fk: HashSet<&str> = def
        .relationships
        .iter()
        .map(|r| r.source_field_name.as_str())
        .collect();
    let core = ["state", "version", "created_at", "updated_at", "owner_id"];
    for f in filters {
        let op = match f.op.as_str() {
            "eq" => "=",
            "ne" => "<>",
            "gt" => ">",
            "gte" => ">=",
            "lt" => "<",
            "lte" => "<=",
            "like" => "ILIKE",
            other => return Err(Error::Invalid(format!("unsupported filter op {other}"))),
        };
        let (lhs, rhs_cast) = if fk.contains(f.field.as_str()) {
            (f.field.clone(), "::uuid")
        } else if scalar.contains(f.field.as_str()) {
            if matches!(f.op.as_str(), "gt" | "gte" | "lt" | "lte") {
                (format!("(attributes->>'{}')", f.field), "::numeric")
            } else {
                (format!("(attributes->>'{}')", f.field), "")
            }
        } else if core.contains(&f.field.as_str()) {
            (f.field.clone(), "")
        } else {
            return Err(Error::Invalid(format!("unknown filter field {}", f.field)));
        };
        parts.push(format!("{lhs} {op} ${}{rhs_cast}", binds.len() + 1));
        binds.push(ListBind::Text(f.value.clone()));
    }
    Ok((parts.join(" AND "), binds))
}

fn build_order(def: &EntityDefinition, sort: &[Sort]) -> String {
    if sort.is_empty() {
        return "id".to_string();
    }
    let scalar: HashSet<&str> = def.fields.iter().map(|f| f.name.as_str()).collect();
    // Numeric-typed scalars must sort numerically, not lexically (else "10" < "2").
    let numeric: HashSet<&str> = def
        .fields
        .iter()
        .filter(|f| {
            matches!(
                f.field_type.as_str(),
                "integer" | "auto_number" | "decimal" | "money"
            )
        })
        .map(|f| f.name.as_str())
        .collect();
    let fk: HashSet<&str> = def
        .relationships
        .iter()
        .map(|r| r.source_field_name.as_str())
        .collect();
    let core = [
        "state",
        "version",
        "created_at",
        "updated_at",
        "id",
        "owner_id",
    ];
    let mut parts: Vec<String> = Vec::new();
    for s in sort {
        if s.field == "id" || core.contains(&s.field.as_str()) || fk.contains(s.field.as_str()) {
            parts.push(format!("{} {}", s.field, dir(s.asc)));
        } else if scalar.contains(s.field.as_str()) {
            let expr = if numeric.contains(s.field.as_str()) {
                format!("(attributes->>'{}')::numeric", s.field)
            } else {
                format!("(attributes->>'{}')", s.field)
            };
            parts.push(format!("{expr} {}", dir(s.asc)));
        }
    }
    if parts.is_empty() {
        "id".to_string()
    } else {
        parts.join(", ")
    }
}

fn dir(asc: bool) -> &'static str {
    if asc {
        "ASC"
    } else {
        "DESC"
    }
}

// ===== helpers =====

fn ensure_active(def: &EntityDefinition) -> Result<()> {
    if def.entity.status != "active" {
        return Err(Error::NotFound(format!(
            "entity {} is retired",
            def.entity.name
        )));
    }
    Ok(())
}

/// Accumulate *all* field-level validation problems for a record body and return
/// them as a single [`Error::Validation`] (ADR-0018 per-field `details`). Covers
/// unknown fields, missing required fields, and per-field type/reference shape —
/// so a client renders every error in one round trip instead of fail-then-retry
/// per field. `auto_number` fields are never required (generated at write time).
pub fn validate_record(def: &EntityDefinition, body: &Map<String, Value>) -> Result<()> {
    use mda_core::FieldError;

    let known: HashSet<&str> = def
        .fields
        .iter()
        .map(|f| f.name.as_str())
        .chain(
            def.relationships
                .iter()
                .map(|r| r.source_field_name.as_str()),
        )
        .collect();

    let mut errs: Vec<FieldError> = Vec::new();

    for k in body.keys() {
        if !known.contains(k.as_str()) {
            errs.push(FieldError::new(
                k.as_str(),
                "mda.unknown_field",
                format!("unknown field {k}"),
            ));
        }
    }

    for f in &def.fields {
        let raw = body.get(&f.name).cloned();
        let raw = match (raw, &f.default_expr) {
            (Some(v), _) => Some(v),
            (None, Some(d)) if !is_expr_marker(d) => Some(d.clone()),
            _ => None,
        };
        match raw {
            None => {
                if f.required && f.field_type != "auto_number" {
                    errs.push(FieldError::new(
                        &f.name,
                        "mda.required",
                        format!("field {} is required", f.name),
                    ));
                }
            }
            Some(v) if !v.is_null() => {
                if let Err(e) = coerce::coerce(&f.field_type, Some(v)) {
                    errs.push(FieldError::new(&f.name, "mda.invalid_type", e.to_string()));
                }
            }
            _ => {}
        }
    }

    for r in &def.relationships {
        let v = body.get(&r.source_field_name);
        if matches!(v, None | Some(Value::Null)) {
            if r.required {
                errs.push(FieldError::new(
                    &r.source_field_name,
                    "mda.required",
                    format!("field {} is required", r.source_field_name),
                ));
            }
        } else if let Err(e) = uuid_or_null(v, &r.source_field_name) {
            errs.push(FieldError::new(
                &r.source_field_name,
                "mda.invalid_reference",
                e.to_string(),
            ));
        }
    }

    if errs.is_empty() {
        Ok(())
    } else {
        Err(Error::Validation {
            message: format!("{} validation problem(s)", errs.len()),
            fields: errs,
        })
    }
}

fn uuid_or_null(v: Option<&Value>, field: &str) -> Result<Option<Uuid>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Uuid::parse_str(s)
            .map(Some)
            .map_err(|_| Error::Invalid(format!("{field} is not a valid UUID"))),
        Some(_) => Err(Error::Invalid(format!("{field} must be a UUID string"))),
    }
}

// ===== restore from archive (ADR-0006 / ADR-0015) =====
//
// Hard delete moves the row to biz_archive.<table> via the BEFORE DELETE
// trigger. Restore re-inserts the most recently archived copy of one record
// with a higher `version` (so any stale client hits a clean 409, §5.9) and
// created_at preserved. Single-record scope here; batch/cascade restore (one
// click for a whole cascade tree) is the full ADR-0015 design and a follow-up.
pub async fn restore(
    pool: &PgPool,
    tenant: Uuid,
    def: &EntityDefinition,
    id: Uuid,
) -> Result<Value> {
    ensure_active(def)?;
    // core + attributes + FK columns (same set as create; generated columns are
    // excluded — they regenerate from `attributes` on insert).
    let table = &def.entity.table_name;
    let fk = def
        .relationships
        .iter()
        .map(|r| r.source_field_name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT INTO biz.{table} AS t \
            (id, tenant_id, owner_id, state, version, created_at, updated_at, attributes{fk_head}) \
         SELECT id, tenant_id, owner_id, state, version + 1, created_at, now(), attributes{fk_head} \
         FROM biz_archive.{table} a \
         WHERE a.id = $1 AND a.tenant_id = $2 \
         ORDER BY a.archived_at DESC \
         LIMIT 1 \
         RETURNING to_jsonb(t.*) AS doc",
        fk_head = if fk.is_empty() {
            String::new()
        } else {
            format!(", {fk}")
        }
    );
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, tenant).await?;
    let row: Option<(Value,)> = sqlx::query_as(&sql)
        .bind(id)
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Error::internal)?;
    if let Some((doc,)) = &row {
        // Restoring a record re-materializes its rule-derived visibility
        // (ADR-0013) — the archived row's shares were dropped at delete time.
        crate::sharing::recompute_record(
            &mut tx,
            tenant,
            &def.entity.name,
            id,
            &reconstruct(def, doc.clone()),
        )
        .await?;
    }
    tx.commit().await.map_err(Error::internal)?;
    let (doc,) = row.ok_or_else(|| Error::NotFound(format!("archived record {id}")))?;
    Ok(reconstruct(def, doc))
}

/// Flatten a `to_jsonb(t.*)` row into the record shape (system columns +
/// attributes + FK columns) that rules / sharing conditions evaluate against.
pub fn reconstruct(def: &EntityDefinition, doc: Value) -> Value {
    let Some(obj) = doc.as_object() else {
        return doc;
    };
    let mut out = Map::new();
    for c in [
        "id",
        "version",
        "owner_id",
        "state",
        "created_at",
        "updated_at",
    ] {
        if let Some(v) = obj.get(c) {
            out.insert(c.into(), v.clone());
        }
    }
    if let Some(Value::Object(attrs)) = obj.get("attributes") {
        for f in &def.fields {
            if let Some(v) = attrs.get(&f.name) {
                out.insert(f.name.clone(), v.clone());
            }
        }
    }
    for r in &def.relationships {
        if let Some(v) = obj.get(&r.source_field_name) {
            out.insert(r.source_field_name.clone(), v.clone());
        }
    }
    Value::Object(out)
}

fn is_expr_marker(v: &Value) -> bool {
    matches!(v, Value::Object(m) if m.contains_key("$expr"))
}

/// Next value for an `auto_number` field (gapless within the txn via row lock).
pub async fn next_sequence(
    pool: &PgPool,
    tenant: Uuid,
    entity_id: Uuid,
    field: &str,
) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO meta.md_sequence (tenant_id, entity_id, field_name, next)
         VALUES ($1, $2, $3, 1)
         ON CONFLICT (tenant_id, entity_id, field_name)
         DO UPDATE SET next = meta.md_sequence.next + 1
         RETURNING next",
    )
    .bind(tenant)
    .bind(entity_id)
    .bind(field)
    .fetch_one(pool)
    .await
    .map_err(Error::internal)?;
    Ok(row.0)
}
