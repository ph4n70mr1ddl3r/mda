//! `mda-rules` — business rule engine (PLAN §4.3 / §5.9).
//!
//! Phase 4 supports **set-field** rules firing synchronously: on a create/update
//! the matching rules' conditions are evaluated (bounded DSL) and their
//! `set_field` actions produce derived field values applied in the same write.
//! Calculated fields (a field whose config carries a `formula`) are computed the
//! same way. More action kinds + async outbox side-effects arrive later.
//!
//! Cycle detection: a rule cascade is limited to [`MAX_RULE_FIRINGS`] per
//! `fire()` call. If exceeded the engine returns an error to prevent infinite
//! loops from self-referential rule sets.

pub use mda_expression::Registry;

use mda_core::{Error, Result};
use mda_expression::{eval, Expr};
use mda_meta::EntityDefinition;
use serde_json::{Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

/// Maximum number of rule actions that may fire in a single `fire()` call.
/// Guards against infinite rule cycles (e.g. A sets X → B sets Y → A sets X …).
const MAX_RULE_FIRINGS: u32 = 50;

/// A business rule row.
#[derive(sqlx::FromRow)]
pub struct Rule {
    #[allow(dead_code)]
    pub id: Uuid,
    pub entity: String,
    pub event: String,
    pub condition: Value,
    pub action_type: String,
    pub action_field: Option<String>,
    pub action_value: Option<Value>,
    #[allow(dead_code)]
    pub priority: i32,
}

/// Load active rules for `(tenant, entity)`, ordered by priority.
pub async fn load_active(pool: &PgPool, tenant: Uuid, entity: &str) -> Result<Vec<Rule>> {
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant.to_string())
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
    let rules: Vec<Rule> = sqlx::query_as(
        "SELECT id, entity, event, condition, action_type, action_field, action_value, priority
           FROM meta.md_rule
          WHERE tenant_id = $1 AND entity = $2 AND active = TRUE
          ORDER BY priority, id",
    )
    .bind(tenant)
    .bind(entity)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(rules)
}

/// Fire the rules matching `event` against `ctx`; return the field assignments
/// (field -> value) from set_field actions whose condition is true. `ctx` is
/// mutated in place with each applied assignment so later rules see earlier ones.
///
/// Stops with an error if more than [`MAX_RULE_FIRINGS`] actions fire, which
/// indicates a likely cycle in the rule set.
pub fn fire(
    rules: &[Rule],
    event: &str,
    ctx: &mut Map<String, Value>,
    reg: &Registry,
) -> Result<()> {
    let mut fired: u32 = 0;
    for r in rules {
        if r.event != event || r.action_type != "set_field" {
            continue;
        }
        let cond = Expr::from_json(&r.condition)?;
        if !mda_expression::truth(&eval(&cond, &Value::Object(ctx.clone()), reg)?) {
            continue;
        }
        let Some(field) = r.action_field.as_deref() else {
            continue;
        };
        let value = match &r.action_value {
            Some(v) => eval(&Expr::from_json(v)?, &Value::Object(ctx.clone()), reg)?,
            None => Value::Null,
        };
        ctx.insert(field.to_string(), value);
        fired += 1;
        if fired > MAX_RULE_FIRINGS {
            return Err(Error::Invalid(format!(
                "rule execution exceeded max firings ({MAX_RULE_FIRINGS}); likely a rule cycle"
            )));
        }
    }
    Ok(())
}

/// Compute calculated fields: any field whose `config.formula` is an expression.
pub fn compute_calculated(
    def: &EntityDefinition,
    ctx: &mut Map<String, Value>,
    reg: &Registry,
) -> Result<()> {
    for f in &def.fields {
        if let Some(formula) = f.config.get("formula") {
            if formula.is_null() {
                continue;
            }
            let expr = Expr::from_json(formula)?;
            let v = eval(&expr, &Value::Object(ctx.clone()), reg)?;
            ctx.insert(f.name.clone(), v);
        }
    }
    Ok(())
}
