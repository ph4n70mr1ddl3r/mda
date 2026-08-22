//! DDL generation: turn a draft entity/field/relationship into the SQL that
//! materializes a `biz.<table>` (PLAN §5.1, §5.7, ADR-0001).
//!
//! Identifiers are interpolated (they come from validated, trusted metadata —
//! never from request values; §5.16). Values are never interpolated.

use mda_core::{Error, Result};
use mda_meta::draft::{DraftEntity, DraftField, DraftRelationship};
use serde_json::Value;

/// Map a metadata field type to its SQL column type.
pub fn sql_type(field_type: &str, config: &Value) -> Result<String> {
    Ok(match field_type {
        "string" | "text" | "enum" | "attachment" => "TEXT".to_string(),
        "integer" | "auto_number" => "BIGINT".to_string(),
        "decimal" => {
            // Bounded, not `as u32` — a precision of exactly 2^32 would wrap
            // to 0 (NUMERIC(0,s)) instead of erroring. The draft gate rejects
            // out-of-range values; this is the defense-in-depth backstop.
            let p = config
                .get("precision")
                .and_then(|v| v.as_u64())
                .unwrap_or(20);
            let s = config.get("scale").and_then(|v| v.as_u64()).unwrap_or(0);
            if !(1..=1000).contains(&p) || s > p || s > 1000 {
                return Err(Error::Invalid(format!(
                    "decimal precision/scale out of range (precision {p}, scale {s}): 1 ≤ precision ≤ 1000, scale ≤ precision"
                )));
            }
            format!("NUMERIC({p},{s})")
        }
        "money" => "NUMERIC(20,4)".to_string(),
        "bool" => "BOOLEAN".to_string(),
        "date" => "DATE".to_string(),
        "datetime" => "TIMESTAMPTZ".to_string(),
        "json" => "JSONB".to_string(),
        other => {
            return Err(Error::Invalid(format!(
                "field type {other} cannot be hoisted to a column"
            )))
        }
    })
}

/// The generated-column expression deriving a column from `attributes`.
fn gen_expr(name: &str, sqltype: &str) -> String {
    format!("((attributes->>'{name}')::{sqltype})")
}

/// Defense-in-depth assert before identifier interpolation: metadata names are
/// validated at draft/publish time (`mda_meta::draft::is_valid_identifier`, the
/// single gate per §5.16), so reaching here with an unsafe name means that gate
/// was bypassed — refuse to build the SQL rather than interpolate.
fn guard_ident(kind: &str, name: &str) -> Result<()> {
    if mda_meta::draft::is_valid_identifier(name) {
        Ok(())
    } else {
        Err(Error::internal(anyhow::anyhow!(
            "unsafe {kind} identifier `{name}` reached DDL generation (metadata validation gate bypassed)"
        )))
    }
}

fn on_delete_clause(on_delete: &Option<String>) -> &'static str {
    match on_delete.as_deref() {
        Some("set_null") => "SET NULL",
        Some("cascade") => "CASCADE",
        _ => "RESTRICT",
    }
}

/// Statements to ensure the `biz` schema exists.
pub fn ensure_schema() -> Vec<String> {
    vec!["CREATE SCHEMA IF NOT EXISTS biz".to_string()]
}

