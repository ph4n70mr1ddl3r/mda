//! API client for the MDA runtime (gloo-net + bearer auth).

use serde::Deserialize;

/// Resolve the API origin (first match wins):
/// 1. `window.__MDA_API_BASE__` — settable at *serve* time (a templated
///    index.html or an inline `<script>` before the bundle loads), so the same
///    static bundle works in any environment; an empty string means same-origin.
/// 2. build-time `MDA_API_BASE` env (`MDA_API_BASE=https://api.example.com trunk build`)
/// 3. the dev default (Trunk on :8081, API on :8080).
pub fn api_base() -> String {
    if let Some(w) = web_sys::window() {
        let key = wasm_bindgen::JsValue::from_str("__MDA_API_BASE__");
        if let Ok(v) = js_sys::Reflect::get(&w.into(), &key) {
            if let Some(s) = v.as_string() {
                return s;
            }
        }
    }
    option_env!("MDA_API_BASE")
        .map(str::to_string)
        .unwrap_or_else(|| "http://localhost:8080".to_string())
}

#[derive(Deserialize, Clone)]
pub struct ModelInfo {
    pub entities: Vec<EntityInfo>,
}

#[derive(Deserialize, Clone)]
pub struct EntityInfo {
    pub name: String,
    pub label: Option<String>,
    pub fields: Vec<FieldInfo>,
    #[allow(dead_code)]
    pub relationships: Vec<RelInfo>,
}

#[derive(Deserialize, Clone)]
pub struct FieldInfo {
    pub name: String,
    pub label: Option<String>,
    pub field_type: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Deserialize, Clone)]
pub struct RelInfo {
    pub source_field_name: String,
}

#[derive(Deserialize, Clone, serde::Serialize)]
pub struct ListResult {
    pub items: Vec<serde_json::Value>,
    pub total: u64,
}

#[derive(Deserialize)]
pub struct TokenResp {
    pub access_token: String,
}

pub async fn login(tenant: &str, email: &str, password: &str) -> Result<String, String> {
    let resp = gloo_net::http::Request::post(&format!("{}/api/auth/login", api_base()))
        .header("content-type", "application/json")
        .body(
            serde_json::json!({"tenant": tenant, "email": email, "password": password}).to_string(),
        )
        .unwrap()
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("Login failed ({})", resp.status()));
    }
    let body: TokenResp = resp.json().await.map_err(|e| e.to_string())?;
    Ok(body.access_token)
}

