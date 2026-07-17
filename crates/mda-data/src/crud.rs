//! Generic CRUD + list over the generated `biz.<table>` tables.
//!
//! Record-level security (Phase 3): a [`RecordScope`] injects an ownership/OWD
//! predicate into every query (never post-filtering — §5.11). Team-OWD and
//! criteria-based sharing arrive in Phase 6 (ADR-0013); here: owner + public OWD.
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

/// Resolved record-level scope for one request on one entity.
#[derive(Debug, Clone, Copy)]
pub struct RecordScope {
    pub user_id: Uuid,
    pub public_read: bool,
    pub public_write: bool,
    pub bypass: bool,
}

impl RecordScope {
    /// A superuser scope (no owner filter).
    pub fn superuser(user_id: Uuid) -> Self {
        Self {
            user_id,
            public_read: true,
            public_write: true,
            bypass: true,
        }
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Filter {
    pub field: String,
    pub op: String,
    pub value: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
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

/// Read-visibility predicate (with a `{u}` placeholder for the user bind).
/// owner OR public already handled by caller; here: owner OR shared-with-me.
/// Returns None when the scope grants broad read (bypass / public_read).
fn read_predicate(s: &RecordScope) -> Option<String> {
    if s.bypass || s.public_read {
        None
    } else {
        Some(
            "(t.owner_id = ${u} OR EXISTS (SELECT 1 FROM sec.sec_record_share rs \
              WHERE rs.tenant_id = t.tenant_id AND rs.record_id = t.id \
                AND rs.principal_id = ${u} AND rs.access IN ('read','write')))"
                .to_string(),
        )
    }
}

/// Write predicate: owner OR shared-with-write-access.
fn write_predicate(s: &RecordScope) -> Option<String> {
    if s.bypass || s.public_write {
        None
    } else {
        Some(
            "(t.owner_id = ${u} OR EXISTS (SELECT 1 FROM sec.sec_record_share rs \
              WHERE rs.tenant_id = t.tenant_id AND rs.record_id = t.id \
                AND rs.principal_id = ${u} AND rs.access = 'write'))"
                .to_string(),
        )
    }
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
    validate_known_keys(def, &body)?;

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
    for f in &def.fields {
        if f.field_type == "auto_number" && !attributes.contains_key(&f.name) {
            let n = next_sequence(pool, tenant, def.entity.id, &f.name).await?;
            attributes.insert(f.name.clone(), Value::from(n));
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
    q.execute(pool).await.map_err(Error::internal)?;

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
    if let Some(ref p) = &rp {
        sql.push_str(&format!(" AND {}", p.replace("{u}", "3")));
    }
    let mut q = sqlx::query_as::<_, (Value,)>(sql.as_str())
        .bind(id)
        .bind(tenant);
    if rp.is_some() {
        q = q.bind(scope.user_id);
    }
    q.fetch_optional(pool)
        .await
        .map_err(Error::internal)?
        .map(|(v,)| reconstruct(def, v))
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
    validate_known_keys(def, &body)?;

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
    if let Some(ref p) = &rp {
        let idx = binds.len() + 4;
        where_parts.push(p.replace("{u}", &idx.to_string()));
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
    if rp.is_some() {
        q = q.bind(scope.user_id);
    }

    match q.fetch_optional(pool).await.map_err(Error::internal)? {
        Some((doc,)) => Ok(reconstruct(def, doc)),
        None => distinguish_not_found_or_conflict(pool, def, tenant, id, scope).await,
    }
}

async fn distinguish_not_found_or_conflict(
    pool: &PgPool,
    def: &EntityDefinition,
    tenant: Uuid,
    id: Uuid,
    scope: &RecordScope,
) -> Result<Value> {
    // exists at all (ignoring scope)?
    let exists: Option<(i32,)> = sqlx::query_as(&format!(
        "SELECT 1 FROM biz.{} WHERE id = $1 AND tenant_id = $2",
        def.entity.table_name
    ))
    .bind(id)
    .bind(tenant)
    .fetch_optional(pool)
    .await
    .map_err(Error::internal)?;
    match exists {
        None => Err(Error::NotFound(format!("record {id}"))),
        Some(_) => {
            let rp = write_predicate(scope);
            let mut sql = format!(
                "SELECT 1 FROM biz.{} WHERE id = $1 AND tenant_id = $2",
                def.entity.table_name
            );
            if let Some(ref p) = &rp {
                sql.push_str(&format!(" AND {}", p.replace("{u}", "3")));
            }
            let mut q = sqlx::query_as::<_, (i32,)>(sql.as_str())
                .bind(id)
                .bind(tenant);
            if rp.is_some() {
                q = q.bind(scope.user_id);
            }
            let writable: Option<(i32,)> = q.fetch_optional(pool).await.map_err(Error::internal)?;
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
        "DELETE FROM biz.{} WHERE id = $1 AND tenant_id = $2",
        def.entity.table_name
    );
    if let Some(ref p) = &rp {
        sql.push_str(&format!(" AND {}", p.replace("{u}", "3")));
    }
    let mut q = sqlx::query(&sql).bind(id).bind(tenant);
    if rp.is_some() {
        q = q.bind(scope.user_id);
    }
    let res = q.execute(pool).await.map_err(Error::internal)?;
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
        where_sql.push_str(&format!(" AND {}", p.replace("{u}", &n.to_string())));
        binds.push(ListBind::Uuid(scope.user_id));
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
    let total = cq.fetch_one(pool).await.map_err(Error::internal)?;

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
    let rows = q.fetch_all(pool).await.map_err(Error::internal)?;
    let items = rows.into_iter().map(|(v,)| reconstruct(def, v)).collect();
    Ok(ListResult {
        items,
        total: total as u64,
        page,
        page_size,
    })
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

fn validate_known_keys(def: &EntityDefinition, body: &Map<String, Value>) -> Result<()> {
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
    for k in body.keys() {
        if !known.contains(k.as_str()) {
            return Err(Error::Invalid(format!("unknown field {k}")));
        }
    }
    Ok(())
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

fn reconstruct(def: &EntityDefinition, doc: Value) -> Value {
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