/// CREATE TABLE for a brand-new entity: core columns + GENERATED columns for
/// unique/indexed scalar fields + `attributes` JSONB + tenant index + field
/// indexes. Reference (FK) columns are added separately by [`add_relationship`].
pub fn create_table(table: &str, e: &DraftEntity) -> Result<Vec<String>> {
    guard_ident("table", table)?;
    for f in &e.fields {
        guard_ident("field", &f.name)?;
    }
    for r in &e.relationships {
        guard_ident("relationship", &r.source_field_name)?;
    }
    let mut out = Vec::new();
    let mut cols: Vec<String> = vec![
        "id UUID PRIMARY KEY".into(),
        "tenant_id UUID NOT NULL".into(),
        "owner_id UUID".into(),
        "state TEXT NOT NULL DEFAULT 'active'".into(),
        "version BIGINT NOT NULL DEFAULT 1".into(),
        "created_at TIMESTAMPTZ NOT NULL DEFAULT now()".into(),
        "updated_at TIMESTAMPTZ NOT NULL DEFAULT now()".into(),
    ];
    for f in &e.fields {
        if f.is_unique || f.is_indexed {
            let st = sql_type(&f.field_type, &f.config)?;
            let mut c = format!(
                "{} {st} GENERATED ALWAYS AS ({}) STORED",
                f.name,
                gen_expr(&f.name, &st)
            );
            if f.is_unique {
                c.push_str(" UNIQUE");
            }
            cols.push(c);
        }
    }
    cols.push("attributes JSONB NOT NULL DEFAULT '{}'::jsonb".into());
    let body = cols.join(",\n  ");
    out.push(format!(
        "CREATE TABLE IF NOT EXISTS biz.{table} (\n  {body}\n)"
    ));
    out.push(format!(
        "CREATE INDEX IF NOT EXISTS {table}_tenant_idx ON biz.{table} (tenant_id)"
    ));
    for f in &e.fields {
        if f.is_indexed && !f.is_unique {
            out.push(format!(
                "CREATE INDEX IF NOT EXISTS {table}_{}_idx ON biz.{table} ({})",
                f.name, f.name
            ));
        }
    }
    // ADR-0006 / ADR-0015: a twin archive table + BEFORE DELETE trigger so every
    // hard delete (and every cascade-deleted child) is recoverable. The archive
    // is a plain structural copy (no generated columns / constraints); the
    // generic mda.archive_row() copies the non-generated columns on DELETE.
    out.push(format!(
        "CREATE TABLE IF NOT EXISTS biz_archive.{table} (LIKE biz.{table})"
    ));
    out.push(format!(
        "ALTER TABLE biz_archive.{table} \
         ADD COLUMN IF NOT EXISTS archive_batch_id UUID, \
         ADD COLUMN IF NOT EXISTS archived_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
         ADD COLUMN IF NOT EXISTS archived_by UUID"
    ));
    out.push(format!(
        "CREATE INDEX IF NOT EXISTS archive_{table}_restore_idx \
         ON biz_archive.{table} (tenant_id, id, archived_at DESC)"
    ));
    out.push(format!(
        "DROP TRIGGER IF EXISTS {table}_archive ON biz.{table}"
    ));
    out.push(format!(
        "CREATE TRIGGER {table}_archive BEFORE DELETE ON biz.{table} \
         FOR EACH ROW EXECUTE FUNCTION mda.archive_row()"
    ));
    // Row-Level Security: tenant isolation at the DB layer (§5.4 / §5.11). The
    // app connects as a non-superuser role that owns these tables, so both
    // ENABLE and FORCE are required (FORCE makes even the owner subject). The
    // app sets `app.tenant_id` per operation; a query that forgets — or a
    // cross-tenant probe — sees nothing (fail-closed).
    out.extend(rls_stmts("biz", table));
    out.extend(rls_stmts("biz_archive", table));
    Ok(out)
}

/// ENABLE + FORCE RLS and a `tenant_isolation` policy for one table. Policy
/// names are scoped per-table, so a fixed name is reused safely on every table.
fn rls_stmts(schema: &str, table: &str) -> Vec<String> {
    vec![
        format!("ALTER TABLE {schema}.{table} ENABLE ROW LEVEL SECURITY"),
        format!("ALTER TABLE {schema}.{table} FORCE ROW LEVEL SECURITY"),
        format!("DROP POLICY IF EXISTS tenant_isolation ON {schema}.{table}"),
        format!(
            "CREATE POLICY tenant_isolation ON {schema}.{table} \
             USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid) \
             WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)"
        ),
    ]
}