pub async fn get_model(token: &str) -> Result<ModelInfo, String> {
    let resp = gloo_net::http::Request::get(&format!("{}/api/studio/model", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json::<ModelInfo>().await.map_err(|e| e.to_string())
}

pub async fn list_records(token: &str, entity: &str) -> Result<ListResult, String> {
    let resp = gloo_net::http::Request::get(&format!("{}/api/data/{entity}", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json::<ListResult>().await.map_err(|e| e.to_string())
}

pub async fn get_record(token: &str, entity: &str, id: &str) -> Result<serde_json::Value, String> {
    let resp = gloo_net::http::Request::get(&format!("{}/api/data/{entity}/{id}", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| e.to_string())
}

pub async fn create_record(token: &str, entity: &str, body: String) -> Result<(), String> {
    let resp = gloo_net::http::Request::post(&format!("{}/api/data/{entity}", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(body)
        .unwrap()
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("Create failed ({})", resp.status()));
    }
    Ok(())
}

pub async fn update_record(
    token: &str,
    entity: &str,
    id: &str,
    version: i64,
    body: String,
) -> Result<(), String> {
    let resp = gloo_net::http::Request::patch(&format!("{}/api/data/{entity}/{id}", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .header("if-match", &version.to_string())
        .header("content-type", "application/json")
        .body(body)
        .unwrap()
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("Update failed ({})", resp.status()));
    }
    Ok(())
}

pub async fn delete_record(token: &str, entity: &str, id: &str) -> Result<(), String> {
    let resp = gloo_net::http::Request::delete(&format!("{}/api/data/{entity}/{id}", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("Delete failed ({})", resp.status()));
    }
    Ok(())
}

/// SSE endpoint URL for browser `EventSource` (which can't set headers).
/// `ticket` is a short-lived, one-shot token from [`event_ticket`] — never the
/// access JWT, so no long-lived credential lands in the URL. Both values are
/// percent-encoded so a channel like `record:Customer:<uuid>` survives intact.
pub fn events_url(ticket: &str, channel: &str) -> String {
    format!(
        "{}/api/events?ticket={}&channel={}",
        api_base(),
        pct_enc(ticket),
        pct_enc(channel),
    )
}

/// Fetch a short-lived, one-shot SSE ticket. The access JWT goes in the header;
/// the returned ticket is what `EventSource` carries in the URL (so the JWT
/// never appears there).
pub async fn event_ticket(token: &str) -> Result<String, String> {
    let resp = gloo_net::http::Request::post(&format!("{}/api/auth/event-ticket", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("event-ticket failed ({})", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    body["ticket"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| "missing ticket in response".into())
}

/// Minimal percent-encoder for the query-safe (RFC 3986 unreserved) set. Keeps
/// us off `js_sys::encodeURIComponent` (and its dependency) for two short inputs.
fn pct_enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        }
    }
    out
}

// localStorage helpers
pub fn local_get(key: &str) -> Option<String> {
    let window = web_sys::window()?;
    let storage = window.local_storage().ok()??;
    storage.get_item(key).ok()?
}

pub fn local_set(key: &str, val: &str) {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.set_item(key, val);
        }
    }
}

pub fn local_remove(key: &str) {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.remove_item(key);
        }
    }
}

// ===== UI definitions (Phase 6): navigation, views, forms, dashboards =====

#[derive(Deserialize, Clone, serde::Serialize)]
pub struct NavItem {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub entity: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    pub label: String,
}

pub async fn get_navigation(token: &str) -> Result<Vec<NavItem>, String> {
    let resp = gloo_net::http::Request::get(&format!("{}/api/navigation", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    serde_json::from_value(body["items"].clone()).map_err(|e| e.to_string())
}

#[derive(Deserialize, Clone, serde::Serialize)]
pub struct ViewColumn {
    pub field: String,
    pub label: String,
    #[serde(default)]
    pub r#type: String,
}

#[derive(Deserialize, Clone, serde::Serialize)]
pub struct ViewInfo {
    pub columns: Vec<ViewColumn>,
}

/// The default list-view definition (None when the API has no view — the
/// caller falls back to the raw model fields).
pub async fn get_view(token: &str, entity: &str) -> Result<Option<ViewInfo>, String> {
    let resp = gloo_net::http::Request::get(&format!("{}/api/views/{entity}", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Ok(None);
    }
    resp.json::<ViewInfo>()
        .await
        .map(Some)
        .map_err(|e| e.to_string())
}

#[derive(Deserialize, Clone, serde::Serialize)]
pub struct FormField {
    pub name: String,
    pub label: String,
    #[serde(rename = "type", default)]
    pub field_type: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub widget: String,
    #[serde(default)]
    pub options: serde_json::Value,
    #[serde(default)]
    pub target_entity: Option<String>,
}

#[derive(Deserialize, Clone, serde::Serialize)]
pub struct FormSection {
    pub title: Option<String>,
    pub fields: Vec<FormField>,
}

#[derive(Deserialize, Clone, serde::Serialize)]
pub struct FormInfo {
    pub label: serde_json::Value,
    pub sections: Vec<FormSection>,
}

pub async fn get_form(token: &str, entity: &str) -> Result<Option<FormInfo>, String> {
    let resp = gloo_net::http::Request::get(&format!("{}/api/forms/{entity}", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Ok(None);
    }
    resp.json::<FormInfo>()
        .await
        .map(Some)
        .map_err(|e| e.to_string())
}

#[derive(Deserialize, Clone, serde::Serialize)]
pub struct DashSummary {
    pub id: String,
    pub name: String,
    pub label: String,
}

pub async fn list_dashboards(token: &str) -> Result<Vec<DashSummary>, String> {
    let resp = gloo_net::http::Request::get(&format!("{}/api/dashboards", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Ok(vec![]);
    }
    resp.json::<Vec<DashSummary>>()
        .await
        .map_err(|e| e.to_string())
}

#[derive(Deserialize, Clone, serde::Serialize)]
pub struct DashTile {
    pub title: serde_json::Value,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub result: Option<ReportResult>,
}

#[derive(Deserialize, Clone, serde::Serialize)]
pub struct ReportResult {
    pub columns: Vec<String>,
    pub rows: Vec<serde_json::Value>,
}

#[derive(Deserialize, Clone, serde::Serialize)]
pub struct DashboardInfo {
    pub id: String,
    pub label: String,
    pub items: Vec<DashTile>,
}

pub async fn get_dashboard(token: &str, id: &str) -> Result<DashboardInfo, String> {
    let resp = gloo_net::http::Request::get(&format!("{}/api/dashboards/{id}", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json::<DashboardInfo>()
        .await
        .map_err(|e| e.to_string())
}

// ===== Studio (Phase 8): generic JSON calls + typed helpers =====
//
// The Studio talks to the admin-gated surfaces (draft lifecycle, UI-definition
// authoring, report authoring, rule/workflow authoring, the admin security
// API, import/export). Most payloads are returned as raw JSON — the Studio
// renders them structurally, and keeping the client thin avoids mirroring a
// dozen DTOs in WASM.

async fn send_json(
    method: &str,
    path: &str,
    token: &str,
    body: Option<String>,
    if_match: Option<&str>,
) -> Result<serde_json::Value, String> {
    use gloo_net::http::Method;
    let method = match method {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "PUT" => Method::PUT,
        "PATCH" => Method::PATCH,
        _ => Method::DELETE,
    };
    // Callers pass paths with a leading slash ("/api/…") and api_base() may
    // carry a trailing slash (served env) — normalize both so the join never
    // produces "//" (axum would 404 the doubled path).
    let url = format!(
        "{}/{}",
        api_base().trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let b = gloo_net::http::RequestBuilder::new(&url)
        .method(method)
        .header("Authorization", &format!("Bearer {token}"));
    let b = if let Some(etag) = if_match {
        b.header("if-match", etag)
    } else {
        b
    };
    let resp = match body {
        Some(body) => {
            b.header("content-type", "application/json")
                .body(body)
                .unwrap()
                .send()
                .await
        }
        None => b.send().await,
    }
    .map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !resp_ok(status) {
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v["message"]
                    .as_str()
                    .or_else(|| v["error"].as_str())
                    .map(String::from)
            })
            .unwrap_or(text);
        return Err(format!("HTTP {status}: {msg}"));
    }
    if text.is_empty() {
        Ok(serde_json::Value::Null)
    } else {
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }
}

fn resp_ok(status: u16) -> bool {
    (200..300).contains(&status)
}

pub async fn sget(token: &str, path: &str) -> Result<serde_json::Value, String> {
    send_json("GET", path, token, None, None).await
}

pub async fn spost(
    token: &str,
    path: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    send_json("POST", path, token, Some(body.to_string()), None).await
}

pub async fn spatch(
    token: &str,
    path: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    send_json("PATCH", path, token, Some(body.to_string()), None).await
}

pub async fn sput(
    token: &str,
    path: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    send_json("PUT", path, token, Some(body.to_string()), None).await
}

pub async fn sdelete(token: &str, path: &str) -> Result<serde_json::Value, String> {
    send_json("DELETE", path, token, None, None).await
}

/// `PUT /api/studio/drafts/:id/model` with the OCC etag — the one Studio call
/// that needs a header beyond auth, so it gets its own wrapper.
pub async fn save_draft_model(
    token: &str,
    draft_id: &str,
    etag: &str,
    model: serde_json::Value,
) -> Result<serde_json::Value, String> {
    send_json(
        "PUT",
        &format!("/api/studio/drafts/{draft_id}/model"),
        token,
        Some(model.to_string()),
        Some(etag),
    )
    .await
}

/// `GET /api/auth/me` → is the caller a superuser (Studio gate)?
pub async fn is_admin(token: &str) -> Result<bool, String> {
    let me = sget(token, "/api/auth/me").await?;
    Ok(me["is_superuser"].as_bool().unwrap_or(false))
}
