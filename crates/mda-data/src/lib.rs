//! `mda-data` — dynamic data access (PLAN §5.1 / §5.7 / §5.9).
//!
//! Storage model realized:
//! - every entity publishes to a real table `biz.<table>`;
//! - **reference fields are real typed columns with native `FOREIGN KEY`s**
//!   (always hoisted — the whole point of Pattern B);
//! - **unique/indexed scalar fields are GENERATED columns** derived from the
//!   `attributes JSONB` payload (single source of truth — no dual-write);
//! - all other scalar fields live in `attributes JSONB`.
//!
//! CRUD therefore writes only `attributes` + the FK columns; the generated
//! columns populate themselves and carry the `UNIQUE`/index constraints.

pub mod coerce;
pub mod crud;
pub mod ddl;
pub mod sharing;

pub use crud::{
    create, delete, list, mass_target_ids, pred_render, read, read_predicate, reconstruct, restore,
    update, write_predicate, Filter, ListParams, ListResult, RecordScope, Sort, MAX_PAGE_SIZE,
};
pub use sharing::{bump_epoch, drop_record_shares, load_rules, recompute_record, ShareRule};
