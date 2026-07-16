//! JWT issue/verify (HS256). Secret from `MDA_JWT_SECRET` (a dev default warns).

use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use mda_core::{Error, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Access/refresh token claims.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,    // user id
    pub tenant: String, // tenant id
    pub exp: usize,
    pub iat: usize,
}

#[derive(Clone)]
pub struct JwtConfig {
    encoding: EncodingKey,
    decoding: DecodingKey,
    access_ttl: Duration,
    refresh_ttl: Duration,
}

pub struct AccessToken {
    pub access: String,
    pub refresh: String,
}

impl JwtConfig {
    pub fn from_env() -> Self {
        let secret = std::env::var("MDA_JWT_SECRET").unwrap_or_else(|_| {
            tracing::warn!("MDA_JWT_SECRET unset — using insecure dev default");
            "dev-insecure-secret-change-me".to_string()
        });
        Self {
            encoding: EncodingKey::from_secret(secret.as_bytes()),
            decoding: DecodingKey::from_secret(secret.as_bytes()),
            access_ttl: Duration::minutes(15),
            refresh_ttl: Duration::days(7),
        }
    }

    fn issue(&self, user: Uuid, tenant: Uuid, ttl: Duration) -> Result<String> {
        let now = Utc::now();
        let claims = Claims {
            sub: user.to_string(),
            tenant: tenant.to_string(),
            iat: now.timestamp() as usize,
            exp: (now + ttl).timestamp() as usize,
        };
        encode(&Header::default(), &claims, &self.encoding).map_err(Error::internal)
    }

    pub fn issue_pair(&self, user: Uuid, tenant: Uuid) -> Result<AccessToken> {
        Ok(AccessToken {
            access: self.issue(user, tenant, self.access_ttl)?,
            refresh: self.issue(user, tenant, self.refresh_ttl)?,
        })
    }

    pub fn issue_access(&self, user: Uuid, tenant: Uuid) -> Result<String> {
        self.issue(user, tenant, self.access_ttl)
    }

    pub fn verify(&self, token: &str) -> Result<Claims> {
        decode::<Claims>(token, &self.decoding, &Validation::default())
            .map(|d| d.claims)
            .map_err(Error::internal)
    }
}

/// Verify a bearer token and return its claims (convenience for extractors).
pub fn verify_access_token(cfg: &JwtConfig, token: &str) -> Result<Claims> {
    cfg.verify(token)
}
