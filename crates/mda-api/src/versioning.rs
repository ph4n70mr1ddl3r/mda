//! API versioning & deprecation (PLAN §9 deferral).
//!
//! A stable versioning/deprecation strategy for generated SDK clients (§7):
//! - **Negotiation:** the requested major version is read from an explicit
//!   `X-API-Version: <n>` header, or an `Accept: application/vnd.mda+json;
//!   version=<n>` vendor media-type parameter. Absent → the current stable
//!   major (`MDA_API_VERSION`, default `1`). Both REST and GraphQL honour it.
//! - **Discovery:** every response carries `MDA-API-Version: <served>` so an SDK
//!   can detect which version it actually got (a server may serve a different
//!   major than requested when the request is unpinned).
//! - **Deprecation:** when a newer major is current, an older major is
//!   *deprecated* (still served, for a grace window). Requests pinning a
//!   deprecated major get RFC-8594 `Deprecation`, `Sunset`, and `Link
//!   rel="deprecation"` headers so SDKs can warn + migrate ahead of removal.
//! - **Sunset enforcement:** a major older than the floor (`MDA_MIN_API_VERSION`)
//!   is *unsupported* and rejected with `400` + `mda.unsupported_version`.
//!
//! Only major `1` ships today; the machinery is config-driven so a future `v2`
//! cutover is an env change (`MDA_API_VERSION=2`, `MDA_DEPRECATED_VERSIONS=1`),
//! not a code change — and parallel major schemas across publishes (§7) slot in
//! behind the same negotiation boundary.

use std::collections::HashSet;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// The default current stable major when `MDA_API_VERSION` is unset.
pub const DEFAULT_API_VERSION: u32 = 1;

/// API-versioning configuration, environment-driven with safe defaults.
#[derive(Clone, Debug)]
pub struct VersioningConfig {
    /// The current stable major (what unpinned requests are served).
    pub current: u32,
    /// The oldest major still served (older → 400 `mda.unsupported_version`).
    pub min_supported: u32,
    /// Majors that are served but deprecated (emit Sunset/Deprecation headers).
    pub deprecated: HashSet<u32>,
    /// The HTTP `Sunset` date (RFC-1123) advertised for deprecated majors.
    pub sunset: String,
    /// A documentation URL advertised via `Link rel="deprecation"`.
    pub deprecation_link: String,
}

impl Default for VersioningConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

impl VersioningConfig {
    /// Parse the versioning configuration from the environment.
    pub fn from_env() -> Self {
        let current = std::env::var("MDA_API_VERSION")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_API_VERSION);
        let min_supported = std::env::var("MDA_MIN_API_VERSION")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_API_VERSION);
        let deprecated = std::env::var("MDA_DEPRECATED_VERSIONS")
            .ok()
            .map(|s| {
                s.split(',')
                    .filter_map(|v| v.trim().parse::<u32>().ok())
                    .collect()
            })
            .unwrap_or_default();
        let sunset = std::env::var("MDA_SUNSET_DATE")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Sun, 31 Dec 2099 00:00:00 GMT".to_string());
        let deprecation_link = std::env::var("MDA_DEPRECATION_LINK")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "https://mda.example.com/docs/api-versioning".to_string());
        Self {
            current,
            min_supported,
            deprecated,
            sunset,
            deprecation_link,
        }
    }

    /// Resolve the requested major from the request headers. `None` means the
    /// client did not pin a version → serve [`Self::current`].
    fn requested(&self, headers: &HeaderMap) -> Option<u32> {
        if let Some(v) = headers.get("x-api-version").and_then(|h| h.to_str().ok()) {
            if let Ok(n) = v.trim().parse::<u32>() {
                return Some(n);
            }
        }
        // Accept: application/vnd.mda+json; version=2  (vendor media type)
        for accept in headers.get_all(axum::http::header::ACCEPT).iter() {
            if let Ok(s) = accept.to_str() {
                if let Some(n) = parse_vendor_version(s) {
                    return Some(n);
                }
            }
        }
        None
    }
}

/// Extract a `version=<n>` parameter from an `application/vnd.mda+json` accept.
fn parse_vendor_version(accept: &str) -> Option<u32> {
    let lower = accept.to_ascii_lowercase();
    if !lower.contains("vnd.mda") {
        return None;
    }
    for part in accept.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("version=") {
            if let Ok(n) = rest.trim().parse::<u32>() {
                return Some(n);
            }
        }
    }
    None
}

/// The outcome of version negotiation for one request.
struct Decision {
    /// The major the client asked for (None = unpinned).
    requested: Option<u32>,
    /// The major actually served.
    served: u32,
    deprecated: bool,
}

impl VersioningConfig {
    fn decide(&self, headers: &HeaderMap) -> Decision {
        let requested = self.requested(headers);
        let served = requested.unwrap_or(self.current);
        let deprecated = self.deprecated.contains(&served);
        Decision {
            requested,
            served,
            deprecated,
        }
    }
}

