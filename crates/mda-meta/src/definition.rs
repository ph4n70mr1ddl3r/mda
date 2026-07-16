//! A loaded entity definition: the entity row plus its fields and relationships.

use crate::model::{Entity, Field, Relationship};

/// A fully-loaded entity: the row plus all its fields and (outbound) relationships.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EntityDefinition {
    pub entity: Entity,
    pub fields: Vec<Field>,
    pub relationships: Vec<Relationship>,
}
