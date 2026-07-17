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
            let p = config
                .get("precision")
                .and_then(|v| v.as_u64())
                .unwrap_or(20) as u32;
            let s = config.get("scale").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
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
    out.push(format!("CREATE TABLE biz.{table} (\n  {body}\n)"));
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
        "DROP TRIGGER IF EXISTS {table}_archive ON biz.{table}; \
         CREATE TRIGGER {table}_archive BEFORE DELETE ON biz.{table} \
         FOR EACH ROW EXECUTE FUNCTION mda.archive_row()"
    ));
    Ok(out)
}

/// Add a new field to an existing biz table. Only unique/indexed fields need a
/// generated column; plain scalars already live in `attributes`.
pub fn add_field(table: &str, f: &DraftField) -> Result<Vec<String>> {
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
}
