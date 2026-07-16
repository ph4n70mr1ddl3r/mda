//! `mda-security` — authentication & authorization (PLAN §5.11).
//!
//! Phase 3 implements the **tenant, object, field, record(ownership/OWD)**
//! grains. Record sharing rules / role hierarchy / materialized `sec_record_share`
//! arrive in Phase 6 (ADR-0013); Postgres RLS is a defense-in-depth follow-up.

pub mod context;
pub mod identity;
pub mod jwt;
pub mod password;

pub use context::{load_identity, resolve_owd};
pub use identity::{Access, Identity, Owd};
pub use jwt::{verify_access_token, AccessToken, JwtConfig};
pub use password::{hash_password, verify_password};
