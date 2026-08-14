//! `mda-integration` — the hub-model integration layer (PLAN §5.22 / Phase 9).
//!
//! Integration is a *capability* of the application platform: the platform syncs
//! and orchestrates with external systems **because it is a participant with its
//! own canonical model and business logic** — not a wire-level relay. Becoming a
//! general-purpose stateless broker / iPaaS is an explicit non-goal (§1); that is
//! a different product. Everything here is generic data-integration mechanics and
//! introduces no vendor or business noun (principle 8).
//!
//! - **Hub, not broker (§5.22.1):** every flow materializes data into the
//!   platform's canonical `biz.*` entities — there is no stateless A→B
//!   pass-through — so the hub applies AuthZ, audit, rules, and transformation
//!   between systems.
//! - **Connector boundary (§5.6/§5.22.6):** core ships the universal HTTP
//!   transport + a pluggable Auth boundary (secret-resolved via the
//!   `SecretStore`). Niche formats / vendor protocols are extension connectors.
//! - **Correlation & idempotency (§5.22.3):** the `int_external_id` registry
//!   drives upsert-by-external-key, idempotent re-delivery, and cross-path
//!   dedup — the single most important reliability primitive for multi-system
//!   sync.
//! - **Conflict policy (§5.22.4):** a declared per-flow policy reconciles
//!   cross-system updates (distinct from internal OCC, §5.9). v1 implements
//!   `last_write_wins`; `manual` quarantines a conflict for a human.
//! - **Transform steps (§5.22.2):** each step runs a bounded expression-engine
//!   transform (§5.2) — value translation (`int_value_map`), conditional,
//!   enrichment — inheriting the DSL's safety (no second scripting surface).

use std::collections::HashMap;

use async_trait::async_trait;
use mda_core::{Error, Result, SecretStore};
use mda_data::RecordScope;
use mda_expression::{eval, Expr, Registry};
use mda_meta::EntityDefinition;
use serde_json::{Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

/// A typed integration adapter. Core ships the HTTP transport; extension
/// transports (DB/file/MQ/GraphQL/SOAP, EDI/IDoc/AS2) are add-ons. Auth is
/// resolved server-side from the [`SecretStore`] (§5.20) — values never leave the
/// connector run.
#[async_trait]
pub trait Connector: Send + Sync {
    fn transport(&self) -> &str;
    /// Pull external records (inbound fetch / scheduled).
    async fn fetch(
        &self,
        path: &str,
        secrets: &dyn SecretStore,
        tenant: Uuid,
    ) -> Result<Vec<Value>>;
    /// Push records to the external system (outbound).
    async fn push(
        &self,
        path: &str,
        body: &Value,
        secrets: &dyn SecretStore,
        tenant: Uuid,
    ) -> Result<()>;
}

/// The universal HTTP transport. `auth` is a JSON object:
/// `{ "kind": "none" | "basic" | "bearer" | "header", "secret_ref": "<name>",
///    "header_name": "..." }`. The secret value is resolved at run time.
pub struct HttpConnector {
    pub base_url: String,
    pub auth: Value,
    client: reqwest::Client,
}

impl HttpConnector {
    pub fn new(base_url: String, auth: Value) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            auth,
            client: reqwest::Client::new(),
        }
    }

    /// Resolve the auth header(s) server-side from the secret store.
    fn auth_headers(
        &self,
        secrets: &dyn SecretStore,
        _tenant: Uuid,
    ) -> Result<Vec<(String, String)>> {
        let kind = self
            .auth
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("none");
        match kind {
            "none" | "" => Ok(vec![]),
            "bearer" | "header" => {
                let ref_name = self
                    .auth
                    .get("secret_ref")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| Error::Invalid("auth bearer/header needs secret_ref".into()))?;
                let val = secrets
                    .resolve(ref_name)?
                    .ok_or_else(|| Error::NotFound(format!("secret {ref_name}")))?;
                let header_name = self
                    .auth
                    .get("header_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Authorization");
                let hv = if kind == "bearer" {
                    format!(
                        "Bearer {}",
                        String::from_utf8(val).map_err(Error::internal)?
                    )
                } else {
                    String::from_utf8(val).map_err(Error::internal)?
                };
                Ok(vec![(header_name.to_string(), hv)])
            }
            "basic" => {
                // secret_ref holds "user:pass" (dev); production resolves per-cred.
                let ref_name = self
                    .auth
                    .get("secret_ref")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| Error::Invalid("auth basic needs secret_ref".into()))?;
                let val = secrets
                    .resolve(ref_name)?
                    .ok_or_else(|| Error::NotFound(format!("secret {ref_name}")))?;
                let cred = String::from_utf8(val).map_err(Error::internal)?;
                let encoded = base64_std(&cred);
                Ok(vec![(
                    "Authorization".to_string(),
                    format!("Basic {encoded}"),
                )])
            }
            other => Err(Error::Invalid(format!("unknown auth kind {other}"))),
        }
    }
}

