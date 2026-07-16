//! Request extractors.

use async_trait::async_trait;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use uuid::Uuid;

/// Resolve the tenant from the `X-Tenant-Id` header, defaulting to the bootstrap
/// tenant (all-zeros). A Phase-1 stand-in: Phase 3 derives the tenant from real
/// auth (PLAN §5.4 / §5.11).
#[derive(Debug, Clone, Copy)]
pub struct TenantId(pub Uuid);

#[async_trait]
impl<S> FromRequestParts<S> for TenantId
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let id = parts
            .headers
            .get("x-tenant-id")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| Uuid::parse_str(s).ok())
            .unwrap_or_else(Uuid::nil);
        Ok(TenantId(id))
    }
}
