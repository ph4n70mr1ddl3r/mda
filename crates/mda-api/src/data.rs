//! Runtime data API (PLAN §7): generic, dynamic CRUD over `biz.<table>` for any
//! active entity. Addressed by entity name; reads the definition through the
//! metadata cache; enforces OCC on update via the `If-Match` version header.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use mda_core::{Error, Result};
use mda_data::{self, ListParams};
use mda_meta::loader;
use serde::Deserialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::error::ApiResult;
use crate::extract::TenantId;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/data/:entity", get(list_records).post(create_record))
        .route(
            "/api/data/:entity/:id",
            get(read_record).patch(update_record).delete(delete_record),
        )
}

#[derive(Deserialize, Default)]
struct ListQuery {
    #[serde(default, deserialize_with = "string_or_seq")]
    filter: Vec<String>,
    #[serde(default, deserialize_with = "string_or_seq")]
    sort: Vec<String>,
    #[serde(default)]
    page: Option<u64>,
    #[serde(default)]
    page_size: Option<u64>,
}

/// Accept a single value or a repeated sequence (serde_urlencoded returns a
/// string for one occurrence, a seq for several).
fn string_or_seq<'de, D>(de: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Visitor;
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or a sequence of strings")
        }
        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(vec![v.to_string()])
        }
        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut out = Vec::new();
            while let Some(s) = seq.next_element::<String>()? {
                out.push(s);
            }
            Ok(out)
        }
    }
    de.deserialize_any(V)
}

async fn list_records(
    State(st): State<AppState>,
    TenantId(tenant): TenantId,
    Path(entity): Path<String>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<mda_data::ListResult>> {
    let def = entity_def(&st, tenant, &entity).await?;
    let params = parse_list_params(q)?;
    let res = mda_data::list(&st.pool, tenant, &def, &params).await?;
    Ok(Json(res))
}

async fn create_record(
    State(st): State<AppState>,
    TenantId(tenant): TenantId,
    Path(entity): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let def = entity_def(&st, tenant, &entity).await?;
    let map = into_object(body)?;
    let rec = mda_data::create(&st.pool, tenant, &def, map).await?;
    Ok((StatusCode::CREATED, Json(rec)))
}

async fn read_record(
    State(st): State<AppState>,
    TenantId(tenant): TenantId,
    Path((entity, id)): Path<(String, Uuid)>,
) -> ApiResult<Json<Value>> {
    let def = entity_def(&st, tenant, &entity).await?;
    let rec = mda_data::read(&st.pool, tenant, &def, id).await?;
    Ok(Json(rec))
}

async fn update_record(
    State(st): State<AppState>,
    TenantId(tenant): TenantId,
    Path((entity, id)): Path<(String, Uuid)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let expected = version_from_headers(&headers)?;
    let def = entity_def(&st, tenant, &entity).await?;
    let map = into_object(body)?;
    let rec = mda_data::update(&st.pool, tenant, &def, id, expected, map).await?;
    Ok(Json(rec))
}

async fn delete_record(
    State(st): State<AppState>,
    TenantId(tenant): TenantId,
    Path((entity, id)): Path<(String, Uuid)>,
) -> ApiResult<Response> {
    let def = entity_def(&st, tenant, &entity).await?;
    mda_data::delete(&st.pool, tenant, &def, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ===== helpers =====

async fn entity_def(
    st: &AppState,
    tenant: Uuid,
    name: &str,
) -> Result<std::sync::Arc<mda_meta::EntityDefinition>> {
    let id = loader::entity_id_by_name(&st.pool, tenant, name).await?;
    st.cache.get_entity(&st.pool, tenant, id).await
}

fn into_object(v: Value) -> Result<Map<String, Value>> {
    match v {
        Value::Object(m) => Ok(m),
        _ => Err(Error::Invalid("request body must be a JSON object".into())),
    }
}

fn version_from_headers(headers: &HeaderMap) -> Result<i64> {
    headers
        .get("if-match")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.trim_matches('"').parse::<i64>().ok())
        .ok_or_else(|| Error::Invalid("If-Match version header required".into()))
}

fn parse_list_params(q: ListQuery) -> Result<ListParams> {
    let mut filters = Vec::new();
    for f in q.filter {
        let mut parts = f.splitn(3, ':');
        let field = parts.next().unwrap_or("").trim().to_string();
        let op = parts.next().unwrap_or("").trim().to_string();
        let value = parts.next().unwrap_or("").to_string();
        if field.is_empty() || op.is_empty() {
            return Err(Error::Invalid(format!("bad filter: {f}")));
        }
        filters.push(mda_data::Filter { field, op, value });
    }
    let mut sort = Vec::new();
    for s in q.sort {
        let s = s.trim();
        if let Some(stripped) = s.strip_prefix('-') {
            sort.push(mda_data::Sort {
                field: stripped.to_string(),
                asc: false,
            });
        } else {
            sort.push(mda_data::Sort {
                field: s.to_string(),
                asc: true,
            });
        }
    }
    Ok(ListParams {
        filters,
        sort,
        page: q.page.unwrap_or(1),
        page_size: q.page_size.unwrap_or(0),
    })
}
