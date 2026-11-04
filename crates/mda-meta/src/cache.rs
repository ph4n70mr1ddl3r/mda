//! The metadata cache (PLAN §5.3).
//!
//! - Keyed by `(tenant_id, entity_id)` — never by entity id alone, so the cache
//!   cannot leak across tenants.
//! - Read-through: a miss loads via [`crate::loader`].
//! - Invalidation: Postgres `LISTEN meta_changed` is the fast path; a low-rate
//!   **version-stamp poll** against `md_active_version` is the self-healing
//!   fallback (NOTIFY is lossy across reconnects/replicas).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use mda_core::{Error, Result};
use moka::future::Cache;
use sqlx::postgres::PgListener;
use uuid::Uuid;

use crate::definition::EntityDefinition;
use crate::loader;

/// In-memory metadata cache (entity definitions), tenant-scoped keys.
#[derive(Clone)]
pub struct MetadataCache {
    entities: Cache<(Uuid, Uuid), Arc<EntityDefinition>>,
}

impl MetadataCache {
    pub fn new() -> Self {
        let entities = Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(600))
            .build();
        Self { entities }
    }

    /// Get a loaded entity definition, loading from the DB on a miss.
    pub async fn get_entity(
        &self,
        pool: &sqlx::PgPool,
        tenant: Uuid,
        entity_id: Uuid,
    ) -> Result<Arc<EntityDefinition>> {
        self.entities
            .try_get_with((tenant, entity_id), async move {
                loader::load_entity_definition(pool, tenant, entity_id)
                    .await
                    .map(Arc::new)
            })
            .await
            .map_err(|e| Error::internal(anyhow::anyhow!("metadata cache load failed: {e}")))
    }

    /// Drop all cached entries (after a publish / meta_changed notification).
    pub fn invalidate_all(&self) {
        self.entities.invalidate_all();
    }

    pub fn entry_count(&self) -> u64 {
        self.entities.entry_count()
    }
}

impl Default for MetadataCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawn the fast-path invalidator: `LISTEN meta_changed` → `invalidate_all`.
pub fn spawn_listen(pool: sqlx::PgPool, cache: MetadataCache) {
    tokio::spawn(async move {
        let mut listener = loop {
            match PgListener::connect_with(&pool).await {
                Ok(l) => break l,
                Err(e) => {
                    tracing::warn!(?e, "pg listener connect failed; poll fallback covers this");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        };
        loop {
            if let Err(e) = listener.listen("meta_changed").await {
                tracing::warn!(?e, "pg listen failed; retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            tracing::info!("LISTEN meta_changed (cache invalidator)");
            loop {
                match listener.recv().await {
                    Ok(n) => {
                        tracing::debug!(payload = %n.payload(), "meta_changed → invalidate cache");
                        cache.invalidate_all();
                    }
                    Err(e) => {
                        tracing::warn!(?e, "pg listener recv error; reconnecting");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        break; // reconnect outer loop
                    }
                }
            }
        }
    });
}

/// Spawn the self-healing fallback: poll `md_active_version` and invalidate if
/// any tenant's version advanced (covers lossy/missed NOTIFY).
pub fn spawn_poll(pool: sqlx::PgPool, cache: MetadataCache) {
    tokio::spawn(async move {
        let mut last: HashMap<Uuid, i64> = HashMap::new();
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let rows: Vec<(Uuid, i64)> =
                match sqlx::query_as("SELECT tenant_id, version FROM meta.md_active_version")
                    .fetch_all(&pool)
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(?e, "version-stamp poll failed");
                        continue;
                    }
                };
            let mut changed = false;
            for (t, v) in &rows {
                if last.get(t) != Some(v) {
                    changed = true;
                }
                last.insert(*t, *v);
            }
            if changed {
                tracing::debug!("version-stamp poll detected change → invalidate cache");
                cache.invalidate_all();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use mda_core::Error;
    use serde_json::Value;
    use uuid::Uuid;

    #[test]
    fn first_poll_detects_change_when_last_empty() {
        let tenant = Uuid::new_v4();
        let mut last: HashMap<Uuid, i64> = HashMap::new();
        let rows: Vec<(Uuid, i64)> = vec![(tenant, 5)];
        let mut changed = false;
        for (t, v) in &rows {
            if last.get(t) != Some(v) {
                changed = true;
            }
            last.insert(*t, *v);
        }
        assert!(changed, "first poll should detect change when last is empty");
        assert_eq!(last.get(&tenant), Some(&5));
    }

    #[test]
    fn first_poll_no_change_when_versions_same() {
        let tenant = Uuid::new_v4();
        let mut last: HashMap<Uuid, i64> = HashMap::new();
        let rows: Vec<(Uuid, i64)> = vec![(tenant, 5)];
        let mut changed = false;
        for (t, v) in &rows {
            if last.get(t) != Some(v) {
                changed = true;
            }
            last.insert(*t, *v);
        }
        // Re-run with same version - changed should be false
        let mut changed2 = false;
        for (t, v) in &rows {
            if last.get(t) != Some(v) {
                changed2 = true;
            }
            last.insert(*t, *v);
        }
        assert!(!changed2, "same version on re-check should not trigger change");
    }

    #[test]
    fn version_advance_detected_on_subsequent_poll() {
        let tenant = Uuid::new_v4();
        let mut last: HashMap<Uuid, i64> = HashMap::new();
        // First poll: version 5
        let rows1: Vec<(Uuid, i64)> = vec![(tenant, 5)];
        let mut changed = false;
        for (t, v) in &rows1 {
            if last.get(t) != Some(v) {
                changed = true;
            }
            last.insert(*t, *v);
        }
        assert!(changed, "first poll should detect initial version");
        // Second poll: version advanced to 7
        let rows2: Vec<(Uuid, i64)> = vec![(tenant, 7)];
        let mut changed2 = false;
        for (t, v) in &rows2 {
            if last.get(t) != Some(v) {
                changed2 = true;
            }
            last.insert(*t, *v);
        }
        assert!(changed2, "subsequent poll should detect version advance from 5 to 7");
    }

    #[test]
    fn multiple_tenants_some_change() {
        let tenant1 = Uuid::new_v4();
        let tenant2 = Uuid::new_v4();
        let mut last: HashMap<Uuid, i64> = HashMap::new();
        let rows: Vec<(Uuid, i64)> = vec![(tenant1, 3), (tenant2, 5)];
        let mut changed = false;
        for (t, v) in &rows {
            if last.get(t) != Some(v) {
                changed = true;
            }
            last.insert(*t, *v);
        }
        assert!(changed, "should detect change when any tenant's version changes");
        assert_eq!(last.get(&tenant1), Some(&3));
        assert_eq!(last.get(&tenant2), Some(&5));
    }

    #[test]
    fn no_rows_means_no_change() {
        let tenant = Uuid::new_v4();
        let mut last: HashMap<Uuid, i64> = HashMap::new();
        let rows: Vec<(Uuid, i64)> = vec![];
        let mut changed = false;
        for (t, v) in &rows {
            if last.get(t) != Some(v) {
                changed = true;
            }
            last.insert(*t, *v);
        }
        assert!(!changed, "no rows should not trigger change");
    }
}
