//! Internationalization — metadata/UI string translations (PLAN §9 / Phase 11
//! deferral).
//!
//! `meta.md_translation` holds one value per `(locale, namespace, msg_key)`.
//! `locale = ''` is the default/fallback bundle. A request locale resolves
//! **best-match**: exact (`en-US`) → language prefix (`en`) → default (`''`),
//! so a partial translation falls back gracefully to the default bundle.
//!
//! Covers **metadata/UI strings only** for v1 (labels, messages, template
//! strings). Record-data i18n (translatable enum/reference data, multi-language
//! record fields) is the explicitly-deferred U5 and stays out until a real
//! multi-locale tenant needs it. The translation bundle is also injected into
//! the template render context (§5.19) so a template localizes with
//! `{{ i18n.greeting }}` — pure strings, so AuthZ-by-construction is preserved.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use mda_core::Error;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/translations",
            post(upsert_translation).get(list_translations),
        )
        .route(
            "/api/translations/:locale",
            get(get_bundle).delete(delete_locale),
        )
        .route(
            "/api/translations/:locale/:namespace/:key",
            axum::routing::delete(delete_translation),
        )
        .route("/api/i18n/:locale", get(get_bundle))
}

#[derive(Debug, Deserialize)]
struct UpsertBody {
    /// `''` (or omitted) = the default/fallback bundle.
    #[serde(default)]
    locale: String,
    #[serde(default = "default_namespace")]
    namespace: String,
    key: String,
    value: String,
}

fn default_namespace() -> String {
    "ui".to_string()
}

#[derive(Debug, Serialize)]
struct TranslationRow {
    locale: String,
    namespace: String,
    key: String,
    value: String,
    updated_at: String,
}

#[derive(Debug, Deserialize, Default)]
struct ListQuery {
    #[serde(default)]
    namespace: Option<String>,
}

