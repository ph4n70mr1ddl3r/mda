//! Criteria-based sharing rules (ADR-0013) — materialized record visibility.
//!
//! A sharing rule (`sec.sec_share_rule`) names an entity, a **condition** in the
//! bounded expression DSL, a **principal** (user or team) and an `access` level.
//! Records matching the condition are materialized into
//! `sec.sec_record_share` rows carrying the rule's `epoch`; enforcement
//! (crud.rs `read_predicate`/`write_predicate`) honors a rule-derived share
//! **only while its epoch equals the rule's current epoch** — so bumping the
//! epoch instantly revokes every share computed under the old one (revoke-safe
//! invalidation) without touching the materialized table.
//!
//! Per ADR-0013 the recompute is **split by trigger**:
//!  - *record write* → [`recompute_record`] runs **synchronously, inside the
//!    write transaction** (bounded: O(active rules) for one record) — a record's
//!    own shares are always fresh right after its write; there is no per-record
//!    revocation lag at all.
//!  - *admin rule change* → the epoch is bumped (create does **not** bump — a
//!    purely additive grant can never revoke, ADR-0013 rule 3) and the rule is
//!    re-materialized in bounded keyset batches by the admin API
//!    (`POST /api/admin/share-rules/:id/recompute`). Grant-side catch-up is
//!    progressive; the epoch already guarantees revoke-side correctness.

use mda_core::{Error, Result};
use mda_expression::{eval, Expr, Registry};
use serde_json::Value;
use uuid::Uuid;

/// A sharing-rule row (`sec.sec_share_rule`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ShareRule {
    pub id: Uuid,
    pub entity: String,
    pub condition: Value,
    pub principal_id: Uuid,
    pub access: String,
    pub epoch: i64,
    pub active: bool,
}

impl ShareRule {
    /// Does this rule's condition match `record` (the reconstructed after-image,
    /// the same shape rules/calculated fields evaluate against)?
    pub fn matches(&self, record: &Value, reg: &Registry) -> bool {
        let Ok(expr) = Expr::from_json(&self.condition) else {
            // An unparsable condition can never *widen* access: treat as
            // no-match (under-grant is safe, over-grant is not).
            return false;
        };
        match eval(&expr, record, reg) {
            Ok(v) => mda_expression::truth(&v),
            Err(_) => false,
        }
    }
}

/// Load the tenant's active sharing rules for one entity (in-rule order).
/// Runs inside the caller's transaction (already under the tenant GUC).
pub async fn load_rules(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    entity: &str,
) -> Result<Vec<ShareRule>> {
    let rules: Vec<ShareRule> = sqlx::query_as(
        "SELECT id, entity, condition, principal_id, access, epoch, active
           FROM sec.sec_share_rule
          WHERE tenant_id = $1 AND entity = $2 AND active
          ORDER BY created_at, id",
    )
    .bind(tenant)
    .bind(entity)
    .fetch_all(&mut **tx)
    .await
    .map_err(Error::internal)?;
    Ok(rules)
}

/// Synchronously recompute **one record's** rule-derived shares, in the write
/// transaction. Manual shares (`rule_id IS NULL`) are never touched: they are
/// managed via `POST /api/shares/...` and win over a colliding rule share
/// (`ON CONFLICT DO NOTHING` keeps the manual grant's access level).
pub async fn recompute_record(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    entity: &str,
    record_id: Uuid,
    record: &Value,
) -> Result<()> {
    let rules = load_rules(tx, tenant, entity).await?;
    // Drop this record's rule-derived shares, then re-insert the current
    // matches at the rules' current epochs. One statement either way; the
    // transaction makes the swap atomic with the write that triggered it.
    sqlx::query("DELETE FROM sec.sec_record_share WHERE tenant_id = $1 AND record_id = $2 AND rule_id IS NOT NULL")
        .bind(tenant)
        .bind(record_id)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
    let reg = Registry::new();
    for r in &rules {
        if !r.matches(record, &reg) {
            continue;
        }
        sqlx::query(
            "INSERT INTO sec.sec_record_share
                 (tenant_id, entity, record_id, principal_id, access, rule_id, epoch)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (tenant_id, record_id, principal_id) DO NOTHING",
        )
        .bind(tenant)
        .bind(entity)
        .bind(record_id)
        .bind(r.principal_id)
        .bind(&r.access)
        .bind(r.id)
        .bind(r.epoch)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
    }
    Ok(())
}

/// Remove every share row for a hard-deleted record (the materialized table has
/// no FK to dynamic `biz.*` tables, so cleanup is explicit — ADR-0006).
pub async fn drop_record_shares(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    record_id: Uuid,
) -> Result<()> {
    sqlx::query("DELETE FROM sec.sec_record_share WHERE tenant_id = $1 AND record_id = $2")
        .bind(tenant)
        .bind(record_id)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
    Ok(())
}

/// Bump a rule's epoch (instant revoke of every share computed under the old
/// epoch — ADR-0013 rule 1). Called on **edit** (condition/access/principal)
/// and on deactivate; a freshly **created** rule never bumps (additive grant).
pub async fn bump_epoch(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    rule_id: Uuid,
) -> Result<i64> {
    let (epoch,): (i64,) = sqlx::query_as(
        "UPDATE sec.sec_share_rule SET epoch = epoch + 1
          WHERE tenant_id = $1 AND id = $2 RETURNING epoch",
    )
    .bind(tenant)
    .bind(rule_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(Error::internal)?
    .ok_or_else(|| Error::NotFound(format!("share rule {rule_id}")))?;
    Ok(epoch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(condition: Value) -> ShareRule {
        ShareRule {
            id: Uuid::new_v4(),
            entity: "Deal".into(),
            condition,
            principal_id: Uuid::new_v4(),
            access: "read".into(),
            epoch: 1,
            active: true,
        }
    }

    #[test]
    fn rule_matches_on_condition() {
        let r = rule(json!({
            "op":"Cmp","kind":"gt",
            "lhs":{"op":"Field","name":"amount"},
            "rhs":{"op":"Lit","value":100}
        }));
        let rec = json!({"amount": 250});
        assert!(r.matches(&rec, &Registry::new()));
        let rec = json!({"amount": 50});
        assert!(!r.matches(&rec, &Registry::new()));
    }

    #[test]
    fn malformed_condition_never_grants() {
        let r = rule(json!({"op":"NotAnOp"}));
        let rec = json!({"amount": 250});
        assert!(
            !r.matches(&rec, &Registry::new()),
            "under-grant, never over-grant"
        );
    }
}
