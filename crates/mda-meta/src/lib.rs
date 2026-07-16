//! `mda-meta` — the metadata model: typed structs for the `meta.md_*` tables,
//! the loader + in-memory cache (`moka`) with LISTEN/NOTIFY invalidation, and
//! the draft → publish lifecycle's pure (DB-free) diff/validate logic.
//!
//! This is the *fixed* meta-model (ADR-0008): `md_entity`, `md_field`, etc. are
//! static Rust structs + SQL, not first-class runtime entities.

pub mod cache;
pub mod definition;
pub mod draft;
pub mod loader;
pub mod model;

pub use cache::MetadataCache;
pub use definition::EntityDefinition;
pub use draft::{
    diff, DiffReport, DraftEntity, DraftField, DraftModel, DraftModule, DraftRelationship,
};
pub use model::{Entity, Field, Module, Relationship};
