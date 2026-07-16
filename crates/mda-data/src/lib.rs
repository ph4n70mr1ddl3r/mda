//! `mda-data` — dynamic data access: the query builder + CRUD over the
//! generated `biz.<table>` tables (hoisted columns + JSONB `attributes`).
//!
//! **Not built in Phase 0.** This crate exists to fix the workspace boundary
//! early; the DDL/migration engine + generic CRUD + list/query land in Phase 2
//! (PLAN §9).
