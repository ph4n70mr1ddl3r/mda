//! API client for the MDA runtime (gloo-net + bearer auth).

use serde::Deserialize;

const API_BASE: &str = "http://localhost:8080";

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
    let resp = gloo_net::http::Request::post(&format!("{API_BASE}/api/auth/login"))
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
    let resp = gloo_net::http::Request::get(&format!("{API_BASE}/api/studio/model"))
        .header("Authorization", &format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json::<ModelInfo>().await.map_err(|e| e.to_string())
}

pub async fn list_records(token: &str, entity: &str) -> Result<ListResult, String> {
    let resp = gloo_net::http::Request::get(&format!("{API_BASE}/api/data/{entity}"))
        .header("Authorization", &format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json::<ListResult>().await.map_err(|e| e.to_string())
}

pub async fn get_record(token: &str, entity: &str, id: &str) -> Result<serde_json::Value, String> {
    let resp = gloo_net::http::Request::get(&format!("{API_BASE}/api/data/{entity}/{id}"))
        .header("Authorization", &format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| e.to_string())
}

pub async fn create_record(token: &str, entity: &str, body: String) -> Result<(), String> {
    let resp = gloo_net::http::Request::post(&format!("{API_BASE}/api/data/{entity}"))
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
    let resp = gloo_net::http::Request::patch(&format!("{API_BASE}/api/data/{entity}/{id}"))
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
    let resp = gloo_net::http::Request::delete(&format!("{API_BASE}/api/data/{entity}/{id}"))
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
        "{API_BASE}/api/events?ticket={}&channel={}",
        pct_enc(ticket),
        pct_enc(channel),
    )
}

/// Fetch a short-lived, one-shot SSE ticket. The access JWT goes in the header;
/// the returned ticket is what `EventSource` carries in the URL (so the JWT
/// never appears there).
pub async fn event_ticket(token: &str) -> Result<String, String> {
    let resp = gloo_net::http::Request::post(&format!("{API_BASE}/api/auth/event-ticket"))
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
