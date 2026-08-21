//! `GET /api/schema/:entity` — JSON-Schema discovery for the dynamic data API
//! (PLAN §7). The schema is derived from the **active model** at request time
//! (never a stale, stored artifact) and resolved against the caller's security,
//! mirroring the UI render APIs (Phase 6):
//!
//! - requires object-level `read` on the entity (403 otherwise);
//! - a field the caller cannot read (FLS `none`) is **dropped**, never hidden
//!   with a null type — a definition can't widen access (§5.11/§5.17);
//! - a field the caller may only read carries JSON-Schema `readOnly: true`,
//!   so generated SDK clients surface it as immutable;
//! - reference (relationship) columns are `string`/`format: uuid` annotated
//!   with `x-mda-target-entity` so clients can drive pickers.
//!
//! The shape describes the **record payload** of `/api/data/:entity` — system
//! columns (`id`, `version`, `owner_id`, `state`, timestamps) plus entity
//! fields and reference columns.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use mda_core::Error;
use mda_security::{Access, Identity};
use serde_json::{json, Map, Value};

use crate::auth::AuthUser;
use crate::data::entity_def;
use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/schema/:entity", get(entity_schema))
}

/// `GET /api/schema/:entity` — JSON Schema (draft 2020-12) for the entity's
/// record payload, FLS-projected per caller.
async fn entity_schema(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(entity): Path<String>,
) -> ApiResult<Json<Value>> {
    if !user.can(&entity, "read") {
        return Err(Error::Forbidden(format!("missing read on {entity}")).into());
    }
    let def = entity_def(&st, user.tenant_id, &entity).await?;
    // Reference columns carry the target entity's *name* (the same resolution
    // the form renderer uses for pickers), not its internal id.
    let mut targets: HashMap<String, String> = HashMap::new();
    for r in &def.relationships {
        if targets.contains_key(&r.source_field_name) {
            continue;
        }
        if let Ok(t) =
            mda_meta::loader::load_entity_definition(&st.pool, user.tenant_id, r.target_entity_id)
                .await
        {
            targets.insert(r.source_field_name.clone(), t.entity.name.clone());
        }
    }
    Ok(Json(build(&user, &entity, &def, &targets)))
}

