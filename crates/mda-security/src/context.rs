//! Effective-context loader: build an [`Identity`] from the `sec_*` tables.

use std::collections::{HashMap, HashSet};

use mda_core::{Error, Result};
use uuid::Uuid;

use crate::identity::{Access, Identity, Owd};

/// Load the effective identity (roles -> object perms + field perms) for a user.
/// `tenant` is the verified JWT's tenant claim — `sec_user` is RLS-gated, so the
/// lookup runs under that tenant's GUC. A JWT whose (sub, tenant) don't match a
/// real user-in-that-tenant fails closed (NotFound).
pub async fn load_identity(pool: &sqlx::PgPool, user_id: Uuid, tenant: Uuid) -> Result<Identity> {
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    crate::set_tenant(&mut tx, tenant).await?;
    let (tenant_id, team_id): (Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT tenant_id, team_id FROM sec.sec_user WHERE id = $1 AND active = TRUE",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?
    .ok_or_else(|| Error::NotFound(format!("user {user_id}")))?;

    // object permissions across all the user's roles
    let perms: Vec<(String, String)> = sqlx::query_as(
        "SELECT p.entity, p.verb
           FROM sec.sec_permission p
           JOIN sec.sec_role_assignment a ON a.role_id = p.role_id
          WHERE a.user_id = $1",
    )
    .bind(user_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    let object_perms: HashSet<(String, String)> = perms.into_iter().collect();

    // field permissions (reduce to the most permissive access per (entity, field))
    let fps: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT fp.entity, fp.field, fp.access
           FROM sec.sec_field_permission fp
           JOIN sec.sec_role_assignment a ON a.role_id = fp.role_id
          WHERE a.user_id = $1",
    )
    .bind(user_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    let mut field_perms: HashMap<(String, String), Access> = HashMap::new();
    for (entity, field, access) in fps {
        let a = Access::parse(&access);
        field_perms
            .entry((entity, field))
            .and_modify(|prev| {
                if (a as u8) > (*prev as u8) {
                    *prev = a;
                }
            })
            .or_insert(a);
    }

    Ok(Identity::new(
        user_id,
        tenant_id,
        team_id,
        object_perms,
        field_perms,
    ))
}

/// Resolve the OWD for an entity (default Private). `sec_owd` is RLS-gated by
/// tenant, so this runs under the tenant GUC.
pub async fn resolve_owd(pool: &sqlx::PgPool, tenant: Uuid, entity: &str) -> Result<Owd> {
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    crate::set_tenant(&mut tx, tenant).await?;
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT default_access FROM sec.sec_owd WHERE tenant_id = $1 AND entity = $2",
    )
    .bind(tenant)
    .bind(entity)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(row.map(|(d,)| Owd::parse(&d)).unwrap_or(Owd::Private))
}
