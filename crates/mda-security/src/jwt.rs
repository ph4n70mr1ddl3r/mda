//! JWT issue/verify (HS256). Secret from `MDA_JWT_SECRET` (a dev default warns).
//!
//! Three token types share one signing key, distinguished by the `typ` claim.
//! `access` (15 m) is sent as `Authorization: Bearer`, verified statelessly on
//! every request, and carries the session id (`sid`) so logout can revoke it.
//! `refresh` (7 d) is used only at `/api/auth/refresh`, which checks the matching
//! `sec_session` row and rotates it; reuse of a rotated refresh revokes every
//! session for the user (refresh-token-theft containment). `ticket` (60 s) is a
//! one-shot token used as `?ticket=` for the SSE stream (browser `EventSource`
//! can't set headers); it carries no privileges of its own — the events handler
//! resolves it to an identity — so it never exposes the access JWT in a URL.

use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use mda_core::{Error, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Access/refresh/ticket token claims.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,    // user id
    pub tenant: String, // tenant id
    /// `"access"` | `"refresh"` | `"ticket"` — checked by the typed verify methods
    /// so a token of one type can't be used where another is required.
    #[serde(default)]
    pub typ: String,
    /// Session id (access + refresh). Absent on tickets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    pub exp: usize,
    pub iat: usize,
}

pub const ACCESS: &str = "access";
pub const REFRESH: &str = "refresh";
pub const TICKET: &str = "ticket";

#[derive(Clone)]
pub struct JwtConfig {
    encoding: EncodingKey,
    decoding: DecodingKey,
    access_ttl: Duration,
    refresh_ttl: Duration,
    ticket_ttl: Duration,
}

/// An access + refresh pair sharing a session id.
pub struct AccessToken {
    pub access: String,
    pub refresh: String,
}

/// Minimum entropy for HMAC-SHA256 keys (32 bytes = 256 bits).
const MIN_SECRET_LEN: usize = 32;

impl JwtConfig {
    pub fn from_env() -> Self {
        let secret = match std::env::var("MDA_JWT_SECRET") {
            Ok(s) if s.len() >= MIN_SECRET_LEN => s,
            Ok(_) => {
                let msg = format!(
                    "MDA_JWT_SECRET is too short (need ≥ {MIN_SECRET_LEN} bytes for HS256)"
                );
                if cfg!(debug_assertions) {
                    tracing::warn!("{msg} — using insecure dev default");
                    "dev-insecure-secret-change-me-32b!".to_string()
                } else {
                    panic!("{msg}");
                }
            }
            Err(_) => {
                if cfg!(debug_assertions) {
                    tracing::warn!(
                        "MDA_JWT_SECRET unset — using insecure dev default \
                         (set it before deploying!)"
                    );
                    "dev-insecure-secret-change-me-32b!".to_string()
                } else {
                    panic!(
                        "MDA_JWT_SECRET is required in release mode \
                         (generate with: openssl rand -hex 32)"
                    );
                }
            }
        };
        let ticket_ttl = Duration::seconds(env_secs("MDA_TICKET_TTL_SECS", 60).max(5) as i64);
        Self {
            encoding: EncodingKey::from_secret(secret.as_bytes()),
            decoding: DecodingKey::from_secret(secret.as_bytes()),
            access_ttl: Duration::minutes(15),
            refresh_ttl: Duration::days(7),
            ticket_ttl,
        }
    }

    fn issue(
        &self,
        user: Uuid,
        tenant: Uuid,
        typ: &str,
        sid: Option<Uuid>,
        ttl: Duration,
    ) -> Result<String> {
        let now = Utc::now();
        let claims = Claims {
            sub: user.to_string(),
            tenant: tenant.to_string(),
            typ: typ.to_string(),
            sid: sid.map(|s| s.to_string()),
            iat: now.timestamp() as usize,
            exp: (now + ttl).timestamp() as usize,
        };
        encode(&Header::default(), &claims, &self.encoding).map_err(Error::internal)
    }

    /// Issue an access + refresh pair bound to `sid` (a `sec_session` row).
    pub fn issue_pair(&self, user: Uuid, tenant: Uuid, sid: Uuid) -> Result<AccessToken> {
        Ok(AccessToken {
            access: self.issue(user, tenant, ACCESS, Some(sid), self.access_ttl)?,
            refresh: self.issue(user, tenant, REFRESH, Some(sid), self.refresh_ttl)?,
        })
    }

    /// Issue a stateless access token. `sid` should be `Some` for tokens issued
    /// by login (so logout can revoke the session) and `None` for direct/test
    /// issuance that doesn't participate in sessions.
    pub fn issue_access(&self, user: Uuid, tenant: Uuid, sid: Option<Uuid>) -> Result<String> {
        self.issue(user, tenant, ACCESS, sid, self.access_ttl)
    }

    /// Issue a one-shot SSE ticket (very short TTL).
    pub fn issue_ticket(&self, user: Uuid, tenant: Uuid) -> Result<String> {
        self.issue(user, tenant, TICKET, None, self.ticket_ttl)
    }

    /// Refresh-token TTL; sessions inherit this as their expiry.
    pub fn refresh_ttl(&self) -> Duration {
        self.refresh_ttl
    }

    /// Ticket TTL in seconds, for the `expires_in` field of the ticket response.
    pub fn ticket_ttl_secs(&self) -> u64 {
        self.ticket_ttl.num_seconds().max(0) as u64
    }

    fn verify_typed(&self, token: &str, typ: &str) -> Result<Claims> {
        let claims = decode::<Claims>(token, &self.decoding, &Validation::default())
            .map(|d| d.claims)
            .map_err(Error::internal)?;
        if claims.typ != typ {
            return Err(Error::Invalid(format!("expected {typ} token")));
        }
        Ok(claims)
    }

    pub fn verify_access(&self, token: &str) -> Result<Claims> {
        self.verify_typed(token, ACCESS)
    }
    pub fn verify_refresh(&self, token: &str) -> Result<Claims> {
        self.verify_typed(token, REFRESH)
    }
    pub fn verify_ticket(&self, token: &str) -> Result<Claims> {
        self.verify_typed(token, TICKET)
    }
}

fn env_secs(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}