/// Pure projection: entity definition + caller → JSON Schema. Unit-tested.
fn build(
    user: &Identity,
    entity: &str,
    def: &mda_meta::EntityDefinition,
    targets: &HashMap<String, String>,
) -> Value {
    let mut props = Map::new();
    let mut required: Vec<&str> = Vec::new();

    // System columns (§5.1 core schema). Read-only on every write path; only
    // `state` (workflow transitions) and `owner_id` (transfer/mass actions)
    // move, via their dedicated paths.
    for (name, mut spec) in [
        (
            "id",
            json!({"type": "string", "format": "uuid", "readOnly": true}),
        ),
        (
            "version",
            json!({"type": "integer", "readOnly": true, "description": "OCC counter (If-Match)"}),
        ),
        (
            "owner_id",
            json!({"type": ["string", "null"], "format": "uuid",
                   "description": "record owner; moves via transfer/mass actions"}),
        ),
        (
            "state",
            json!({"type": ["string", "null"],
                   "description": "workflow state; set via POST /:id/:transition"}),
        ),
        (
            "created_at",
            json!({"type": ["string", "null"], "format": "date-time", "readOnly": true}),
        ),
        (
            "updated_at",
            json!({"type": ["string", "null"], "format": "date-time", "readOnly": true}),
        ),
    ] {
        props.insert(name.to_string(), spec.take());
    }

    // Entity fields, FLS-projected.
    for f in &def.fields {
        let access = user.field_access(entity, &f.name);
        if access == Access::None {
            continue; // dropped — the schema cannot widen access
        }
        let mut spec = match f.field_type.as_str() {
            "string" | "text" | "enum" | "attachment" => json!({"type": "string"}),
            "integer" | "auto_number" => json!({"type": "integer"}),
            "decimal" | "money" => json!({"type": "number"}),
            "bool" => json!({"type": "boolean"}),
            "date" => json!({"type": "string", "format": "date"}),
            "datetime" => json!({"type": "string", "format": "date-time"}),
            // `json` accepts anything — no constraint beyond the annotation.
            _ => json!({}),
        };
        if let Some(obj) = spec.as_object_mut() {
            obj.insert("x-mda-type".into(), json!(f.field_type));
            if access == Access::Read {
                obj.insert("readOnly".into(), json!(true));
            }
            if let Some(opts) = f.config.get("options").and_then(|o| o.as_array()) {
                obj.insert("enum".into(), Value::Array(opts.to_vec()));
            }
            if let Some(label) = &f.label {
                obj.insert("title".into(), json!(label));
            }
        }
        // Server-assigned (`auto_number`) fields are never client-writable.
        if f.field_type == "auto_number" {
            if let Some(obj) = spec.as_object_mut() {
                obj.insert("readOnly".into(), json!(true));
            }
        }
        props.insert(f.name.clone(), spec);
        if f.required && f.default_expr.is_none() && f.field_type != "auto_number" {
            required.push(&f.name);
        }
    }

    // Reference columns (relationships hoisted to FK columns, §5.1/§5.7).
    for r in &def.relationships {
        if user.field_access(entity, &r.source_field_name) == Access::None {
            continue;
        }
        let mut spec = json!({
            "type": ["string", "null"],
            "format": "uuid",
            "x-mda-type": "reference",
            "x-mda-on-delete": r.on_delete,
        });
        if let Some(t) = targets.get(&r.source_field_name) {
            spec["x-mda-target-entity"] = json!(t);
        }
        props.insert(r.source_field_name.clone(), spec);
    }

    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("/api/schema/{entity}"),
        "title": def.entity.label.clone().unwrap_or_else(|| entity.to_string()),
        "type": "object",
        "additionalProperties": false,
        "properties": Value::Object(props),
        "required": required,
        "x-mda": {
            "entity": entity,
            "table": def.entity.table_name,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use uuid::Uuid;

    fn identity(field_perms: HashMap<(String, String), Access>) -> Identity {
        Identity::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            None,
            HashSet::from([("Customer".to_string(), "read".to_string())]),
            field_perms,
        )
    }

    fn def() -> mda_meta::EntityDefinition {
        serde_json::from_value(json!({
            "entity": {"id": Uuid::new_v4(), "tenant_id": Uuid::new_v4(), "module_id": null,
                        "table_name": "customer_x", "name": "Customer", "label": "Customer",
                        "description": null, "status": "active",
                        "created_at": "2025-01-01T00:00:00Z", "updated_at": "2025-01-01T00:00:00Z"},
            "fields": [
              {"id": Uuid::new_v4(), "tenant_id": Uuid::new_v4(), "entity_id": Uuid::new_v4(),
               "name": "name", "label": "Name", "field_type": "string", "required": true,
               "is_unique": false, "is_indexed": false, "default_expr": null, "config": {},
               "status": "active", "created_at": "2025-01-01T00:00:00Z", "updated_at": "2025-01-01T00:00:00Z"},
              {"id": Uuid::new_v4(), "tenant_id": Uuid::new_v4(), "entity_id": Uuid::new_v4(),
               "name": "tier", "label": "Tier", "field_type": "enum", "required": false,
               "is_unique": false, "is_indexed": false, "default_expr": null,
               "config": {"options": ["Bronze", "Silver"]},
               "status": "active", "created_at": "2025-01-01T00:00:00Z", "updated_at": "2025-01-01T00:00:00Z"},
              {"id": Uuid::new_v4(), "tenant_id": Uuid::new_v4(), "entity_id": Uuid::new_v4(),
               "name": "salary", "label": "Salary", "field_type": "money", "required": false,
               "is_unique": false, "is_indexed": false, "default_expr": null, "config": {},
               "status": "active", "created_at": "2025-01-01T00:00:00Z", "updated_at": "2025-01-01T00:00:00Z"},
              {"id": Uuid::new_v4(), "tenant_id": Uuid::new_v4(), "entity_id": Uuid::new_v4(),
               "name": "code", "label": "Code", "field_type": "auto_number", "required": true,
               "is_unique": false, "is_indexed": false, "default_expr": null,
               "config": {"prefix": "C-"},
               "status": "active", "created_at": "2025-01-01T00:00:00Z", "updated_at": "2025-01-01T00:00:00Z"}
            ],
            "relationships": [
              {"id": Uuid::new_v4(), "tenant_id": Uuid::new_v4(),
               "source_entity_id": Uuid::new_v4(), "source_field_name": "ref_region_id",
               "target_entity_id": Uuid::new_v4(), "cardinality": "many_to_one",
               "strength": "lookup", "on_delete": "set_null", "required": false,
               "reference_qualifier": null, "rollup_summary": null,
               "created_at": "2025-01-01T00:00:00Z", "updated_at": "2025-01-01T00:00:00Z"}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn schema_maps_types_and_required() {
        let targets = HashMap::from([("ref_region_id".to_string(), "Region".to_string())]);
        let s = build(&identity(HashMap::new()), "Customer", &def(), &targets);
        assert_eq!(s["$schema"], "https://json-schema.org/draft/2020-12/schema");
        assert_eq!(s["properties"]["name"]["type"], "string");
        assert_eq!(s["properties"]["tier"]["enum"], json!(["Bronze", "Silver"]));
        assert_eq!(s["properties"]["tier"]["x-mda-type"], "enum");
        assert_eq!(s["properties"]["salary"]["type"], "number");
        // auto_number: integer, server-assigned, exempt from required
        assert_eq!(s["properties"]["code"]["type"], "integer");
        assert_eq!(s["properties"]["code"]["readOnly"], true);
        assert_eq!(s["required"], json!(["name"]));
        // system columns present and read-only where immutable
        assert_eq!(s["properties"]["id"]["format"], "uuid");
        assert_eq!(s["properties"]["id"]["readOnly"], true);
        assert_eq!(s["properties"]["version"]["readOnly"], true);
        // reference column carries its target + on_delete
        assert_eq!(s["properties"]["ref_region_id"]["x-mda-type"], "reference");
        assert_eq!(
            s["properties"]["ref_region_id"]["x-mda-on-delete"],
            "set_null"
        );
        assert_eq!(
            s["properties"]["ref_region_id"]["x-mda-target-entity"],
            "Region"
        );
    }

    #[test]
    fn fls_none_dropped_and_read_is_readonly() {
        let perms: HashMap<(String, String), Access> = [
            (("Customer".into(), "salary".into()), Access::None),
            (("Customer".into(), "tier".into()), Access::Read),
        ]
        .into_iter()
        .collect();
        let s = build(&identity(perms), "Customer", &def(), &HashMap::new());
        assert!(
            s["properties"].get("salary").is_none(),
            "FLS none must be dropped"
        );
        assert_eq!(s["properties"]["tier"]["readOnly"], true);
        assert_eq!(s["properties"]["name"].get("readOnly"), None);
    }
}
