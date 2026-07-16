//! MDA server binary — wires config, tracing, DB pool, migrations, and the API.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    mda_server::run().await
}
