//! GraphQL — a first-class runtime data API (ADR-0010).
//!
//! The schema is generated from the active model and re-built when the active
//! version advances (publish). It runs alongside REST (which stays for Studio,
//! auth, and SSE). MVP scope is **query/traversal-first** (reads + nested fetches
//! over relationships); mutations reach REST parity progressively.
//!
//! Security is enforced **by construction**, sharing REST's service layer:
//! - object: needs `read` on the entity (else nothing is returned);
//! - field: FLS read projection (unreadable fields are dropped per the caller);
//! - record: the caller's ownership/OWD predicate is injected into every read;
//! - traversal: a nested reference is loaded only if the caller can `read` the
//!   target entity, and the target is FLS-projected under the caller.
//!
//! Expensive nested queries are denied via depth + complexity limits (§5.17).

use std::collections::HashMap;
use std::sync::Arc;

use async_graphql::dynamic::{
    Field, FieldFuture, FieldValue, InputObject, InputValue, Object, Schema, SchemaBuilder, TypeRef,
};
use async_graphql::ErrorExtensions;
use async_graphql::Value as GqlValue;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use mda_core::Error;
use mda_data::RecordScope;
use mda_meta::{loader, EntityDefinition};
use mda_security::{Access, Identity, Owd};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

/// Max selection depth and total complexity for one GraphQL operation. Guards
/// against expensive nested queries (a metadata-driven API otherwise invites
/// arbitrarily deep traversal).
const MAX_DEPTH: usize = 8;
const MAX_COMPLEXITY: usize = 1000;

/// The shared, per-`(tenant, active_version)` schema cache. Stored in
/// [`AppState::gql`] and cleared on `meta_changed` (see [`spawn_invalidator`]).
pub type SchemaCache = HashMap<(uuid::Uuid, i64), async_graphql::dynamic::Schema>;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/graphql", post(execute))
}

/// `POST /api/graphql` — execute a GraphQL operation under the caller's identity.
#[derive(serde::Deserialize)]
struct GqlRequest {
    query: String,
    #[serde(default)]
    variables: HashMap<String, Value>,
    #[serde(default)]
    operation_name: Option<String>,
}

async fn execute(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(req): Json<GqlRequest>,
) -> ApiResult<Json<Value>> {
    let schema = schema_for(&st, user.tenant_id).await?;
    let mut request =
        async_graphql::Request::new(req.query).variables(async_graphql::Variables::from_value(
            serde_json::from_value(Value::Object(req.variables.into_iter().collect()))
                .unwrap_or_default(),
        ));
    if let Some(op) = req.operation_name {
        request = request.operation_name(op);
    }
    let request = request.data(st.pool.clone()).data(user.clone());
    let resp = schema.execute(request).await;
    let out: Value = serde_json::from_str(&serde_json::to_string(&resp).unwrap_or_default())
        .unwrap_or_else(|_| json!({ "errors": [{ "message": "internal error" }] }));
    Ok(Json(out))
}

/// Build (or fetch from cache) the dynamic schema for a tenant's active model.
/// Cached per active version so a publish (version advance) triggers a rebuild.
async fn schema_for(st: &AppState, tenant: Uuid) -> Result<Schema, Error> {
    let version = loader::active_version(&st.pool, tenant).await?;
    if let Some(s) = st.gql.read().await.get(&(tenant, version)) {
        return Ok(s.clone());
    }
    let schema = build_schema(st, tenant).await?;
    st.gql
        .write()
        .await
        .insert((tenant, version), schema.clone());
    Ok(schema)
}

/// Drop every cached GraphQL schema (every tenant / every version). Called by
/// the `meta_changed` LISTEN worker + the version-stamp poll fallback, which
/// already invalidate the entity-definition cache (ADR-0020 follow-up: the
/// schema is keyed by `(tenant, version)` so a publish already rebuilds it —
/// this clears the *stale* version entries so they do not accumulate across
/// many publishes, and guarantees a prompt rebuild even if a version stamp is
/// ever reused).
///
/// Safe to call from any task; the guard is a `RwLock`. Exported so the server
/// wiring can hook the same invalidation that drives the metadata cache.
pub fn invalidate_all(state: &AppState) {
    let gql = state.gql.clone();
    tokio::spawn(async move {
        gql.write().await.clear();
        tracing::debug!("meta_changed → graphql schema cache cleared");
    });
}