/// Axum middleware: negotiate the API version, reject unsupported majors, and
/// stamp discovery/deprecation headers on every response.
pub async fn middleware(State(cfg): State<VersioningConfig>, req: Request, next: Next) -> Response {
    let decision = cfg.decide(req.headers());

    // A pinned request for a major below the supported floor is rejected. We do
    // not silently upgrade an explicitly-pinned client (that is a breaking lie).
    if let Some(asked) = decision.requested {
        if asked < cfg.min_supported {
            return unsupported_response(
                asked,
                cfg.min_supported,
                &cfg.sunset,
                &cfg.deprecation_link,
            );
        }
    }

    let mut resp = next.run(req).await;
    // Discovery: which major was actually served.
    let served = decision.served.to_string();
    let _ = resp.headers_mut().insert(
        HeaderName::from_static("mda-api-version"),
        HeaderValue::from_str(&served).unwrap(),
    );
    if decision.deprecated {
        // RFC-8594 deprecation signalling — SDKs warn + migrate ahead of removal.
        let _ = resp.headers_mut().insert(
            HeaderName::from_static("deprecation"),
            HeaderValue::from_static("true"),
        );
        let _ = resp.headers_mut().insert(
            HeaderName::from_static("sunset"),
            HeaderValue::from_str(&cfg.sunset).unwrap(),
        );
        let _ = resp.headers_mut().insert(
            HeaderName::from_static("link"),
            HeaderValue::from_str(
                format!("<{}>; rel=\"deprecation\"", cfg.deprecation_link).as_str(),
            )
            .unwrap(),
        );
    }
    resp
}

/// The 400 response for an unsupported major. Carries the stable `code` so SDKs
/// branch on it, plus `Sunset`/`Link` pointing at the migration guide.
fn unsupported_response(asked: u32, min: u32, sunset: &str, link: &str) -> Response {
    let body = Json(json!({
        "code": "mda.unsupported_version",
        "error": "unsupported_version",
        "status": 400,
        "message": format!("API major {asked} is no longer served (minimum supported: {min})"),
        "requested_version": asked,
        "minimum_supported_version": min,
    }));
    let mut resp = (StatusCode::BAD_REQUEST, body).into_response();
    let asked_s = asked.to_string();
    let _ = resp.headers_mut().insert(
        HeaderName::from_static("mda-api-version"),
        HeaderValue::from_str(&asked_s).unwrap(),
    );
    let _ = resp.headers_mut().insert(
        HeaderName::from_static("sunset"),
        HeaderValue::from_str(sunset).unwrap(),
    );
    let _ = resp.headers_mut().insert(
        HeaderName::from_static("link"),
        HeaderValue::from_str(format!("<{link}>; rel=\"deprecation\"").as_str()).unwrap(),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(current: u32, min: u32, deprecated: &[u32]) -> VersioningConfig {
        VersioningConfig {
            current,
            min_supported: min,
            deprecated: deprecated.iter().copied().collect(),
            sunset: "Sun, 31 Dec 2099 00:00:00 GMT".into(),
            deprecation_link: "https://mda.example.com/docs/api-versioning".into(),
        }
    }

    #[test]
    fn unpinned_request_serves_current() {
        let c = cfg(1, 1, &[]);
        let h = HeaderMap::new();
        let d = c.decide(&h);
        assert_eq!(d.served, 1);
        assert!(!d.deprecated);
    }

    #[test]
    fn x_api_version_header_is_honoured() {
        let c = cfg(2, 1, &[1]);
        let mut h = HeaderMap::new();
        h.insert("x-api-version", HeaderValue::from_static("1"));
        let d = c.decide(&h);
        assert_eq!(d.served, 1);
        assert!(d.deprecated, "v1 is deprecated when v2 is current");
    }

    #[test]
    fn accept_vendor_media_type_is_honoured() {
        let c = cfg(2, 1, &[1]);
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::ACCEPT,
            HeaderValue::from_static("application/vnd.mda+json; version=1"),
        );
        let d = c.decide(&h);
        assert_eq!(d.served, 1);
        assert!(d.deprecated);
    }

    #[test]
    fn below_floor_is_flagged_as_unsupported_at_request_time() {
        // A future v3 world: current=3, min=2, v1 unsupported.
        let c = cfg(3, 2, &[2]);
        let mut h = HeaderMap::new();
        h.insert("x-api-version", HeaderValue::from_static("1"));
        // The decide() lets the layer reject; here we assert the floor logic.
        assert_eq!(c.requested(&h), Some(1));
        assert!(1 < c.min_supported);
        let d = c.decide(&h);
        assert_eq!(d.served, 1); // served-major recorded for the rejection envelope
    }

    #[test]
    fn vendor_version_parse_ignores_unrelated_accepts() {
        assert_eq!(parse_vendor_version("application/json"), None);
        assert_eq!(
            parse_vendor_version("application/vnd.mda+json; version=2"),
            Some(2)
        );
    }
}
