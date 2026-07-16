//! DB loaders for the metadata model. Read-only; writes happen at publish time
//! (see the Studio service in `mda-api`).

use mda_core::{Error, Result};
use uuid::Uuid;

use crate::definition::EntityDefinition;
use crate::draft::{DraftEntity, DraftField, DraftModel, DraftModule, DraftRelationship};
use crate::model::{Entity, Field, Module, Relationship};

/// Load the full active model for a tenant as a `DraftModel` (used by branch,
/// export, snapshot archival, and diff).
pub async fn load_active_model(pool: &sqlx::PgPool, tenant: Uuid) -> Result<DraftModel> {
    let modules: Vec<Module> = sqlx::query_as::<_, Module>(
        "SELECT id, tenant_id, name, label, created_at, updated_at FROM meta.md_module WHERE tenant_id = $1 ORDER BY name",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await
    .map_err(Error::internal)?;

    let entities: Vec<Entity> = sqlx::query_as::<_, Entity>(
        "SELECT id, tenant_id, module_id, table_name, name, label, description, status, created_at, updated_at
         FROM meta.md_entity WHERE tenant_id = $1 AND status = 'active' ORDER BY name",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await
    .map_err(Error::internal)?;

    let fields: Vec<Field> = sqlx::query_as::<_, Field>(
        "SELECT id, tenant_id, entity_id, name, label, field_type, required, is_unique, is_indexed,
                default_expr, config, status, created_at, updated_at
         FROM meta.md_field WHERE tenant_id = $1 AND status = 'active'",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await
    .map_err(Error::internal)?;

    let rels: Vec<Relationship> = sqlx::query_as::<_, Relationship>(
        "SELECT id, tenant_id, source_entity_id, source_field_name, target_entity_id, cardinality,
                strength, on_delete, required, reference_qualifier, rollup_summary, created_at, updated_at
         FROM meta.md_relationship WHERE tenant_id = $1",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await
    .map_err(Error::internal)?;

    let model = assemble_draft_model(modules, entities, fields, rels);
    Ok(model)
}

/// Load a single entity definition (entity + its fields + outbound relationships).
pub async fn load_entity_definition(
    pool: &sqlx::PgPool,
    tenant: Uuid,
    entity_id: Uuid,
) -> Result<EntityDefinition> {
    let entity = sqlx::query_as::<_, Entity>(
        "SELECT id, tenant_id, module_id, table_name, name, label, description, status, created_at, updated_at
         FROM meta.md_entity WHERE tenant_id = $1 AND id = $2 AND status = 'active'",
    )
    .bind(tenant)
    .bind(entity_id)
    .fetch_optional(pool)
    .await
    .map_err(Error::internal)?
    .ok_or_else(|| Error::NotFound(format!("entity {entity_id}")))?;

    let fields = sqlx::query_as::<_, Field>(
        "SELECT id, tenant_id, entity_id, name, label, field_type, required, is_unique, is_indexed,
                default_expr, config, status, created_at, updated_at
         FROM meta.md_field WHERE tenant_id = $1 AND entity_id = $2 AND status = 'active'",
    )
    .bind(tenant)
    .bind(entity_id)
    .fetch_all(pool)
    .await
    .map_err(Error::internal)?;

    let relationships = sqlx::query_as::<_, Relationship>(
        "SELECT id, tenant_id, source_entity_id, source_field_name, target_entity_id, cardinality,
                strength, on_delete, required, reference_qualifier, rollup_summary, created_at, updated_at
         FROM meta.md_relationship WHERE tenant_id = $1 AND source_entity_id = $2",
    )
    .bind(tenant)
    .bind(entity_id)
    .fetch_all(pool)
    .await
    .map_err(Error::internal)?;

    Ok(EntityDefinition {
        entity,
        fields,
        relationships,
    })
}

/// The current active model version for a tenant (`md_active_version.version`).
pub async fn active_version(pool: &sqlx::PgPool, tenant: Uuid) -> Result<i64> {
    let v: Option<(i64,)> =
        sqlx::query_as("SELECT version FROM meta.md_active_version WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_optional(pool)
            .await
            .map_err(Error::internal)?;
    Ok(v.map(|(v,)| v).unwrap_or(0))
}

/// Resolve an active entity's id by (tenant, name) — the runtime data API
/// (`/api/data/:entity`) addresses entities by name.
pub async fn entity_id_by_name(pool: &sqlx::PgPool, tenant: Uuid, name: &str) -> Result<Uuid> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM meta.md_entity WHERE tenant_id = $1 AND name = $2 AND status = 'active'",
    )
    .bind(tenant)
    .bind(name)
    .fetch_optional(pool)
    .await
    .map_err(Error::internal)?;
    row.map(|(id,)| id)
        .ok_or_else(|| Error::NotFound(format!("entity {name}")))
}

/// All active entity ids for a tenant (for cache invalidation / enumeration).
pub async fn entity_ids_for_tenant(pool: &sqlx::PgPool, tenant: Uuid) -> Result<Vec<Uuid>> {
    let rows: Vec<(Uuid,)> =
        sqlx::query_as("SELECT id FROM meta.md_entity WHERE tenant_id = $1 AND status = 'active'")
            .bind(tenant)
            .fetch_all(pool)
            .await
            .map_err(Error::internal)?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

// ===== assembly =====

fn assemble_draft_model(
    modules: Vec<Module>,
    entities: Vec<Entity>,
    fields: Vec<Field>,
    rels: Vec<Relationship>,
) -> DraftModel {
    let mut model = DraftModel {
        modules: modules
            .into_iter()
            .map(|m| DraftModule {
                id: m.id,
                name: m.name,
                label: m.label,
            })
            .collect(),
        entities: Vec::new(),
    };

    for e in entities {
        let my_fields: Vec<DraftField> = fields
            .iter()
            .filter(|f| f.entity_id == e.id)
            .map(|f| DraftField {
                id: f.id,
                name: f.name.clone(),
                label: f.label.clone(),
                field_type: f.field_type.clone(),
                required: f.required,
                is_unique: f.is_unique,
                is_indexed: f.is_indexed,
                default_expr: f.default_expr.clone(),
                config: f.config.clone(),
            })
            .collect();
        let my_rels: Vec<DraftRelationship> = rels
            .iter()
            .filter(|r| r.source_entity_id == e.id)
            .map(|r| DraftRelationship {
                id: r.id,
                source_field_name: r.source_field_name.clone(),
                target_entity_id: r.target_entity_id,
                cardinality: r.cardinality.clone(),
                strength: r.strength.clone(),
                on_delete: r.on_delete.clone(),
                required: r.required,
                reference_qualifier: r.reference_qualifier.clone(),
                rollup_summary: r.rollup_summary.clone(),
            })
            .collect();
        model.entities.push(DraftEntity {
            id: e.id,
            module_id: e.module_id,
            name: e.name,
            table_name: e.table_name,
            label: e.label,
            description: e.description,
            fields: my_fields,
            relationships: my_rels,
        });
    }
    model
}