fn base64_std(s: &str) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    STANDARD.encode(s.as_bytes())
}

#[async_trait]
impl Connector for HttpConnector {
    fn transport(&self) -> &str {
        "http"
    }

    async fn fetch(
        &self,
        path: &str,
        secrets: &dyn SecretStore,
        tenant: Uuid,
    ) -> Result<Vec<Value>> {
        let url = format!("{}{path}", self.base_url);
        let mut req = self.client.get(&url);
        for (k, v) in self.auth_headers(secrets, tenant)? {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(Error::internal)?;
        if !resp.status().is_success() {
            return Err(Error::internal(anyhow::anyhow!(
                "fetch {url} returned {}",
                resp.status()
            )));
        }
        let v: Value = resp.json().await.map_err(Error::internal)?;
        Ok(match v {
            Value::Array(a) => a,
            other => vec![other],
        })
    }

    async fn push(
        &self,
        path: &str,
        body: &Value,
        secrets: &dyn SecretStore,
        tenant: Uuid,
    ) -> Result<()> {
        let url = format!("{}{path}", self.base_url);
        let mut req = self.client.post(&url).json(body);
        for (k, v) in self.auth_headers(secrets, tenant)? {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(Error::internal)?;
        if !resp.status().is_success() {
            return Err(Error::internal(anyhow::anyhow!(
                "push {url} returned {}",
                resp.status()
            )));
        }
        Ok(())
    }
}

/// A flow definition loaded from `int.flow`.
#[derive(Debug, Clone)]
pub struct Flow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub direction: String,
    pub entity: String,
    pub connector_id: Option<Uuid>,
    pub webhook_id: Option<Uuid>,
    pub endpoint_path: Option<String>,
    pub mapping: Value,
    pub external_key_field: String,
    pub conflict_policy: String,
    pub system: Option<String>,
    /// Per-flow scoped principal (§5.22 follow-up): newly created records are
    /// owned by this user when set, instead of a blanket system superuser.
    pub running_user_id: Option<Uuid>,
    /// Flow-level config (currently `sor_fields` for the `field_level_sor`
    /// conflict policy: the canonical fields this external system owns).
    pub config: Value,
}

/// A transform step loaded from `int.flow_step` (ordered by seq).
#[derive(Debug, Clone)]
pub struct FlowStep {
    pub seq: i32,
    pub kind: String,
    pub config: Value,
}

/// Load an active inbound flow triggered by a webhook (§5.21 receiver enqueues
/// `integration.inbound` with the webhook_id).
pub async fn flow_for_webhook(
    pool: &PgPool,
    tenant: Uuid,
    webhook_id: Uuid,
) -> Result<Option<Flow>> {
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    #[allow(clippy::type_complexity)]
    type FlowRow = (
        Uuid,
        String,
        String,
        String,
        Option<Uuid>,
        Option<Uuid>,
        Option<String>,
        Value,
        String,
        String,
        Option<String>,
        Option<Uuid>,
        Value,
    );
    let row: Option<FlowRow> = sqlx::query_as(
        "SELECT id, name, direction, entity, connector_id, webhook_id, endpoint_path,
                mapping, external_key_field, conflict_policy, system, running_user_id, config
           FROM int.flow
          WHERE tenant_id = $1 AND webhook_id = $2 AND active = TRUE AND direction = 'inbound'",
    )
    .bind(tenant)
    .bind(webhook_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(row.map(|r| Flow {
        id: r.0,
        tenant_id: tenant,
        name: r.1,
        direction: r.2,
        entity: r.3,
        connector_id: r.4,
        webhook_id: Some(webhook_id),
        endpoint_path: r.6,
        mapping: r.7,
        external_key_field: r.8,
        conflict_policy: r.9,
        system: r.10,
        running_user_id: r.11,
        config: r.12,
    }))
}

/// Load a flow by id (any direction).
pub async fn flow_by_id(pool: &PgPool, tenant: Uuid, id: Uuid) -> Result<Flow> {
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    #[allow(clippy::type_complexity)]
    type Row = (
        Uuid,
        String,
        String,
        String,
        Option<Uuid>,
        Option<Uuid>,
        Option<String>,
        Value,
        String,
        String,
        Option<String>,
        Option<Uuid>,
        Value,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT id, name, direction, entity, connector_id, webhook_id, endpoint_path,
                mapping, external_key_field, conflict_policy, system, running_user_id, config
           FROM int.flow WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    let r = row.ok_or_else(|| Error::NotFound(format!("flow {id}")))?;
    Ok(Flow {
        id: r.0,
        tenant_id: tenant,
        name: r.1,
        direction: r.2,
        entity: r.3,
        connector_id: r.4,
        webhook_id: r.5,
        endpoint_path: r.6,
        mapping: r.7,
        external_key_field: r.8,
        conflict_policy: r.9,
        system: r.10,
        running_user_id: r.11,
        config: r.12,
    })
}

/// Load a connector (base_url + auth) for outbound / scheduled fetch.
pub async fn connector_for(pool: &PgPool, tenant: Uuid, id: Uuid) -> Result<(String, Value)> {
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    let row: Option<(String, Value)> =
        sqlx::query_as("SELECT base_url, auth FROM int.connector WHERE tenant_id = $1 AND id = $2")
            .bind(tenant)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    row.ok_or_else(|| Error::NotFound(format!("connector {id}")))
}

/// Load a flow's ordered transform steps.
pub async fn flow_steps(pool: &PgPool, flow_id: Uuid) -> Result<Vec<FlowStep>> {
    let rows: Vec<(i32, String, Value)> = sqlx::query_as(
        "SELECT seq, kind, config FROM int.flow_step WHERE flow_id = $1 ORDER BY seq",
    )
    .bind(flow_id)
    .fetch_all(pool)
    .await
    .map_err(Error::internal)?;
    Ok(rows
        .into_iter()
        .map(|(seq, kind, config)| FlowStep { seq, kind, config })
        .collect())
}

/// Resolve a dotted path in a JSON value (missing → Null).
fn resolve_path(ctx: &Value, path: &str) -> Value {
    let mut cur = ctx;
    for seg in path.split('.') {
        cur = cur.get(seg.trim()).unwrap_or(&Value::Null);
    }
    cur.clone()
}

/// Apply a flow's mapping (external payload → biz record fields).
fn apply_mapping(mapping: &Value, external: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(obj) = mapping.as_object() {
        for (biz_field, ext_path) in obj {
            if let Some(path) = ext_path.as_str() {
                out.insert(biz_field.clone(), resolve_path(external, path));
            }
        }
    }
    out
}

/// Apply transform steps to the mapped record (in place). Returns `false` if a
/// `filter` step rejected the record (the flow should skip materializing it).
fn apply_steps(
    pool_steps: &[FlowStep],
    rec: &mut Map<String, Value>,
    value_maps: &HashMap<String, Value>,
    reg: &Registry,
) -> Result<bool> {
    let ctx = Value::Object(rec.clone());
    for s in pool_steps {
        match s.kind.as_str() {
            "transform" => {
                if let Some(fields) = s.config.get("fields").and_then(|v| v.as_object()) {
                    for (field, expr_v) in fields {
                        let expr = Expr::from_json(expr_v)?;
                        let val = eval(&expr, &ctx, reg)?;
                        rec.insert(field.clone(), val);
                    }
                }
            }
            "value_map" => {
                let field = s.config.get("field").and_then(|v| v.as_str());
                let map_name = s.config.get("map").and_then(|v| v.as_str());
                if let (Some(f), Some(name)) = (field, map_name) {
                    if let Some(entries) = value_maps.get(name).and_then(|v| v.as_object()) {
                        if let Some(cur) = rec.get(f) {
                            let key = match cur {
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            if let Some(mapped) = entries.get(&key) {
                                rec.insert(f.to_string(), mapped.clone());
                            }
                        }
                    }
                }
            }
            "filter" => {
                if let Some(cond) = s.config.get("condition") {
                    let expr = Expr::from_json(cond)?;
                    if !mda_expression::truth(&eval(&expr, &ctx, reg)?) {
                        return Ok(false);
                    }
                }
            }
            other => {
                tracing::warn!(kind = other, "unknown flow step kind; skipping");
            }
        }
    }
    Ok(true)
}

/// Load the value maps referenced by a flow's steps (RLS-gated → tenant GUC).
async fn load_value_maps(
    pool: &PgPool,
    tenant: Uuid,
    steps: &[FlowStep],
) -> Result<HashMap<String, Value>> {
    let names: Vec<String> = steps
        .iter()
        .filter_map(|s| {
            if s.kind == "value_map" {
                s.config
                    .get("map")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            } else {
                None
            }
        })
        .collect();
    if names.is_empty() {
        return Ok(HashMap::new());
    }
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT name, entries FROM int.value_map WHERE tenant_id = $1 AND name = ANY($2)",
    )
    .bind(tenant)
    .bind(&names)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(rows.into_iter().collect())
}

/// Look up an existing record by external key (the correlation registry).
async fn lookup_external(
    pool: &PgPool,
    tenant: Uuid,
    entity: &str,
    system: &str,
    external_key: &str,
) -> Result<Option<Uuid>> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT record_id FROM int_external_id
          WHERE tenant_id = $1 AND entity = $2 AND system = $3 AND external_key = $4",
    )
    .bind(tenant)
    .bind(entity)
    .bind(system)
    .bind(external_key)
    .fetch_optional(pool)
    .await
    .map_err(Error::internal)?;
    Ok(row.map(|(r,)| r))
}

/// Record (or refresh) the correlation link. Idempotent on
/// (tenant, entity, system, external_key).
async fn upsert_external(
    pool: &PgPool,
    tenant: Uuid,
    entity: &str,
    record_id: Uuid,
    system: &str,
    external_key: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO int_external_id (tenant_id, entity, record_id, system, external_key)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (tenant_id, entity, system, external_key)
         DO UPDATE SET record_id = EXCLUDED.record_id, updated_at = now()",
    )
    .bind(tenant)
    .bind(entity)
    .bind(record_id)
    .bind(system)
    .bind(external_key)
    .execute(pool)
    .await
    .map_err(Error::internal)?;
    Ok(())
}

