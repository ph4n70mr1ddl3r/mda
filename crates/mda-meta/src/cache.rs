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
        let mut initialized = false;
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
                    if initialized {
                        changed = true;
                    }
                    last.insert(*t, *v);
                }
            }
            initialized = true;
            if changed {
                tracing::debug!("version-stamp poll detected change → invalidate cache");
                cache.invalidate_all();
            }
        }
    });
}
