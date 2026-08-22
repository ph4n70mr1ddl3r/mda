//! `mda-reports` — structured reporting (PLAN §5.17).
//!
//! A report dataset is a structured declaration (base entity + select fields +
//! filters + group_by + order_by + limit) that the engine compiles to
//! parameterized SQL over `biz.<table>`. Because the engine builds the SQL, it
//! enforces the **runner's** object/field/record security by construction:
//!  - object: needs `read` on the base entity (and on every entity a reference
//!    traversal crosses);
//!  - field (projection): unreadable select fields are dropped;
//!  - field (semantic): an unreadable field in `filter`/`group_by`/`order_by` is
//!    a run-time error (a dropped filter/group would change semantics / leak);
//!  - record: the runner's ownership/OWD/sharing predicate (the same one the
//!    data API injects) is part of the WHERE.
//!
//! Fields may traverse references (`customer.name`) — compiled to real LEFT
//! JOINs over the hoisted FK columns (§5.7). Renderers: CSV, HTML, XLSX and PDF
//! (`render`), so a run can be exported for any audience. Scheduled delivery
//! rides the §14 scheduler (`kind=report`) with optional `report.completed`
//! notification delivery.

use mda_core::{Error, Result};
use mda_data::{Filter, RecordScope, Sort};
use mda_meta::{loader, EntityDefinition};
use mda_security::{Access, Identity, Owd};
use serde::Deserialize;
use serde_json::{Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

pub mod render;
pub mod template;

pub use render::{to_html, to_pdf, to_xlsx};
pub use template::{render, render_body, Rendered, Template};

/// Wall-clock cap (ms) on a single synchronous report run (§5.17 cost control).
/// Overruns are killed by Postgres and surface as an internal error; large
/// reports must run async as a job (a follow-up).
const REPORT_TIMEOUT_MS: &str = "10000";

/// A structured report dataset (the JSON stored in `md_report.dataset`).
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
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

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
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
///
/// Fields may traverse references (`customer.name`, `customer.region.name`, up
/// to [`MAX_HOPS`] hops): each hop resolves to a real `LEFT JOIN` over the
/// hoisted FK column (§5.7 — indexed, no string keys). Security is enforced per
/// hop: every crossed entity requires object-level `read`, and the leaf field is
/// field-level checked against the entity it belongs to (select fields are
/// dropped when unreadable — graceful; filter/group/order paths error —
/// semantic, §5.11/§5.17). The record-scope predicate is the **same** predicate
/// the data API injects (owner ∨ shares ∨ team-OWD ∨ role hierarchy), so a
/// report never sees a record the runner could not read.
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

    // ---- joins accumulated by reference traversals ----
    let mut joins: Vec<String> = Vec::new();

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
        let r = resolve_field(pool, identity.tenant_id, &def, &f.field).await?;
        // field grain: drop unreadable select fields (graceful) — a denied hop
        // or denied leaf field removes the column rather than failing the run.
        if !readable(identity, &r) {
            continue;
        }
        merge_joins(&mut joins, &r);
        let expr = match f.aggregate.as_deref() {
            Some("count") => format!("count({})", r.sql),
            Some("sum") | Some("avg") => {
                format!("{}({}::numeric)", f.aggregate.as_deref().unwrap(), r.sql)
            }
            Some("min") | Some("max") => format!("{}({})", f.aggregate.as_deref().unwrap(), r.sql),
            Some(other) => return Err(Error::Invalid(format!("unknown aggregate {other}"))),
            None => r.sql.to_string(),
        };
        pairs.push(format!("'{alias}', {expr}"));
        columns.push(alias);
    }
    if pairs.is_empty() {
        return Err(Error::Invalid("report selects no fields".into()));
    }
    // Duplicate aliases silently shadow each other: jsonb_build_object keeps
    // the *last* pair for a repeated key, so the first field's values vanish —
    // and the CSV renderer would emit a duplicate header that from_csv (the
    // impex import parser) rejects, breaking an export→import round-trip.
    // Catch it at run time with a message naming the alias.
    {
        let mut seen = std::collections::HashSet::with_capacity(columns.len());
        for c in &columns {
            if !seen.insert(c.as_str()) {
                return Err(Error::Invalid(format!(
                    "duplicate select alias '{c}': give each selected field a distinct alias"
                )));
            }
        }
    }

    // ---- group_by (semantic: unreadable => error) ----
    let mut group_exprs: Vec<String> = Vec::new();
    for g in &ds.group_by {
        let r = resolve_field(pool, identity.tenant_id, &def, g).await?;
        require_readable(identity, &r, "group_by")?;
        merge_joins(&mut joins, &r);
        group_exprs.push(r.sql);
    }

    // ---- order_by (semantic: must be a known, readable field; never interpolated raw) ----
    let mut order_parts: Vec<String> = Vec::new();
    for s in &ds.order_by {
        let r = resolve_field(pool, identity.tenant_id, &def, &s.field).await?;
        require_readable(identity, &r, "order_by")?;
        merge_joins(&mut joins, &r);
        let d = if s.asc { "ASC" } else { "DESC" };
        order_parts.push(format!("{} {d}", r.sql));
    }
    let order_sql = if order_parts.is_empty() {
        "1".to_string()
    } else {
        order_parts.join(", ")
    };

    // ---- WHERE: tenant + record scope + filters ----
    let mut parts: Vec<String> = vec!["t.tenant_id = $1".into()];
    let mut binds: Vec<RB> = vec![RB::Uuid(identity.tenant_id)];
    let mut n = 2usize;
    if let Some(pred) = mda_data::read_predicate(&scope) {
        let (frag, ub) = mda_data::pred_render(&pred, &scope, n);
        parts.push(frag);
        n += ub.len();
        for u in ub {
            binds.push(RB::Uuid(u));
        }
    }
    for f in &ds.filters {
        let r = resolve_field(pool, identity.tenant_id, &def, &f.field).await?;
        require_readable(identity, &r, "filter")?;
        merge_joins(&mut joins, &r);
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
        let rhs_cast = match r.kind {
            FieldKind::Fk => "::uuid",
            FieldKind::System if matches!(f.op.as_str(), "gt" | "gte" | "lt" | "lte") => {
                "::numeric"
            }
            FieldKind::Scalar if matches!(f.op.as_str(), "gt" | "gte" | "lt" | "lte") => {
                "::numeric"
            }
            _ => "",
        };
        parts.push(format!("{} {op} ${n}{rhs_cast}", r.sql));
        binds.push(RB::Text(f.value.clone()));
        n += 1;
    }

    let table = &def.entity.table_name;
    let select_clause = format!("SELECT jsonb_build_object({}) AS row", pairs.join(", "));
    let mut sql = format!(
        "{select_clause} FROM biz.{table} t {join_sql} WHERE {}",
        parts.join(" AND "),
        join_sql = joins.join(" ")
    );
    if !group_exprs.is_empty() {
        sql.push_str(&format!(" GROUP BY {}", group_exprs.join(", ")));
    }
    sql.push_str(&format!(" ORDER BY {order_sql}"));
    // No unbounded fetches: an absent limit gets the default cap (a wide table
    // can return millions of rows well inside the 10 s statement timeout and
    // OOM the process); an explicit limit is clamped to the same cap. Larger
    // exports belong in the async job path (§5.13).
    const DEFAULT_ROW_CAP: u64 = 10_000;
    let lim = ds
        .limit
        .unwrap_or(DEFAULT_ROW_CAP)
        .clamp(1, DEFAULT_ROW_CAP);
    sql.push_str(&format!(" LIMIT {lim}"));

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
    let mut q = sqlx::query_as::<_, (Value,)>(sql.as_str());
    for b in &binds {
        q = match b {
            RB::Uuid(u) => q.bind(*u),
            RB::Text(s) => q.bind(s),
        };
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

/// Mixed report binds (uuid record-scope binds + text filter values).
enum RB {
    Uuid(Uuid),
    Text(String),
}

/// System columns present on every generated `biz.<table>` (§5.7) that reports
/// may select/filter/group on in addition to declared fields.
pub const SYSTEM_FIELDS: [&str; 6] = [
    "id",
    "version",
    "state",
    "owner_id",
    "created_at",
    "updated_at",
];

/// How the leaf of a field reference is stored.
#[derive(Debug, Clone, Copy, PartialEq)]
enum FieldKind {
    Scalar,
    Fk,
    System,
}

/// Reject aliases that contain characters unsafe in a single-quoted SQL literal.
fn is_safe_alias(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A resolved field reference: the SQL column expression (base table alias
/// `t`, traversal join aliases `j_*`), the join fragments each hop needs, the
/// entities crossed (for per-hop AuthZ), and the leaf (entity, field) the
/// field-level check applies to.
struct FieldRef {
    sql: String,
    joins: Vec<String>,
    hop_entities: Vec<String>,
    leaf_entity: String,
    leaf_field: String,
    kind: FieldKind,
}

/// Maximum reference-traversal depth (`customer.region.name` = 2 hops).
const MAX_HOPS: usize = 3;

/// Resolve a (possibly dotted) field path against the base entity definition.
/// Every hop must be a reference field; the join chain is built left-join over
/// the hoisted FK column so a missing reference yields NULL, not a dropped row.
async fn resolve_field(
    pool: &PgPool,
    tenant: Uuid,
    base: &EntityDefinition,
    path: &str,
) -> Result<FieldRef> {
    let segments: Vec<&str> = path.split('.').map(str::trim).collect();
    if segments.iter().any(|s| s.is_empty()) {
        return Err(Error::Invalid(format!("malformed field path '{path}'")));
    }
    if segments.len() > MAX_HOPS + 1 {
        return Err(Error::Invalid(format!(
            "field path '{path}' exceeds {MAX_HOPS} reference hops"
        )));
    }

    let mut entity = base.entity.name.clone();
    let mut table = base.entity.table_name.clone();
    let mut alias = "t".to_string();
    let mut joins: Vec<String> = Vec::new();
    let mut hop_entities: Vec<String> = Vec::new();

    for (i, seg) in segments.iter().enumerate() {
        let last = i == segments.len() - 1;
        // system columns exist on every table (base or joined target)
        if last && SYSTEM_FIELDS.contains(seg) {
            return Ok(FieldRef {
                sql: format!("{alias}.{seg}"),
                joins,
                hop_entities,
                leaf_entity: entity,
                leaf_field: (*seg).to_string(),
                kind: FieldKind::System,
            });
        }
        let current = loaded_def(pool, tenant, &entity).await?;
        if let Some(rel) = current
            .relationships
            .iter()
            .find(|r| r.source_field_name == *seg)
        {
            // a reference — either the leaf (the id itself) or the next hop
            let leaf_sql = format!("{alias}.{seg}");
            if last {
                return Ok(FieldRef {
                    sql: leaf_sql,
                    joins,
                    hop_entities,
                    leaf_entity: entity,
                    leaf_field: (*seg).to_string(),
                    kind: FieldKind::Fk,
                });
            }
            if i == MAX_HOPS {
                return Err(Error::Invalid(format!(
                    "field path '{path}' exceeds {MAX_HOPS} reference hops"
                )));
            }
            let target = loader::load_entity_definition(pool, tenant, rel.target_entity_id).await?;
            if target.entity.status != "active" {
                return Err(Error::Invalid(format!(
                    "entity {} is retired",
                    target.entity.name
                )));
            }
            let jalias = format!(
                "j_{}",
                segments[..=i]
                    .iter()
                    .map(|s| sanitize_alias(s))
                    .collect::<Vec<_>>()
                    .join("_")
            );
            // tenant-safe join: a cross-tenant id can never match (belt and
            // braces on top of the FK, which already guarantees it).
            joins.push(format!(
                "LEFT JOIN biz.{tname} {ja} ON {ja}.id = {alias}.{seg} AND {ja}.tenant_id = t.tenant_id",
                tname = target.entity.table_name,
                ja = jalias,
            ));
            hop_entities.push(target.entity.name.clone());
            entity = target.entity.name;
            table = target.entity.table_name;
            alias = jalias;
            continue;
        }
        // scalar field on the current entity
        let d = loaded_def(pool, tenant, &entity).await?;
        if !last || !d.fields.iter().any(|f| f.name == *seg) {
            return Err(Error::Invalid(format!("unknown field {path}")));
        }
        return Ok(FieldRef {
            sql: format!("({alias}.attributes->>'{seg}')"),
            joins,
            hop_entities,
            leaf_entity: entity,
            leaf_field: (*seg).to_string(),
            kind: FieldKind::Scalar,
        });
    }
    let _ = table;
    Err(Error::Invalid(format!("unknown field {path}")))
}

/// Look up one entity's definition by name (loads it uncached; reports resolve
/// a handful of hops per run, so this stays cheap).
async fn loaded_def(pool: &PgPool, tenant: Uuid, name: &str) -> Result<EntityDefinition> {
    let id = loader::entity_id_by_name(pool, tenant, name).await?;
    loader::load_entity_definition(pool, tenant, id).await
}

/// Per-hop AuthZ: every crossed entity needs object-level read, and the leaf
/// field must not be field-level denied on the entity it belongs to.
fn readable(identity: &Identity, r: &FieldRef) -> bool {
    for e in &r.hop_entities {
        if !identity.can(e, "read") {
            return false;
        }
    }
    identity.field_access(&r.leaf_entity, &r.leaf_field) != Access::None
}

/// Same check as [`readable`] but **errors** — for filter/group_by/order_by,
/// where silently dropping the reference would change semantics (§5.17).
fn require_readable(identity: &Identity, r: &FieldRef, position: &str) -> Result<()> {
    for e in &r.hop_entities {
        if !identity.can(e, "read") {
            return Err(Error::Forbidden(format!(
                "runner cannot read {e} (crossed by {position} path)"
            )));
        }
    }
    if identity.field_access(&r.leaf_entity, &r.leaf_field) == Access::None {
        return Err(Error::Forbidden(format!(
            "runner cannot read {} used in {}",
            r.leaf_field, position
        )));
    }
    Ok(())
}

/// Dedupe join fragments already accumulated (two fields sharing a path prefix
/// join the same target table once).
fn merge_joins(acc: &mut Vec<String>, r: &FieldRef) {
    for j in &r.joins {
        let key = join_key(j);
        if !acc.iter().any(|e| join_key(e) == key) {
            acc.push(j.clone());
        }
    }
}

/// Identity of a join fragment = its alias (unique per path prefix).
fn join_key(j: &str) -> &str {
    j.split_whitespace().nth(3).unwrap_or_default()
}

fn sanitize_alias(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
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
/// `mda.invalid` for an empty input (no header), for a header with duplicate
/// column names (ambiguous — a later duplicate would silently shadow the
/// earlier column's values), or for a record with more cells than the header
/// (extra cells would be silently dropped). A leading UTF-8 BOM — what every
/// Excel "CSV UTF-8" export starts with — is stripped so the first column maps.
pub fn from_csv(input: &str) -> Result<ReportResult> {
    // Excel writes a BOM before the header; without this the first column name
    // carries '\u{feff}' and the import fails mapping with a confusing name.
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
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
    {
        let mut seen = std::collections::HashSet::with_capacity(columns.len());
        for (i, c) in columns.iter().enumerate() {
            if !seen.insert(c.as_str()) {
                return Err(Error::Invalid(format!(
                    "duplicate CSV column '{c}' (column {}): column names must be unique",
                    i + 1
                )));
            }
        }
    }
    let mut rows = Vec::with_capacity(iter.len());
    for (n, rec) in iter.enumerate() {
        if rec.len() > columns.len() {
            return Err(Error::Invalid(format!(
                "CSV row {} has {} cells but the header has {} columns",
                n + 1,
                rec.len(),
                columns.len()
            )));
        }
        let mut m = Map::with_capacity(columns.len());
        for (i, col) in columns.iter().enumerate() {
            let v = rec.get(i).cloned().unwrap_or_default();
            m.insert(col.clone(), Value::String(v));
        }
        rows.push(m);
    }
    Ok(ReportResult { columns, rows })
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

    #[test]
    fn csv_leading_bom_is_stripped() {
        // Every Excel "CSV UTF-8" export starts with a BOM; without stripping,
        // the first column name carries '\u{feff}' and the impex import fails
        // column mapping with a confusing "unmapped source columns" error.
        let back = from_csv("\u{feff}id,name\n1,Acme\n").expect("parse");
        assert_eq!(back.columns, vec!["id", "name"]);
        assert_eq!(back.rows[0]["id"].as_str(), Some("1"));
    }

    #[test]
    fn csv_duplicate_columns_are_rejected() {
        // A duplicate header is ambiguous: the row map would silently keep the
        // *last* column's value for the shared name (quiet data loss).
        let err = from_csv("id,name,id\n1,Acme,2\n").expect_err("duplicate column");
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn csv_overlong_row_is_rejected() {
        // More cells than the header means malformed CSV; pre-fix the extra
        // cells were silently dropped.
        let err = from_csv("a,b\n1,2,3\n").expect_err("overlong row");
        assert!(err.to_string().contains("row 1"), "{err}");
    }
}
