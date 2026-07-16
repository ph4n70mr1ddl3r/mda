//! `mda-server` — wiring & bootstrap. Constructs config, tracing, the DB pool,
//! the metadata cache (+ invalidation tasks), runs migrations, and serves the
//! API with graceful shutdown.

pub mod config;
pub mod migrate;

use anyhow::Context;
use axum::Router;
use mda_api::AppState;
use sqlx::postgres::PgPoolOptions;

/// Run the server to completion: connect, migrate, bind, serve, shut down.
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

    // Metadata cache + invalidation (PLAN §5.3): LISTEN fast path + version poll.
    let cache = mda_meta::MetadataCache::new();
    mda_meta::cache::spawn_listen(pool.clone(), cache.clone());
    mda_meta::cache::spawn_poll(pool.clone(), cache.clone());

    let app: Router = mda_api::router(AppState {
        pool: pool.clone(),
        cache,
    });

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("listening on {addr}");

    axum::serve(listener, app)
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
