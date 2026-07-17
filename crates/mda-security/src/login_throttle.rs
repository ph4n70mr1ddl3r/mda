//! Login throttling (PLAN §3): brute-force / credential-stuffing defence that
//! is shared across app instances via Postgres (not in-process) so the limit
//! holds no matter which replica serves the request.
//!
//! Each login attempt is tracked under two keys: `acct:<tenant>:<email>` (a
//! per-account progressive lockout) and `ip:<client_ip>` (a per-IP rate limit
//! that slows rotating-account stuffing from one source). Both are checked
//! before credentials are verified, and both are incremented on every failure.
//! The table (`sys.sys_login_throttle`) is intentionally not under RLS — IP
//! rows are tenant-agnostic, and account rows are matched by an exact key
//! lookup, so RLS would only get in the way.
//!
//! Shape: after `max_fails` failures within a rolling `window`, the key is
//! locked for `lockout`. A successful login clears the key, and a burst older
//! than `window` resets — so an occasional typo won't eventually lock a
//! legitimate user out.

use std::time::Duration;

use mda_core::{Error, Result};
use sqlx::PgPool;
use uuid::Uuid;

/// How long after the last attempt a throttle row is retained before the
/// [`spawn_cleanup`] worker deletes it. The table is bounded by distinct
/// (account, IP) keys, but attacker IP churn could otherwise grow it.
const RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);

/// Configuration for the login throttle. Defaults follow the common
/// "5 attempts / 15-minute window / 15-minute lockout" shape; override per env.
#[derive(Clone, Copy, Debug)]
pub struct LoginThrottle {
    /// Failed attempts within `window` that trigger a lockout.
    pub max_fails: i32,
    /// The rolling window over which failures accumulate.
    pub window: Duration,
    /// How long the key stays locked once `max_fails` is reached.
    pub lockout: Duration,
}

impl Default for LoginThrottle {
    fn default() -> Self {
        Self {
            max_fails: 5,
            window: Duration::from_secs(15 * 60),
            lockout: Duration::from_secs(15 * 60),
        }
    }
}

impl LoginThrottle {
    /// Load overrides from the environment, falling back to [`Default`].
    ///   - `MDA_LOGIN_MAX_FAILS`    (default 5)
    ///   - `MDA_LOGIN_WINDOW_SECS`  (default 900)
    ///   - `MDA_LOGIN_LOCKOUT_SECS` (default 900)
    pub fn from_env() -> Self {
        fn env_usize(key: &str, default: usize) -> usize {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|&v| v > 0)
                .unwrap_or(default)
        }
        Self {
            max_fails: env_usize("MDA_LOGIN_MAX_FAILS", 5) as i32,
            window: Duration::from_secs(env_usize("MDA_LOGIN_WINDOW_SECS", 900) as u64),
            lockout: Duration::from_secs(env_usize("MDA_LOGIN_LOCKOUT_SECS", 900) as u64),
        }
    }

    /// Per-account key: `acct:<tenant>:<lowercased email>`.
    pub fn account_key(tenant: Uuid, email: &str) -> String {
        format!("acct:{tenant}:{}", email.trim().to_lowercase())
    }

    /// Per-IP key: `ip:<ip>`.
    pub fn ip_key(ip: &str) -> String {
        format!("ip:{}", ip.trim())
    }

    /// Is `key` currently locked? (A prior burst reached the threshold and the
    /// lockout has not yet elapsed.)
    pub async fn is_locked(&self, pool: &PgPool, key: &str) -> Result<bool> {
        let locked: Option<bool> = sqlx::query_scalar(
            "SELECT locked_until IS NOT NULL AND locked_until > now()
               FROM sys_login_throttle
              WHERE key = $1",
        )
        .bind(key)
        .fetch_optional(pool)
        .await
        .map_err(Error::internal)?;
        Ok(locked.unwrap_or(false))
    }

    /// Record a failed attempt for `key`. Increments the burst counter (resetting
    /// it if the window has elapsed since the first failure in this burst) and
    /// sets `locked_until` once the threshold is reached. Atomic per key via
    /// `INSERT … ON CONFLICT DO UPDATE`, so concurrent logins can't undercount.
    pub async fn record_failure(&self, pool: &PgPool, key: &str) -> Result<()> {
        let window = self.window.as_secs_f64();
        let lockout = self.lockout.as_secs_f64();
        let max = self.max_fails;
        sqlx::query(
            "INSERT INTO sys_login_throttle
                (key, fail_count, first_failed_at, last_attempt_at, locked_until)
             VALUES ($1, 1, now(), now(), NULL)
             ON CONFLICT (key) DO UPDATE SET
                -- Reset the burst if the window has elapsed since its first fail.
                first_failed_at = CASE
                    WHEN sys_login_throttle.first_failed_at IS NULL
                      OR now() - sys_login_throttle.first_failed_at
                           > make_interval(secs => $2)
                    THEN now()
                    ELSE sys_login_throttle.first_failed_at
                END,
                fail_count = CASE
                    WHEN sys_login_throttle.first_failed_at IS NULL
                      OR now() - sys_login_throttle.first_failed_at
                           > make_interval(secs => $2)
                    THEN 1
                    ELSE sys_login_throttle.fail_count + 1
                END,
                last_attempt_at = now(),
                -- Lock (or extend) once the new count reaches the threshold.
                locked_until = CASE
                    WHEN (
                        CASE
                            WHEN sys_login_throttle.first_failed_at IS NULL
                              OR now() - sys_login_throttle.first_failed_at
                                   > make_interval(secs => $2)
                            THEN 1
                            ELSE sys_login_throttle.fail_count + 1
                        END
                    ) >= $3
                    THEN now() + make_interval(secs => $4)
                    ELSE sys_login_throttle.locked_until
                END",
        )
        .bind(key)
        .bind(window)
        .bind(max)
        .bind(lockout)
        .execute(pool)
        .await
        .map_err(Error::internal)?;
        Ok(())
    }

    /// Clear all failure state for `key` (called on a successful login).
    pub async fn record_success(&self, pool: &PgPool, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM sys_login_throttle WHERE key = $1")
            .bind(key)
            .execute(pool)
            .await
            .map_err(Error::internal)?;
        Ok(())
    }
}

/// Delete throttle rows whose last attempt is older than [`RETENTION`].
/// DB-only (no runtime) so it can be driven by whichever crate owns the tokio
/// runtime — `mda_server` spawns an hourly loop over this.
pub async fn prune(pool: &PgPool) {
    let secs = RETENTION.as_secs_f64();
    match sqlx::query(
        "DELETE FROM sys_login_throttle
          WHERE last_attempt_at < now() - make_interval(secs => $1)",
    )
    .bind(secs)
    .execute(pool)
    .await
    {
        Ok(res) => {
            let n = res.rows_affected();
            if n > 0 {
                tracing::debug!(removed = n, "login_throttle cleanup");
            }
        }
        Err(e) => tracing::warn!(?e, "login_throttle cleanup failed"),
    }
}
