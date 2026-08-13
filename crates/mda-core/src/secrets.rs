//! Secrets management trait (PLAN §5.20).
//!
//! Connectors (§4.6), integrations (§5.22), and outbound channels (§5.18) need
//! credentials per tenant that must NEVER live as plaintext in metadata. The
//! reference (`sys_secret`) lives in Postgres; the **value** is resolved at
//! runtime from an external [`SecretStore`].
//!
//! Contract for any implementation:
//! - values are resolved **server-side only**, at the moment a connector/channel
//!   runs, under that connector's authz;
//! - values are **never** returned by any API, **never logged** (`tracing` must
//!   redact known-sensitive fields), and **never serialized** into events,
//!   audit, or outbox payloads;
//! - every resolution is audited by the *caller* (the store is agnostic to the
//!   DB), recording who/when/which/purpose.
//!
//! The trait carries no I/O of its own (mirrors the core "no DB pool" rule); the
//! `resolve` impl may read the environment, a file, or call a cloud KMS.

use crate::Result;

/// A tenant-scoped secret value store. Thread-safe (`Send + Sync`) so it can be
/// shared in application state and resolved concurrently from workers/handlers.
///
/// `resolve` takes the store-specific **ref** (the value stored in
/// `sys_secret.ref`) — NOT the modeler-facing name — because the mapping from
/// `(tenant, name) → ref` lives in the DB. Returns `Ok(None)` when no value is
/// known for the ref (the caller decides whether a missing secret is fatal).
pub trait SecretStore: Send + Sync {
    fn resolve(&self, store_ref: &str) -> Result<Option<Vec<u8>>>;
}