/// `POST /api/translations` — upsert one translation (create or update by
/// natural key `(locale, namespace, key)`). Idempotent.
async fn upsert_translation(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<UpsertBody>,
) -> ApiResult<(StatusCode, Json<TranslationRow>)> {
    if body.key.trim().is_empty() {
        return Err(Error::Invalid("key is required".into()).into());
    }
    if body.namespace.trim().is_empty() {
        return Err(Error::Invalid("namespace must not be empty".into()).into());
    }
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let row: (
        String,
        String,
        String,
        String,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as(
        "INSERT INTO meta.md_translation (tenant_id, locale, namespace, msg_key, value)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (tenant_id, locale, namespace, msg_key)
         DO UPDATE SET value = EXCLUDED.value, updated_at = now()
         RETURNING locale, namespace, msg_key, value, updated_at",
    )
    .bind(user.tenant_id)
    .bind(&body.locale)
    .bind(&body.namespace)
    .bind(&body.key)
    .bind(&body.value)
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok((
        StatusCode::OK,
        Json(TranslationRow {
            locale: row.0,
            namespace: row.1,
            key: row.2,
            value: row.3,
            updated_at: row.4.to_rfc3339(),
        }),
    ))
}

/// `GET /api/translations` — raw management list (all locales for the tenant).
async fn list_translations(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<TranslationRow>>> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let rows: Vec<(
        String,
        String,
        String,
        String,
        chrono::DateTime<chrono::Utc>,
    )> = if let Some(ns) = q.namespace {
        sqlx::query_as(
            "SELECT locale, namespace, msg_key, value, updated_at FROM meta.md_translation
              WHERE tenant_id = $1 AND namespace = $2
              ORDER BY namespace, locale, msg_key",
        )
        .bind(user.tenant_id)
        .bind(ns)
        .fetch_all(&mut *tx)
        .await
        .map_err(Error::internal)?
    } else {
        sqlx::query_as(
            "SELECT locale, namespace, msg_key, value, updated_at FROM meta.md_translation
              WHERE tenant_id = $1 ORDER BY namespace, locale, msg_key",
        )
        .bind(user.tenant_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(Error::internal)?
    };
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(
        rows.into_iter()
            .map(|r| TranslationRow {
                locale: r.0,
                namespace: r.1,
                key: r.2,
                value: r.3,
                updated_at: r.4.to_rfc3339(),
            })
            .collect(),
    ))
}

/// `GET /api/i18n/:locale` / `GET /api/translations/:locale` — the **resolved**
/// bundle for a locale (best-match: exact → language prefix → default), ready
/// for a UI bootstrap. `?namespace=` scopes the keyspace (default: all).
async fn get_bundle(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(locale): Path<String>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let bundle = resolve_bundle(&st.pool, user.tenant_id, &locale, q.namespace.as_deref()).await?;
    let translations: HashMap<String, String> = bundle
        .into_iter()
        .map(|((ns, key), value)| (format!("{ns}.{key}"), value))
        .collect();
    Ok(Json(json!({
        "locale": locale,
        "namespace": q.namespace,
        "translations": translations,
    })))
}

/// `DELETE /api/translations/:locale` — delete an entire locale bundle
/// (`?namespace=` scopes it). Use the specific-key route for one key.
async fn delete_locale(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(locale): Path<String>,
    Query(q): Query<ListQuery>,
) -> ApiResult<StatusCode> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let n = if let Some(ns) = &q.namespace {
        sqlx::query(
            "DELETE FROM meta.md_translation
              WHERE tenant_id = $1 AND locale = $2 AND namespace = $3",
        )
        .bind(user.tenant_id)
        .bind(&locale)
        .bind(ns)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?
        .rows_affected()
    } else {
        sqlx::query("DELETE FROM meta.md_translation WHERE tenant_id = $1 AND locale = $2")
            .bind(user.tenant_id)
            .bind(&locale)
            .execute(&mut *tx)
            .await
            .map_err(Error::internal)?
            .rows_affected()
    };
    tx.commit().await.map_err(Error::internal)?;
    if n == 0 {
        return Err(Error::NotFound(format!("locale {locale}")).into());
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/translations/:locale/:namespace/:key` — delete one translation.
async fn delete_translation(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((locale, namespace, key)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let n = sqlx::query(
        "DELETE FROM meta.md_translation
          WHERE tenant_id = $1 AND locale = $2 AND namespace = $3 AND msg_key = $4",
    )
    .bind(user.tenant_id)
    .bind(&locale)
    .bind(&namespace)
    .bind(&key)
    .execute(&mut *tx)
    .await
    .map_err(Error::internal)?
    .rows_affected();
    tx.commit().await.map_err(Error::internal)?;
    if n == 0 {
        return Err(Error::NotFound(format!("{namespace}.{key}")).into());
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Resolve the best-match translation bundle for a locale. Returns a
/// `(namespace, key) → value` map. Best-match precedence per key:
///   exact locale (en-US) > language prefix (en) > default ('').
/// Exposed (`pub(crate)`) so the template render path can inject the bundle into
/// the render context (§5.19) under `i18n`.
pub(crate) async fn resolve_bundle(
    pool: &sqlx::PgPool,
    tenant: Uuid,
    locale: &str,
    namespace: Option<&str>,
) -> ApiResult<HashMap<(String, String), String>> {
    let lang = locale.split('-').next().unwrap_or("");
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    // Candidate rows: exact locale, language prefix (if different), and default.
    let mut locales: Vec<String> = vec![locale.to_string()];
    if !lang.is_empty() && lang != locale {
        locales.push(lang.to_string());
    }
    locales.push(String::new()); // default bundle
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT locale, namespace, msg_key, value FROM meta.md_translation
          WHERE tenant_id = $1 AND locale = ANY($2)",
    )
    .bind(tenant)
    .bind(&locales)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;

    // Rank each candidate by locale precedence; keep the best per (namespace,key).
    let rank = |l: &str| -> u8 {
        if l == locale {
            0
        } else if l == lang {
            1
        } else {
            2
        }
    };
    let mut best: HashMap<(String, String), (u8, String)> = HashMap::new();
    let ns_filter = namespace.map(|s| s.to_string());
    for (loc, ns, key, value) in rows {
        if let Some(n) = &ns_filter {
            if &ns != n {
                continue;
            }
        }
        let r = rank(&loc);
        let k = (ns, key);
        match best.get(&k) {
            Some((cur, _)) if *cur <= r => {}
            _ => {
                best.insert(k, (r, value));
            }
        }
    }
    Ok(best.into_iter().map(|(k, (_, v))| (k, v)).collect())
}
