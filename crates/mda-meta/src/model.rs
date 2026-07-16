//! Canonical metadata structs (skeleton). The field set mirrors the initial
//! `meta.md_*` schema; the loader + `MetadataCache` arrive in Phase 1.

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// A logical grouping of entities (e.g. "CRM", "HR").
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize, serde::Deserialize)]
pub struct Module {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A business object definition.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize, serde::Deserialize)]
pub struct Entity {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub module_id: Option<Uuid>,
    pub table_name: String,
    pub name: String,
    pub label: Option<String>,
    pub description: Option<String>,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A field (attribute) definition.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize, serde::Deserialize)]
pub struct Field {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub entity_id: Uuid,
    pub name: String,
    pub label: Option<String>,
    pub field_type: String,
    pub required: bool,
    pub is_unique: bool,
    pub is_indexed: bool,
    pub default_expr: Option<serde_json::Value>,
    pub config: serde_json::Value,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A relationship between entities (PLAN §5.7).
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize, serde::Deserialize)]
pub struct Relationship {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub source_entity_id: Uuid,
    pub source_field_name: String,
    pub target_entity_id: Uuid,
    pub cardinality: String,
    pub strength: String,
    pub on_delete: Option<String>,
    pub required: bool,
    pub reference_qualifier: Option<serde_json::Value>,
    pub rollup_summary: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
