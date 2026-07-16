//! `mda-meta` — the metadata model: typed structs for the `meta.md_*` tables
//! and (in Phase 1) the loader + in-memory cache (`moka`) with LISTEN/NOTIFY
//! invalidation.
//!
//! This is the *fixed* meta-model (ADR-0008): `md_entity`, `md_field`, etc.
//! are static Rust structs + SQL, not first-class runtime entities.
//!
//! Phase 0 ships only the type definitions matching the initial migration
//! (`migrations/20260101000001_init_meta.sql`). Loading, caching, and the
//! draft → publish lifecycle land in Phase 1.

pub mod model;

pub use model::{Entity, Field, Module, Relationship};
