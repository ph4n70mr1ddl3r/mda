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
    Field, FieldFuture, FieldValue, Object, Schema, SchemaBuilder, TypeRef,
};
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
    let schema = build_schema(&st.pool, tenant).await?;
    st.gql
        .write()
        .await
        .insert((tenant, version), schema.clone());
    Ok(schema)
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
    async_graphql::Error::new(e.to_string())
}

/// Read a field off a GraphQL object value (None if not an object / missing).
fn field_of<'a>(v: &'a GqlValue, name: &str) -> Option<&'a GqlValue> {
    match v {
        GqlValue::Object(map) => map.get(name),
        _ => None,
    }
}

/// Build the dynamic schema for a tenant from its active model.
async fn build_schema(pool: &sqlx::PgPool, tenant: Uuid) -> Result<Schema, Error> {
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
    let mut builder: SchemaBuilder = Schema::build("Query", None, None);

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
            let field_name = rel.source_field_name.clone();
            let pool = pool.clone();
            let defs_map = defs_map.clone();
            let target = target.clone();
            obj = obj.field(Field::new(
                field_name.clone(),
                TypeRef::named(target.clone()),
                move |ctx| {
                    let pool = pool.clone();
                    let defs_map = defs_map.clone();
                    let field_name = field_name.clone();
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
                            .and_then(|v| field_of(v, &field_name))
                            .and_then(|v| match v {
                                GqlValue::String(s) => Some(s.clone()),
                                _ => None,
                            });
                        let id = fk.and_then(|s| Uuid::parse_str(&s).ok());
                        let Some(id) = id else {
                            return Ok(None);
                        };
                        let def = defs_map.get(&target).unwrap();
                        Ok(load_one(&pool, &user, def, id)
                            .await
                            .map_err(gql_err)?
                            .map(|r| FieldValue::value(record_to_gql(&r))))
                    })
                },
            ));
        }
        builder = builder.register(obj);

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

    builder = builder.register(query);
    builder
        .limit_depth(MAX_DEPTH)
        .limit_complexity(MAX_COMPLEXITY)
        .finish()
        .map_err(|e| Error::internal(anyhow::anyhow!("graphql schema build failed: {e}")))
}

/// `PascalCase` → `camelCase` for query field names.
fn entity_to_camel(name: &str) -> String {
    let mut c = name.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_ascii_lowercase().to_string() + c.as_str(),
    }
}
