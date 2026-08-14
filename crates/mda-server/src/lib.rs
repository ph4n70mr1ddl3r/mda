//! `mda-server` — wiring & bootstrap.

pub mod bootstrap;
pub mod config;
pub mod migrate;
pub mod outbox;

use anyhow::Context;
use axum::Router;
use sqlx::postgres::PgPoolOptions;

/// Run the server to completion: connect, migrate, bootstrap, bind, serve.
pub async fn run() -> anyhow::Result<()> {
    let cfg = config::Settings::load()?;
    init_tracing(&cfg);
    tracing::info!(host = %cfg.host, port = cfg.port, "starting mda");

    let pool = PgPoolOptions::new()
        .max_connections(cfg.db_max_connections)
        .connect(&cfg.database_url)
        .await
        .context("connecting to database")?;

    migrate::run(&pool).await?;
    tracing::info!("database migrated");

    // Ensure a bootstrap admin exists (idempotent) for the bootstrap tenant.
    bootstrap::ensure_admin(&pool).await?;

    // The app serves requests as the non-superuser `mda_app` role so the biz.*
    // RLS policies engage (superusers/owners BYPASS RLS). Migrations + bootstrap
    // above ran as the owner; from here the runtime uses the low-privilege pool
    // when MDA_APP_DATABASE_URL is set. Release refuses to boot without it:
    // owner-mode silently disarms tenant isolation at the DB layer (the same
    // works-as-owner blind spot that hid the schema/grant bugs — see
    // docs/HARDENING.md). Dev builds warn and continue.
    let app_pool = match &cfg.app_database_url {
        Some(url) => {
            tracing::info!("app role pool: mda_app (RLS active)");
            PgPoolOptions::new()
                .max_connections(cfg.db_max_connections)
                .connect(url)
                .await
                .context("connecting app (mda_app) pool")?
        }
        None => {
            if cfg!(debug_assertions) {
                tracing::warn!(
                    "MDA_APP_DATABASE_URL unset — running as the owner; biz.* RLS is INERT. \
                     Set it to the mda_app role for tenant isolation at the DB layer."
                );
            } else {
                panic!(
                    "MDA_APP_DATABASE_URL is required in release mode — without it the app \
                     serves as the database owner and biz.* RLS (tenant isolation) does not \
                     engage. Point it at the non-superuser mda_app role \
                     (postgres://mda_app:…@host/mda)."
                );
            }
            pool.clone()
        }
    };

    // Metadata cache + invalidation (PLAN §5.3). Background workers touch only
    // sys_* / meta tables (no RLS in this scope), so the app role pool is safe.
    let cache = mda_meta::MetadataCache::new();
    mda_meta::cache::spawn_listen(app_pool.clone(), cache.clone());
    mda_meta::cache::spawn_poll(app_pool.clone(), cache.clone());
    outbox::spawn_drain(app_pool.clone());
    mda_api::schedules::spawn_scheduler(app_pool.clone());
    mda_api::notifications::spawn_digest(app_pool.clone());
    mda_api::webhooks::spawn_relay(app_pool.clone());

    let blobs: std::sync::Arc<dyn mda_api::blobs::BlobStore> =
        std::sync::Arc::new(mda_api::blobs::LocalBlobStore::from_env());
    let secrets: std::sync::Arc<dyn mda_core::SecretStore> =
        std::sync::Arc::new(mda_api::secrets::LocalSecretStore::from_env());
    let events = mda_api::events::channel();
    mda_api::events::spawn_listen(app_pool.clone(), events.clone());
    // Hourly purge of stale login-throttle rows (bounded by distinct account/IP
    // keys, but attacker IP churn would otherwise grow it indefinitely).
    {
        let cleanup_pool = app_pool.clone();
        tokio::spawn(async move {
            mda_security::login_throttle::prune(&cleanup_pool).await;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                mda_security::login_throttle::prune(&cleanup_pool).await;
            }
        });
    }

    let app: Router = {
        // The GraphQL schema cache is shared with the invalidator worker, so bind
        // it by name and pass it to both the AppState and the LISTEN worker
        // (ADR-0020 follow-up: a publish rebuilds the schema, and stale version
        // entries are evicted so they do not accumulate across publishes).
        let gql: std::sync::Arc<tokio::sync::RwLock<mda_api::graphql::SchemaCache>> =
            std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        mda_api::graphql::spawn_invalidator(
            app_pool.clone(),
            mda_api::AppState {
                pool: app_pool.clone(),
                cache: cache.clone(),
                jwt: mda_security::JwtConfig::from_env(),
                blobs: blobs.clone(),
                secrets: secrets.clone(),
                events: events.clone(),
                login_throttle: mda_security::LoginThrottle::from_env(),
                gql: gql.clone(),
            },
        );
        mda_api::router(mda_api::AppState {
            pool: app_pool,
            cache,
            jwt: mda_security::JwtConfig::from_env(),
            blobs,
            secrets,
            events,
            login_throttle: mda_security::LoginThrottle::from_env(),
            gql,
        })
    };

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("listening on {addr}");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("server error")?;

    tracing::info!("shutdown complete");
    Ok(())
}

/// Wait for Ctrl-C or SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
}

fn init_tracing(cfg: &config::Settings) {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.log_default));

    match cfg.log_format.as_str() {
        "json" => fmt().with_env_filter(filter).json().init(),
        _ => fmt().with_env_filter(filter).init(),
    }
}
