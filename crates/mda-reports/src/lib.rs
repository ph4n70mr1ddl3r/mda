//! `mda-reports` — structured reporting (PLAN §5.17).
//!
//! A report dataset is a structured declaration (base entity + select fields +
//! filters + group_by + order_by + limit) that the engine compiles to
//! parameterized SQL over `biz.<table>`. Because the engine builds the SQL, it
//! enforces the **runner's** object/field/record security by construction:
//!  - object: needs `read` on the base entity;
//!  - field (projection): unreadable select fields are dropped;
//!  - field (semantic): an unreadable field in `filter`/`group_by`/`order_by` is
//!    a run-time error (a dropped filter/group would change semantics / leak);
//!  - record: the runner's ownership/OWD predicate is injected into the WHERE.
//!
//! Phase 7 supports single-entity reports (reference-traversal joins, scheduled
//! delivery, and PDF/XLSX renderers are follow-ups).

use std::collections::HashSet;

use mda_core::{Error, Result};
use mda_data::{Filter, RecordScope, Sort};
use mda_meta::{loader, EntityDefinition};
use mda_security::{Access, Identity, Owd};
use serde::Deserialize;
use serde_json::{Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

pub mod template;

pub use template::{render, render_body, Rendered, Template};

/// Wall-clock cap (ms) on a single synchronous report run (§5.17 cost control).
/// Overruns are killed by Postgres and surface as an internal error; large
/// reports must run async as a job (a follow-up).
const REPORT_TIMEOUT_MS: &str = "10000";

/// A structured report dataset (the JSON stored in `md_report.dataset`).
#[derive(Debug, Clone, Deserialize)]
pub struct Dataset {
    pub base_entity: String,
    #[serde(default)]
    pub fields: Vec<SelectField>,
    #[serde(default)]
    pub filters: Vec<Filter>,
    #[serde(default)]
    pub group_by: Vec<String>,
    #[serde(default)]
    pub order_by: Vec<Sort>,
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SelectField {
    pub field: String,
    /// count | sum | avg | min | max ; None => plain field.
    #[serde(default)]
    pub aggregate: Option<String>,
    #[serde(default)]
    pub alias: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct ReportResult {
    pub columns: Vec<String>,
    pub rows: Vec<Map<String, Value>>,
}

/// Compile and run a dataset under the runner's identity.
pub async fn run(pool: &PgPool, identity: &Identity, ds: &Dataset) -> Result<ReportResult> {
    // object grain
    if !identity.can(&ds.base_entity, "read") {
        return Err(Error::Forbidden(format!(
            "missing read on {}",
            ds.base_entity
        )));
    }

    let entity_id = loader::entity_id_by_name(pool, identity.tenant_id, &ds.base_entity).await?;
    let def = loader::load_entity_definition(pool, identity.tenant_id, entity_id).await?;
    if def.entity.status != "active" {
        return Err(Error::NotFound(format!(
            "entity {} is retired",
            ds.base_entity
        )));
    }

    let owd = mda_security::resolve_owd(pool, identity.tenant_id, &ds.base_entity).await?;
    let scope = RecordScope {
        user_id: identity.user_id,
        public_read: owd == Owd::PublicRead || owd == Owd::PublicReadWrite,
        public_write: false,
        bypass: identity.is_superuser,
        team_owd: owd == Owd::Team,
        team_id: identity.team_id,
    };

    let scalar: HashSet<&str> = def.fields.iter().map(|f| f.name.as_str()).collect();
    let fk: HashSet<&str> = def
        .relationships
        .iter()
        .map(|r| r.source_field_name.as_str())
        .collect();

    // ---- select (jsonb_build_object pairs) + columns ----
    let mut pairs: Vec<String> = Vec::new();
    let mut columns: Vec<String> = Vec::new();
    for f in &ds.fields {
        let alias = f.alias.clone().unwrap_or_else(|| f.field.clone());
        // Validate alias: must be a safe SQL identifier (no quotes, no
        // backslashes). Single quotes in an alias would break the
        // jsonb_build_object literal.
        if !is_safe_alias(&alias) {
            return Err(Error::Invalid(format!(
                "invalid alias '{alias}': must be alphanumeric + underscores only"
            )));
        }
        // count(*) special case
        if f.aggregate.as_deref() == Some("count") && (f.field == "*" || f.field.is_empty()) {
            pairs.push(format!("'{alias}', count(*)"));
            columns.push(alias);
            continue;
        }
        if !scalar.contains(f.field.as_str()) && !fk.contains(f.field.as_str()) {
            return Err(Error::Invalid(format!("unknown field {}", f.field)));
        }
        // field grain: drop unreadable select fields (graceful)
        if identity.field_access(&ds.base_entity, &f.field) == Access::None {
            continue;
        }
        let col_expr = field_expr(&def, &f.field);
        let expr = match f.aggregate.as_deref() {
            Some("count") => format!("count({col_expr})"),
            Some("sum") | Some("avg") => {
                format!("{}({col_expr}::numeric)", f.aggregate.as_deref().unwrap())
            }
            Some("min") | Some("max") => format!("{}({col_expr})", f.aggregate.as_deref().unwrap()),
            Some(other) => return Err(Error::Invalid(format!("unknown aggregate {other}"))),
            None => col_expr.to_string(),
        };
        pairs.push(format!("'{alias}', {expr}"));
        columns.push(alias);
    }
    if pairs.is_empty() {
        return Err(Error::Invalid("report selects no fields".into()));
    }

    // ---- group_by (semantic: unreadable => error) ----
    let mut group_exprs: Vec<String> = Vec::new();
    for g in &ds.group_by {
        require_readable(identity, &ds.base_entity, g, &scalar, &fk, "group_by")?;
        group_exprs.push(field_expr(&def, g).to_string());
    }

    // ---- order_by (semantic: must be a known, readable field; never interpolated raw) ----
    let mut order_parts: Vec<String> = Vec::new();
    for s in &ds.order_by {
        require_readable(
            identity,
            &ds.base_entity,
            &s.field,
            &scalar,
            &fk,
            "order_by",
        )?;
        let d = if s.asc { "ASC" } else { "DESC" };
        order_parts.push(format!("{} {d}", field_expr(&def, &s.field)));
    }
    let order_sql = if order_parts.is_empty() {
        "1".to_string()
    } else {
        order_parts.join(", ")
    };

    // ---- WHERE: tenant + record scope + filters ----
    let mut parts: Vec<String> = vec!["tenant_id = $1".into()];
    let mut binds: Vec<String> = Vec::new();
    let mut n = 2usize;
    if let Some(u) = read_user(&scope) {
        parts.push(format!("owner_id = ${n}"));
        binds.push(u.to_string());
        n += 1;
    }
    for f in &ds.filters {
        require_readable(identity, &ds.base_entity, &f.field, &scalar, &fk, "filter")?;
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
        } else if matches!(f.op.as_str(), "gt" | "gte" | "lt" | "lte") {
            (field_expr(&def, &f.field).to_string(), "::numeric")
        } else {
            (field_expr(&def, &f.field).to_string(), "")
        };
        parts.push(format!("{lhs} {op} ${n}{rhs_cast}"));
        binds.push(f.value.clone());
        n += 1;
    }

    let table = &def.entity.table_name;
    let select_clause = format!("SELECT jsonb_build_object({}) AS row", pairs.join(", "));
    let mut sql = format!(
        "{select_clause} FROM biz.{table} WHERE {}",
        parts.join(" AND ")
    );
    if !group_exprs.is_empty() {
        sql.push_str(&format!(" GROUP BY {}", group_exprs.join(", ")));
    }
    sql.push_str(&format!(" ORDER BY {order_sql}"));
    if let Some(lim) = ds.limit {
        sql.push_str(&format!(" LIMIT {lim}"));
    }

    // bind tenant + owner + filters (all text; casts applied in SQL). Run inside
    // a short-lived txn so the per-run statement_timeout is scoped to this query
    // only (§5.17) — a runaway report is killed rather than left to scan — AND
    // so the biz.* RLS policy sees this caller's tenant (RLS fail-closes without
    // the GUC; mda-data does the same per operation).
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(identity.tenant_id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
    sqlx::query("SELECT set_config('statement_timeout', $1, true)")
        .bind(REPORT_TIMEOUT_MS)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
    let mut q = sqlx::query_as::<_, (Value,)>(sql.as_str()).bind(identity.tenant_id);
    for b in &binds {
        q = q.bind(b);
    }
    let rows: Vec<(Value,)> = q.fetch_all(&mut *tx).await.map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    let out: Vec<Map<String, Value>> = rows
        .into_iter()
        .map(|(v,)| match v {
            Value::Object(m) => m,
            other => {
                let mut m = Map::new();
                m.insert("value".into(), other);
                m
            }
        })
        .collect();
    Ok(ReportResult { columns, rows: out })
}

/// Render a result as CSV (header = columns).
pub fn to_csv(res: &ReportResult) -> String {
    let mut s = String::new();
    s.push_str(&res.columns.join(","));
    s.push('\n');
    for row in &res.rows {
        let vals: Vec<String> = res
            .columns
            .iter()
            .map(|c| row.get(c).map(cell).unwrap_or_default())
            .collect();
        s.push_str(&vals.join(","));
        s.push('\n');
    }
    s
}

fn cell(v: &Value) -> String {
    match v {
        Value::String(s) => {
            if s.contains(',') || s.contains('"') || s.contains('\n') {
                format!("\"{}\"", s.replace('"', "\"\""))
            } else {
                s.clone()
            }
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn field_expr(def: &EntityDefinition, name: &str) -> String {
    if def
        .relationships
        .iter()
        .any(|r| r.source_field_name == name)
    {
        name.to_string()
    } else {
        format!("(attributes->>'{name}')")
    }
}

/// Reject aliases that contain characters unsafe in a single-quoted SQL literal.
fn is_safe_alias(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn require_readable(
    id: &Identity,
    entity: &str,
    field: &str,
    scalar: &HashSet<&str>,
    fk: &HashSet<&str>,
    position: &str,
) -> Result<()> {
    if !scalar.contains(field) && !fk.contains(field) {
        return Err(Error::Invalid(format!("unknown field {field}")));
    }
    if id.field_access(entity, field) == Access::None {
        return Err(Error::Forbidden(format!(
            "runner cannot read {field} used in {position}"
        )));
    }
    Ok(())
}

fn read_user(s: &RecordScope) -> Option<Uuid> {
    if s.bypass || s.public_read {
        None
    } else {
        Some(s.user_id)
    }
}