/// Spawn the GraphQL schema invalidator: `LISTEN meta_changed` → clear the
/// schema cache. Shares the same Postgres notification channel as the
/// entity-definition cache invalidator (§5.3). Self-healing is provided by the
/// `(tenant, version)` cache key: a publish advances the version, so a stale
/// entry is simply never read again even if a NOTIFY is missed.
pub fn spawn_invalidator(pool: sqlx::PgPool, state: AppState) {
    use std::time::Duration;
    tokio::spawn(async move {
        let mut listener = loop {
            match sqlx::postgres::PgListener::connect_with(&pool).await {
                Ok(l) => break l,
                Err(e) => {
                    tracing::warn!(
                        ?e,
                        "gql invalidator: connect failed; key parity covers this"
                    );
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        };
        loop {
            if let Err(e) = listener.listen("meta_changed").await {
                tracing::warn!(?e, "gql invalidator: listen failed; retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            loop {
                match listener.recv().await {
                    Ok(_) => {
                        let gql = state.gql.clone();
                        gql.write().await.clear();
                    }
                    Err(e) => {
                        tracing::warn!(?e, "gql invalidator: recv error; reconnecting");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        break;
                    }
                }
            }
        }
    });
}

fn gql_type(field_type: &str) -> TypeRef {
    match field_type {
        "integer" | "auto_number" => TypeRef::named(TypeRef::INT),
        "decimal" | "money" => TypeRef::named(TypeRef::FLOAT),
        "bool" => TypeRef::named(TypeRef::BOOLEAN),
        _ => TypeRef::named(TypeRef::STRING),
    }
}

fn core_fields() -> Vec<(&'static str, TypeRef)> {
    vec![
        ("id", TypeRef::named(TypeRef::ID)),
        ("version", TypeRef::named(TypeRef::INT)),
        ("owner_id", TypeRef::named(TypeRef::STRING)),
        ("state", TypeRef::named(TypeRef::STRING)),
        ("created_at", TypeRef::named(TypeRef::STRING)),
        ("updated_at", TypeRef::named(TypeRef::STRING)),
    ]
}

/// A scalar field resolver: read the named key off the parent record object.
fn scalar_field(name: String) -> Field {
    Field::new(name.clone(), gql_type("string"), move |ctx| {
        let v = ctx
            .parent_value
            .as_value()
            .and_then(|val| field_of(val, &name))
            .cloned();
        FieldFuture::Value(v.map(FieldValue::value))
    })
}

/// Convert a serde record into a GraphQL value (nested objects preserved).
fn record_to_gql(rec: &Value) -> GqlValue {
    GqlValue::try_from(rec.clone()).unwrap_or(GqlValue::Null)
}

async fn scope_for(
    pool: &sqlx::PgPool,
    user: &Identity,
    entity: &str,
) -> Result<RecordScope, Error> {
    let owd: Owd = mda_security::resolve_owd(pool, user.tenant_id, entity).await?;
    Ok(RecordScope {
        user_id: user.user_id,
        public_read: owd.allows_read_for_all(),
        public_write: owd.allows_write_for_all(),
        bypass: user.is_superuser,
        team_owd: owd == Owd::Team,
        team_id: user.team_id,
    })
}

/// FLS read projection (same rule as the REST data API).
fn project_field_level(user: &Identity, def: &EntityDefinition, mut rec: Value) -> Value {
    if let Some(obj) = rec.as_object_mut() {
        for f in &def.fields {
            if user.field_access(&def.entity.name, &f.name) == Access::None {
                obj.remove(&f.name);
            }
        }
    }
    rec
}

/// Load one record by id, FLS-projected, under the caller (object+record AuthZ).
async fn load_one(
    pool: &sqlx::PgPool,
    user: &Identity,
    def: &EntityDefinition,
    id: Uuid,
) -> Result<Option<Value>, Error> {
    if !user.can(&def.entity.name, "read") {
        return Ok(None);
    }
    let scope = scope_for(pool, user, &def.entity.name).await?;
    match mda_data::read(pool, user.tenant_id, def, id, &scope).await {
        Ok(rec) => Ok(Some(project_field_level(user, def, rec))),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

fn gql_err<E: std::fmt::Display>(e: E) -> async_graphql::Error {
    // ADR-0018 parity: REST scrubs 5xx internals ("internal server error");
    // GraphQL must not become the side door. `Error::Internal`'s Display would
    // otherwise carry SQL/driver details into errors[].message.
    let msg = e.to_string();
    let scrubbed = if msg.starts_with("internal error") || msg.starts_with("config error") {
        "internal error".to_string()
    } else {
        msg
    };
    async_graphql::Error::new(scrubbed)
}

/// Read a field off a GraphQL object value (None if not an object / missing).
fn field_of<'a>(v: &'a GqlValue, name: &str) -> Option<&'a GqlValue> {
    match v {
        GqlValue::Object(map) => map.get(name),
        _ => None,
    }
}

/// Build the dynamic schema for a tenant from its active model.
async fn build_schema(st: &AppState, tenant: Uuid) -> Result<Schema, Error> {
    let pool = &st.pool;
    let entity_ids = loader::entity_ids_for_tenant(pool, tenant).await?;
    let mut defs: Vec<EntityDefinition> = Vec::new();
    for id in entity_ids {
        defs.push(loader::load_entity_definition(pool, tenant, id).await?);
    }

    let defs_map: Arc<HashMap<String, EntityDefinition>> = Arc::new(
        defs.iter()
            .map(|d| (d.entity.name.clone(), d.clone()))
            .collect(),
    );

    let mut query = Object::new("Query");
    let mut mutation = Object::new("Mutation");
    let mut builder: SchemaBuilder = Schema::build("Query", Some("Mutation"), None);

    for def in &defs {
        let entity_name = def.entity.name.clone();

        // ===== entity object type =====
        let mut obj = Object::new(entity_name.clone());
        for (fname, _fty) in core_fields() {
            obj = obj.field(scalar_field(fname.to_string()));
        }
        for f in &def.fields {
            let name = f.name.clone();
            let ty = gql_type(&f.field_type);
            obj = obj.field(Field::new(name.clone(), ty, move |ctx| {
                let n = name.clone();
                let v = ctx
                    .parent_value
                    .as_value()
                    .and_then(|val| field_of(val, &n))
                    .cloned();
                FieldFuture::Value(v.map(FieldValue::value))
            }));
        }
        // reference fields → nested traversal.
        for rel in &def.relationships {
            let Some(target) = defs
                .iter()
                .find(|d| d.entity.id == rel.target_entity_id)
                .map(|d| d.entity.name.clone())
            else {
                continue;
            };
            // The traversal is exposed under the (camel-cased) target entity name
            // (e.g. `customer`), while the FK value is read from the source column
            // (`customer_id`) on the parent record.
            let gql_name = entity_to_camel(&target);
            let fk_column = rel.source_field_name.clone();
            let pool = pool.clone();
            let defs_map = defs_map.clone();
            let target = target.clone();
            obj = obj.field(Field::new(
                gql_name.clone(),
                TypeRef::named(target.clone()),
                move |ctx| {
                    let pool = pool.clone();
                    let defs_map = defs_map.clone();
                    let fk_column = fk_column.clone();
                    let target = target.clone();
                    FieldFuture::new(async move {
                        let user = ctx.data::<Identity>()?.clone();
                        if !user.can(&target, "read") {
                            return Ok(None);
                        }
                        // read the FK off the parent record object
                        let fk = ctx
                            .parent_value
                            .as_value()
                            .and_then(|v| field_of(v, &fk_column))
                            .and_then(|v| match v {
                                GqlValue::String(s) => Some(s.clone()),
                                _ => None,
                            });
                        let id = fk.and_then(|s| Uuid::parse_str(&s).ok());
                        let Some(id) = id else {
                            return Ok(None);
                        };
                        // Defense in depth: the publish gate rejects
                        // relationships whose target is not in the draft model
                        // (draft.rs diff pass 3), so the target is always
                        // registered — but a resolver must never panic on
                        // model state; resolve null instead.
                        let Some(def) = defs_map.get(&target) else {
                            return Ok(None);
                        };
                        Ok(load_one(&pool, &user, def, id)
                            .await
                            .map_err(gql_err)?
                            .map(|r| FieldValue::value(record_to_gql(&r))))
                    })
                },
            ));
        }
        builder = builder.register(obj);

        // ===== input type for create/update mutations =====
        // One nullable scalar field per data field + relationship FK. Keeping
        // fields nullable lets a client set only the subset they care about on a
        // PATCH; required-ness is enforced by the shared write service.
        let input_name = format!("{entity_name}Input");
        let mut input_obj = InputObject::new(input_name.clone());
        for f in &def.fields {
            input_obj = input_obj.field(InputValue::new(f.name.clone(), gql_type(&f.field_type)));
        }
        for rel in &def.relationships {
            input_obj = input_obj.field(InputValue::new(
                rel.source_field_name.clone(),
                TypeRef::named(TypeRef::STRING),
            ));
        }
        builder = builder.register(input_obj);

        // ===== mutation: create<Entity>(input): <Entity> =====
        let st_c = st.clone();
        let en_c = entity_name.clone();
        mutation = mutation.field(
            Field::new(
                format!("create{entity_name}"),
                TypeRef::named(&entity_name),
                move |ctx| {
                    let st = st_c.clone();
                    let entity = en_c.clone();
                    FieldFuture::new(async move {
                        let user = ctx.data::<Identity>()?.clone();
                        let body = gql_input_to_json(ctx.args.get("input"))?;
                        let rec = crate::data::create_record_service(&st, &user, &entity, body)
                            .await
                            .map_err(api_err_to_gql)?;
                        Ok(Some(FieldValue::value(record_to_gql(&rec))))
                    })
                },
            )
            .argument(InputValue::new(
                "input",
                TypeRef::named_nn(input_name.clone()),
            )),
        );

        // ===== mutation: update<Entity>(id, version, input): <Entity> =====
        let st_u = st.clone();
        let en_u = entity_name.clone();
        let in_u = input_name.clone();
        mutation = mutation.field(
            Field::new(
                format!("update{entity_name}"),
                TypeRef::named(&entity_name),
                move |ctx| {
                    let st = st_u.clone();
                    let entity = en_u.clone();
                    FieldFuture::new(async move {
                        let user = ctx.data::<Identity>()?.clone();
                        let id = parse_id(ctx.args.get("id"))?;
                        let version = ctx.args.get("version").and_then(|a| a.i64().ok());
                        let Some(version) = version else {
                            return Err(async_graphql::Error::new("version is required"));
                        };
                        let body = gql_input_to_json(ctx.args.get("input"))?;
                        let rec = crate::data::update_record_service(
                            &st, &user, &entity, id, version, body,
                        )
                        .await
                        .map_err(api_err_to_gql)?;
                        Ok(Some(FieldValue::value(record_to_gql(&rec))))
                    })
                },
            )
            .argument(InputValue::new("id", TypeRef::named_nn(TypeRef::ID)))
            .argument(InputValue::new("version", TypeRef::named_nn(TypeRef::INT)))
            .argument(InputValue::new("input", TypeRef::named_nn(in_u))),
        );

        // ===== mutation: delete<Entity>(id): Boolean =====
        let st_d = st.clone();
        let en_d = entity_name.clone();
        mutation = mutation.field(
            Field::new(
                format!("delete{entity_name}"),
                TypeRef::named(TypeRef::BOOLEAN),
                move |ctx| {
                    let st = st_d.clone();
                    let entity = en_d.clone();
                    FieldFuture::new(async move {
                        let user = ctx.data::<Identity>()?.clone();
                        let id = parse_id(ctx.args.get("id"))?;
                        crate::data::delete_record_service(&st, &user, &entity, id)
                            .await
                            .map_err(api_err_to_gql)?;
                        Ok(Some(FieldValue::value(true)))
                    })
                },
            )
            .argument(InputValue::new("id", TypeRef::named_nn(TypeRef::ID))),
        );

        // ===== query: <entity>(id: ID!): <Entity> =====
        let pool1 = pool.clone();
        let dm1 = defs_map.clone();
        let en1 = entity_name.clone();
        query = query.field(
            Field::new(
                entity_to_camel(&entity_name),
                TypeRef::named(&entity_name),
                move |ctx| {
                    let pool = pool1.clone();
                    let defs_map = dm1.clone();
                    let entity = en1.clone();
                    FieldFuture::new(async move {
                        let user = ctx.data::<Identity>()?.clone();
                        let def = defs_map.get(&entity).unwrap().clone();
                        let id = ctx
                            .args
                            .get("id")
                            .and_then(|a| a.string().ok())
                            .and_then(|s| Uuid::parse_str(s).ok());
                        let Some(id) = id else {
                            return Ok(None);
                        };
                        Ok(load_one(&pool, &user, &def, id)
                            .await
                            .map_err(gql_err)?
                            .map(|r| FieldValue::value(record_to_gql(&r))))
                    })
                },
            )
            .argument(async_graphql::dynamic::InputValue::new(
                "id",
                TypeRef::named_nn(TypeRef::ID),
            )),
        );

        // ===== query: <entity>s(first: Int): [<Entity>!] =====
        let pool2 = pool.clone();
        let dm2 = defs_map.clone();
        let en2 = entity_name.clone();
        query = query.field(
            Field::new(
                format!("{}s", entity_to_camel(&entity_name)),
                TypeRef::List(Box::new(TypeRef::named_nn(&entity_name))),
                move |ctx| {
                    let pool = pool2.clone();
                    let defs_map = dm2.clone();
                    let entity = en2.clone();
                    FieldFuture::new(async move {
                        let user = ctx.data::<Identity>()?.clone();
                        if !user.can(&entity, "read") {
                            return Ok(Some(FieldValue::list(Vec::<FieldValue>::new())));
                        }
                        let def = defs_map.get(&entity).unwrap().clone();
                        let first = ctx
                            .args
                            .get("first")
                            .and_then(|a| a.u64().ok())
                            .map(|n| n.min(200))
                            .unwrap_or(50);
                        let scope = scope_for(&pool, &user, &entity).await.map_err(gql_err)?;
                        let params = mda_data::ListParams {
                            filters: Vec::new(),
                            sort: Vec::new(),
                            page: 1,
                            page_size: first,
                        };
                        let result = mda_data::list(&pool, user.tenant_id, &def, &params, &scope)
                            .await
                            .map_err(gql_err)?;
                        let out: Vec<FieldValue> = result
                            .items
                            .into_iter()
                            .map(|rec| {
                                FieldValue::value(record_to_gql(&project_field_level(
                                    &user, &def, rec,
                                )))
                            })
                            .collect();
                        Ok(Some(FieldValue::list(out)))
                    })
                },
            )
            .argument(async_graphql::dynamic::InputValue::new(
                "first",
                TypeRef::named(TypeRef::INT),
            )),
        );
    }

    builder = builder.register(query).register(mutation);
    builder
        .limit_depth(MAX_DEPTH)
        .limit_complexity(MAX_COMPLEXITY)
        .finish()
        .map_err(|e| Error::internal(anyhow::anyhow!("graphql schema build failed: {e}")))
}

/// Read a GraphQL input-object argument into a serde_json object for the write
/// service. `as_value()` is the underlying async_graphql::Value, which
/// serializes to its JSON form — so a typed input object round-trips into a
/// serde object the shared write service re-validates + coerces.
fn gql_input_to_json(
    arg: Option<async_graphql::dynamic::ValueAccessor<'_>>,
) -> async_graphql::Result<Value> {
    let Some(a) = arg else {
        return Ok(Value::Object(serde_json::Map::new()));
    };
    serde_json::to_value(a.as_value())
        .map_err(|e| async_graphql::Error::new(format!("could not read mutation input: {e}")))
}

/// Parse a GraphQL `id` argument (`ID` → uuid string) into a [`Uuid`].
fn parse_id(arg: Option<async_graphql::dynamic::ValueAccessor<'_>>) -> async_graphql::Result<Uuid> {
    let Some(a) = arg else {
        return Err(async_graphql::Error::new("id is required"));
    };
    a.string()
        .ok()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| async_graphql::Error::new("id must be a UUID"))
}

/// Map an API-layer error (wrapping the canonical `mda.<kind>` code) into a
/// GraphQL error. The `code` is preserved as a GraphQL extension so SDK/i18n
/// clients can branch on it just like the REST envelope.
fn api_err_to_gql(e: crate::error::ApiError) -> async_graphql::Error {
    let code = e.0.code();
    // internal/config details stay server-side (see gql_err); the stable
    // `code` extension still tells SDK clients what happened.
    let msg = match e.0 {
        mda_core::Error::Internal(_) | mda_core::Error::Config(_) => "internal error".to_string(),
        other => other.to_string(),
    };
    async_graphql::Error::new(msg).extend_with(move |_, v| v.set("code", code))
}

/// `PascalCase` → `camelCase` for query field names.
fn entity_to_camel(name: &str) -> String {
    let mut c = name.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_ascii_lowercase().to_string() + c.as_str(),
    }
}