/// Add a new field to an existing biz table. Only unique/indexed fields need a
/// generated column; plain scalars already live in `attributes`.
pub fn add_field(table: &str, f: &DraftField) -> Result<Vec<String>> {
    guard_ident("table", table)?;
    guard_ident("field", &f.name)?;
    if !(f.is_unique || f.is_indexed) {
        return Ok(vec![]);
    }
    let st = sql_type(&f.field_type, &f.config)?;
    let mut out = Vec::new();
    let mut col = format!(
        "ADD COLUMN {} {st} GENERATED ALWAYS AS ({}) STORED",
        f.name,
        gen_expr(&f.name, &st)
    );
    if f.is_unique {
        col.push_str(" UNIQUE");
    }
    out.push(format!("ALTER TABLE biz.{table} {col}"));
    if f.is_indexed && !f.is_unique {
        out.push(format!(
            "CREATE INDEX IF NOT EXISTS {table}_{}_idx ON biz.{table} ({})",
            f.name, f.name
        ));
    }
    // mirror the column to the archive table as a plain stored column so the
    // BEFORE DELETE trigger's column list stays valid (generated columns aren't
    // inserted; this holds the old value).
    out.push(format!(
        "ALTER TABLE biz_archive.{table} ADD COLUMN IF NOT EXISTS {} {st}",
        f.name
    ));
    Ok(out)
}

/// Add a relationship: hoisted UUID FK column + native `FOREIGN KEY` constraint + index.
pub fn add_relationship(
    table: &str,
    r: &DraftRelationship,
    target_table: &str,
) -> Result<Vec<String>> {
    guard_ident("table", table)?;
    guard_ident("table", target_table)?;
    guard_ident("relationship", &r.source_field_name)?;
    let col = &r.source_field_name;
    let (nullspec, ondel) = match r.strength.as_str() {
        "master_detail" => (" NOT NULL", "CASCADE"),
        _ => ("", on_delete_clause(&r.on_delete)),
    };
    let mut out = vec![
        format!("ALTER TABLE biz.{table} ADD COLUMN {col} UUID{nullspec}"),
        format!(
            "ALTER TABLE biz.{table} ADD CONSTRAINT {table}_{col}_fk \
             FOREIGN KEY ({col}) REFERENCES biz.{target_table} (id) ON DELETE {ondel}"
        ),
        format!("CREATE INDEX IF NOT EXISTS {table}_{col}_idx ON biz.{table} ({col})"),
    ];
    // mirror the FK column to the archive table (see add_field).
    out.push(format!(
        "ALTER TABLE biz_archive.{table} ADD COLUMN IF NOT EXISTS {col} UUID{nullspec}"
    ));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mda_meta::draft::{DraftEntity, DraftField};
    use uuid::Uuid;

    #[test]
    fn create_table_has_core_attrs_and_generated_unique() {
        let mut e = DraftEntity {
            id: Uuid::nil(),
            module_id: None,
            name: "Customer".into(),
            table_name: "customer".into(),
            label: None,
            description: None,
            fields: vec![],
            relationships: vec![],
        };
        e.fields.push(DraftField {
            id: Uuid::nil(),
            name: "email".into(),
            label: None,
            field_type: "string".into(),
            required: false,
            is_unique: true,
            is_indexed: false,
            default_expr: None,
            config: serde_json::json!({}),
        });
        let stmts = create_table("customer", &e).unwrap();
        let create = stmts.join("\n");
        assert!(create.contains("id UUID PRIMARY KEY"));
        assert!(create.contains("attributes JSONB NOT NULL"));
        assert!(create.contains("email TEXT GENERATED ALWAYS AS"));
        assert!(create.contains("UNIQUE"));
    }

    #[test]
    fn sql_type_bounds_decimal_precision_without_wrapping() {
        // 2^32 wraps through `as u32` to 0 — the backstop must reject it (and
        // every other out-of-Postgres-range pair) instead of emitting
        // NUMERIC(0,s).
        for (p, s) in [(0u64, 0u64), (1001, 0), (4294967296, 0), (10, 11)] {
            assert!(
                sql_type("decimal", &serde_json::json!({"precision": p, "scale": s})).is_err(),
                "precision {p} scale {s} must be rejected"
            );
        }
        assert_eq!(
            sql_type("decimal", &serde_json::json!({"precision": 20, "scale": 4})).unwrap(),
            "NUMERIC(20,4)"
        );
        // defaults when config omits them
        assert_eq!(
            sql_type("decimal", &serde_json::json!({})).unwrap(),
            "NUMERIC(20,0)"
        );
    }
}
