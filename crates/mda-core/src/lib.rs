//! `mda-core` — shared primitives: error types, identifiers, traits.
//!
//! Foundational types used by every other crate. Kept deliberately free of I/O
//! and database dependencies (PLAN §6) so the core stays portable and testable.

pub mod error;
pub mod id;

pub use error::{Error, Result};
pub use id::Id;