/// The fields this flow's external system is the authoritative source for (the
/// `field_level_sor` conflict policy config, §5.22.4). Empty → no SOR declared.
fn sor_fields_of(flow: &Flow) -> Vec<String> {
    flow.config
        .get("sor_fields")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Expand an inbound payload using any `debatch` step: one payload carrying an
/// array fans out into one payload per element (the parent's non-array context
/// is overlaid so a batch header propagates to each child). No debatch step →
/// the single payload is returned unchanged.
fn expand_debatch(external: &Value, steps: &[FlowStep]) -> Vec<Value> {
    let Some(step) = steps.iter().find(|s| s.kind == "debatch") else {
        return vec![external.clone()];
    };
    let Some(field) = step.config.get("field").and_then(|v| v.as_str()) else {
        return vec![external.clone()];
    };
    let Some(items) = external.get(field).and_then(|v| v.as_array()) else {
        return vec![external.clone()];
    };
    // parent context minus the debatched array field.
    let parent = {
        let mut p = external.clone();
        if let Some(obj) = p.as_object_mut() {
            obj.remove(field);
        }
        p
    };
    items
        .iter()
        .map(|item| {
            if let (Some(_), Some(mi)) = (parent.as_object(), item.as_object()) {
                let mut merged = parent.clone();
                let m = merged.as_object_mut().unwrap();
                for (k, v) in mi {
                    m.insert(k.clone(), v.clone());
                }
                merged
            } else {
                item.clone()
            }
        })
        .collect()
}

/// Process one external payload → one canonical record (map → transform steps →
/// upsert by external key). Pre-loads nothing: callers pass the already-loaded
/// steps + value maps so a batched/debatched run loads them once. Returns
/// `Ok(None)` when a `filter` step rejected the record.
#[allow(clippy::too_many_arguments)]
async fn process_one(
    pool: &PgPool,
    def: &EntityDefinition,
    flow: &Flow,
    external: &Value,
    owner: Uuid,
    steps: &[FlowStep],
    value_maps: &HashMap<String, Value>,
    reg: &Registry,
) -> Result<Option<Uuid>> {
    let tenant = flow.tenant_id;
    let system = flow.system.clone().unwrap_or_else(|| flow.name.clone());

    let external_key = external
        .get(&flow.external_key_field)
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .ok_or_else(|| {
            Error::Invalid(format!(
                "inbound payload missing external key field '{}'",
                flow.external_key_field
            ))
        })?;

    let mut rec = apply_mapping(&flow.mapping, external);
    if !apply_steps(steps, &mut rec, value_maps, reg)? {
        // a filter step rejected this record — skip (no materialization).
        record_run(pool, tenant, flow, "ok", 0, Some(&external_key), None).await?;
        return Ok(None);
    }

    let existing = lookup_external(pool, tenant, &flow.entity, &system, &external_key).await?;
    let sor_fields = sor_fields_of(flow);
    let record_id = match existing {
        Some(id) => {
            apply_update(
                pool,
                tenant,
                def,
                id,
                &rec,
                &flow.conflict_policy,
                &sor_fields,
            )
            .await?;
            id
        }
        None => {
            let created = mda_data::create(pool, tenant, def, rec, owner).await?;
            let id = created
                .get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok())
                .ok_or_else(|| Error::internal(anyhow::anyhow!("created record missing id")))?;
            id
        }
    };
    upsert_external(
        pool,
        tenant,
        &flow.entity,
        record_id,
        &system,
        &external_key,
    )
    .await?;
    record_run(pool, tenant, flow, "ok", 1, Some(&external_key), None).await?;
    Ok(Some(record_id))
}

