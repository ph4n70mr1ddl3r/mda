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
    // Header names are quoted like any string field so the file round-trips
    // through [`from_csv`] even if a name happens to contain a comma/quote.
    let header: Vec<String> = res.columns.iter().map(|c| quote(c)).collect();
    let mut s = String::new();
    s.push_str(&header.join(","));
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
        Value::String(s) => quote(s),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// RFC-4180 quoting: wrap in quotes iff the value contains a comma, quote,
/// CR, or LF; double any embedded quotes.
fn quote(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Parse RFC-4180 CSV (a header row naming the columns, then one record per
/// line) into a [`ReportResult`] whose rows hold the **raw string** values as
/// `Value::String` (typing/coercion is the caller's job — e.g. the impex import
/// path lets the runtime write pipeline coerce per field type). Quoted fields
/// may contain commas, newlines (CR/LF/CRLF), and `""` for a literal quote.
/// Mirrors [`to_csv`] so an export round-trips back through `from_csv`. Returns
/// `mda.invalid` for an empty input (no header).
pub fn from_csv(input: &str) -> Result<ReportResult> {
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut field = String::new();
    let mut record: Vec<String> = Vec::new();
    let mut in_quotes = false;
    let mut field_pending = false; // we are inside a (possibly empty) field
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => {
                    if matches!(chars.peek(), Some('"')) {
                        field.push('"');
                        chars.next();
                    } else {
                        in_quotes = false;
                    }
                }
                _ => field.push(c),
            }
        } else {
            match c {
                '"' if !field_pending => {
                    in_quotes = true;
                    field_pending = true;
                }
                '"' => field.push('"'),
                ',' => {
                    record.push(std::mem::take(&mut field));
                    field_pending = false;
                }
                '\r' => {
                    if matches!(chars.peek(), Some('\n')) {
                        chars.next();
                    }
                    record.push(std::mem::take(&mut field));
                    records.push(std::mem::take(&mut record));
                    field_pending = false;
                }
                '\n' => {
                    record.push(std::mem::take(&mut field));
                    records.push(std::mem::take(&mut record));
                    field_pending = false;
                }
                _ => {
                    field.push(c);
                    field_pending = true;
                }
            }
        }
    }
    // Flush a trailing record that had no terminating newline. After a newline
    // both `field` and `record` are empty (taken), so we don't emit a phantom
    // empty row.
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }

    let mut iter = records.into_iter();
    let header = iter
        .next()
        .ok_or_else(|| Error::Invalid("empty CSV: no header row".to_string()))?;
    let columns = header.clone();
    let mut rows = Vec::with_capacity(iter.len());
    for rec in iter {
        let mut m = Map::with_capacity(columns.len());
        for (i, col) in columns.iter().enumerate() {
            let v = rec.get(i).cloned().unwrap_or_default();
            m.insert(col.clone(), Value::String(v));
        }
        rows.push(m);
    }
    Ok(ReportResult { columns, rows })
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

#[cfg(test)]
mod csv_tests {
    use super::*;

    fn row(pairs: &[(&str, &str)]) -> Map<String, Value> {
        let mut m = Map::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), Value::String((*v).to_string()));
        }
        m
    }

    #[test]
    fn csv_round_trips_quoting_and_newlines() {
        let res = ReportResult {
            columns: vec!["id".into(), "name".into(), "note".into()],
            rows: vec![
                row(&[
                    ("id", "1"),
                    ("name", "Acme, Inc."),
                    ("note", "has \"quotes\""),
                ]),
                row(&[("id", "2"), ("name", "Line\nBreak"), ("note", "plain")]),
                row(&[("id", "3"), ("name", ""), ("note", "")]),
            ],
        };
        let csv = to_csv(&res);
        let back = from_csv(&csv).expect("round-trip");
        assert_eq!(back.columns, res.columns);
        assert_eq!(back.rows.len(), 3);
        assert_eq!(back.rows[0]["name"].as_str(), Some("Acme, Inc."));
        assert_eq!(back.rows[0]["note"].as_str(), Some("has \"quotes\""));
        assert_eq!(back.rows[1]["name"].as_str(), Some("Line\nBreak"));
        assert_eq!(back.rows[2]["name"].as_str(), Some(""));
    }

    #[test]
    fn csv_handles_crlf_and_trailing_field() {
        let csv = "a,b,c\r\n1,2,\r\n4,5,6\r\n";
        let back = from_csv(csv).expect("parse");
        assert_eq!(back.columns, vec!["a", "b", "c"]);
        assert_eq!(back.rows.len(), 2);
        assert_eq!(back.rows[0]["c"].as_str(), Some(""));
        assert_eq!(back.rows[1]["c"].as_str(), Some("6"));
    }

    #[test]
    fn csv_no_trailing_newline_is_flushed() {
        let csv = "x,y\n1,2";
        let back = from_csv(csv).expect("parse");
        assert_eq!(back.rows.len(), 1);
        assert_eq!(back.rows[0]["y"].as_str(), Some("2"));
    }

    #[test]
    fn csv_empty_input_is_an_error() {
        assert!(from_csv("").is_err());
    }

    #[test]
    fn csv_header_only_yields_zero_rows() {
        let back = from_csv("x,y\n").expect("parse");
        assert_eq!(back.columns, vec!["x", "y"]);
        assert!(back.rows.is_empty());
    }

    #[test]
    fn csv_short_row_pads_missing_columns() {
        let back = from_csv("a,b,c\n1,2\n").expect("parse");
        assert_eq!(back.rows[0]["c"].as_str(), Some(""));
    }
}