/// Run an inbound flow: map the external payload → biz record, apply transforms,
/// and upsert by external key (idempotent). Returns the materialized record id.
/// `system_user` is the fallback owner; a per-flow `running_user_id` (if set)
/// takes precedence so the hub writes under a scoped principal.
pub async fn run_inbound(
    pool: &PgPool,
    def: &EntityDefinition,
    flow: &Flow,
    external: &Value,
    system_user: Uuid,
) -> Result<Uuid> {
    let owner = flow.running_user_id.unwrap_or(system_user);
    let steps = flow_steps(pool, flow.id).await?;
    let value_maps = load_value_maps(pool, flow.tenant_id, &steps).await?;
    let reg = Registry::new();
    process_one(pool, def, flow, external, owner, &steps, &value_maps, &reg)
        .await?
        .ok_or_else(|| Error::Invalid("record filtered by flow step".into()))
}

/// Run an inbound flow over a payload that may carry a batch (a `debatch` step
/// fans one array into many records). Returns the materialized record ids
/// (filtered records are simply omitted).
pub async fn run_inbound_batch(
    pool: &PgPool,
    def: &EntityDefinition,
    flow: &Flow,
    external: &Value,
    system_user: Uuid,
) -> Result<Vec<Uuid>> {
    let owner = flow.running_user_id.unwrap_or(system_user);
    let steps = flow_steps(pool, flow.id).await?;
    let value_maps = load_value_maps(pool, flow.tenant_id, &steps).await?;
    let reg = Registry::new();
    let mut ids = Vec::new();
    for payload in expand_debatch(external, &steps) {
        if let Some(id) =
            process_one(pool, def, flow, &payload, owner, &steps, &value_maps, &reg).await?
        {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Scheduled pull: fetch external records from the flow's connector and
/// materialize each through the inbound pipeline (the `integration` schedule
/// kind, §5.22). A single bad record is logged + skipped — it must not abort the
/// whole pull (at-least-once with a per-record failure surface).
pub async fn fetch_and_run_inbound(
    pool: &PgPool,
    secrets: &dyn SecretStore,
    def: &EntityDefinition,
    flow: &Flow,
    system_user: Uuid,
) -> Result<Vec<Uuid>> {
    let connector_id = flow
        .connector_id
        .ok_or_else(|| Error::Invalid("inbound pull flow has no connector".into()))?;
    let (base_url, auth) = connector_for(pool, flow.tenant_id, connector_id).await?;
    let connector = HttpConnector::new(base_url, auth);
    let path = flow.endpoint_path.as_deref().unwrap_or("/");
    let fetched = connector.fetch(path, secrets, flow.tenant_id).await?;

    let owner = flow.running_user_id.unwrap_or(system_user);
    let steps = flow_steps(pool, flow.id).await?;
    let value_maps = load_value_maps(pool, flow.tenant_id, &steps).await?;
    let reg = Registry::new();
    let mut ids = Vec::new();
    for external in fetched {
        for payload in expand_debatch(&external, &steps) {
            match process_one(pool, def, flow, &payload, owner, &steps, &value_maps, &reg).await {
                Ok(Some(id)) => ids.push(id),
                Ok(None) => {}
                Err(e) => tracing::warn!(%flow.id, err = %e, "inbound pull: record skipped"),
            }
        }
    }
    Ok(ids)
}

/// Apply an inbound update honoring the conflict policy:
/// - `last_write_wins`: apply the full record, retry once on OCC conflict;
/// - `manual`: a cross-system OCC conflict is quarantined for a human;
/// - `field_level_sor`: apply only the fields this system owns (`sor_fields`),
///   preserving fields owned by other systems, retry once on OCC conflict.
async fn apply_update(
    pool: &PgPool,
    tenant: Uuid,
    def: &EntityDefinition,
    id: Uuid,
    rec: &Map<String, Value>,
    policy: &str,
    sor_fields: &[String],
) -> Result<()> {
    let scope = RecordScope::superuser(Uuid::nil());
    for _attempt in 0..2 {
        let current = mda_data::read(pool, tenant, def, id, &scope).await?;
        let version = current
            .get("version")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| Error::internal(anyhow::anyhow!("record missing version")))?;
        // field_level_sor narrows the write to the fields this system owns.
        let to_write: Map<String, Value> = if policy == "field_level_sor" && !sor_fields.is_empty()
        {
            rec.iter()
                .filter(|(k, _)| sor_fields.iter().any(|f| f == k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        } else {
            rec.clone()
        };
        match mda_data::update(pool, tenant, def, id, version, to_write, &scope, None).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                let is_conflict = matches!(e, mda_core::Error::Conflict(_));
                if is_conflict && (policy == "last_write_wins" || policy == "field_level_sor") {
                    continue; // retry with the fresh version
                }
                if is_conflict && policy == "manual" {
                    return Err(Error::Conflict(
                        "cross-system conflict; quarantined for manual resolution (policy=manual)"
                            .into(),
                    ));
                }
                return Err(e);
            }
        }
    }
    Err(Error::Conflict(
        "inbound update lost-update race after retries".into(),
    ))
}

/// Run an outbound flow: read nothing — push the supplied biz record through the
/// mapping + connector to the external system.
pub async fn run_outbound(
    pool: &PgPool,
    secrets: &dyn SecretStore,
    flow: &Flow,
    record: &Value,
) -> Result<()> {
    let connector_id = flow
        .connector_id
        .ok_or_else(|| Error::Invalid("outbound flow has no connector".into()))?;
    let (base_url, auth) = connector_for(pool, flow.tenant_id, connector_id).await?;
    let connector = HttpConnector::new(base_url, auth);
    let path = flow.endpoint_path.as_deref().unwrap_or("/");
    // map the biz record → external payload (inverse mapping: biz_field → external).
    let payload = map_outbound(&flow.mapping, record);
    let _steps = flow_steps(pool, flow.id).await?;
    connector
        .push(path, &payload, secrets, flow.tenant_id)
        .await?;
    record_run(pool, flow.tenant_id, flow, "ok", 1, None, None).await?;
    Ok(())
}

fn map_outbound(mapping: &Value, record: &Value) -> Value {
    // mapping is { biz_field: external_path }; invert so the external payload is
    // built from the record. For nested external paths (a.b) we build nested objs.
    let mut out = Map::new();
    if let Some(obj) = mapping.as_object() {
        for (biz_field, ext_path) in obj {
            if let Some(path) = ext_path.as_str() {
                let val = record.get(biz_field).cloned().unwrap_or(Value::Null);
                set_nested(&mut out, path, val);
            }
        }
    }
    Value::Object(out)
}

fn set_nested(out: &mut Map<String, Value>, path: &str, val: Value) {
    let segs: Vec<&str> = path
        .split('.')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if segs.is_empty() {
        return;
    }
    let mut cur = out;
    for (i, seg) in segs.iter().enumerate() {
        if i + 1 == segs.len() {
            cur.insert(seg.to_string(), val.clone());
        } else {
            let entry = cur
                .entry(seg.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            cur = entry.as_object_mut().unwrap();
        }
    }
}

async fn record_run(
    pool: &PgPool,
    tenant: Uuid,
    flow: &Flow,
    status: &str,
    records: i32,
    external_key: Option<&str>,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO sys_integration_run (tenant_id, flow_id, direction, status, records, external_key, error, finished_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, now())",
    )
    .bind(tenant)
    .bind(flow.id)
    .bind(&flow.direction)
    .bind(status)
    .bind(records)
    .bind(external_key)
    .bind(error)
    .execute(pool)
    .await
    .map_err(Error::internal)?;
    Ok(())
}

/// Record a failed run (used by the drain on error).
pub async fn record_failure(pool: &PgPool, tenant: Uuid, flow: &Flow, error: &str) -> Result<()> {
    record_run(pool, tenant, flow, "failed", 0, None, Some(error)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mapping_resolves_paths() {
        let ext = json!({"external_id":"A1","customer":{"name":"Acme"},"raw_status":"ACTIVE"});
        let mapping = json!({"name":"customer.name","source_status":"raw_status"});
        let rec = apply_mapping(&mapping, &ext);
        assert_eq!(rec["name"], "Acme");
        assert_eq!(rec["source_status"], "ACTIVE");
    }

    #[test]
    fn outbound_mapping_inverts_and_nests() {
        let mapping = json!({"name":"customer.name","tier":"customer.tier"});
        let record = json!({"name":"Acme","tier":"Gold"});
        let out = map_outbound(&mapping, &record);
        assert_eq!(out["customer"]["name"], "Acme");
        assert_eq!(out["customer"]["tier"], "Gold");
    }

    #[test]
    fn value_map_step_translates() {
        let mut rec = Map::new();
        rec.insert("status".into(), json!("ACTIVE"));
        let steps = vec![FlowStep {
            seq: 1,
            kind: "value_map".into(),
            config: json!({"field":"status","map":"status_map"}),
        }];
        let mut maps = HashMap::new();
        maps.insert("status_map".into(), json!({"ACTIVE":"open"}));
        let reg = Registry::new();
        assert!(apply_steps(&steps, &mut rec, &maps, &reg).unwrap());
        assert_eq!(rec["status"], "open");
    }

    #[test]
    fn filter_step_rejects() {
        let mut rec = Map::new();
        rec.insert("amount".into(), json!(5));
        let steps = vec![FlowStep {
            seq: 1,
            kind: "filter".into(),
            // amount > 10
            config: json!({"condition":{"op":"Cmp","kind":"gt","lhs":{"op":"Field","name":"amount"},"rhs":{"op":"Lit","value":10}}}),
        }];
        let reg = Registry::new();
        assert!(!apply_steps(&steps, &mut rec, &HashMap::new(), &reg).unwrap());
    }

    #[test]
    fn transform_step_evaluates() {
        let mut rec = Map::new();
        rec.insert("qty".into(), json!(3));
        rec.insert("price".into(), json!(10));
        let steps = vec![FlowStep {
            seq: 1,
            kind: "transform".into(),
            config: json!({"fields":{"total":{"op":"Arith","kind":"mul","lhs":{"op":"Field","name":"qty"},"rhs":{"op":"Field","name":"price"}}}}),
        }];
        let reg = Registry::new();
        assert!(apply_steps(&steps, &mut rec, &HashMap::new(), &reg).unwrap());
        assert_eq!(rec["total"].as_f64().unwrap(), 30.0);
    }

    #[test]
    fn debatch_step_fans_array_into_payloads() {
        // one payload carrying an array of items + a shared batch field.
        let external = json!({
            "source": "ERP",
            "items": [
                {"external_id": "A1", "name": "Acme"},
                {"external_id": "B2", "name": "Globex"}
            ]
        });
        let steps = vec![FlowStep {
            seq: 1,
            kind: "debatch".into(),
            config: json!({"field": "items"}),
        }];
        let payloads = expand_debatch(&external, &steps);
        assert_eq!(payloads.len(), 2, "fanned into two payloads");
        // each child carries its own keys + the shared parent context, but NOT
        // the debatched array field.
        assert_eq!(payloads[0]["external_id"], "A1");
        assert_eq!(payloads[0]["name"], "Acme");
        assert_eq!(payloads[0]["source"], "ERP", "parent context propagated");
        assert!(payloads[0].get("items").is_none(), "array field stripped");
        assert_eq!(payloads[1]["external_id"], "B2");

        // no debatch step → single payload unchanged.
        let payloads = expand_debatch(&external, &[]);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0], external);
    }

    #[test]
    fn sor_fields_filter_to_owned_fields() {
        // a flow whose external system owns only `name` (not `tier`).
        let flow = Flow {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            name: "sor".into(),
            direction: "inbound".into(),
            entity: "Customer".into(),
            connector_id: None,
            webhook_id: None,
            endpoint_path: None,
            mapping: json!({}),
            external_key_field: "external_id".into(),
            conflict_policy: "field_level_sor".into(),
            system: None,
            running_user_id: None,
            config: json!({"sor_fields": ["name"]}),
        };
        assert_eq!(sor_fields_of(&flow), vec!["name".to_string()]);

        // empty config → no SOR declared.
        let mut flow2 = flow.clone();
        flow2.config = json!({});
        assert!(sor_fields_of(&flow2).is_empty());
    }
}
