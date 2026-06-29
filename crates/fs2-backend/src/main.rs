//! fs2-backend binary entry point.

use fs2_backend::config::BackendConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = BackendConfig::from_env();
    fs2_backend::routes::run_server(config).await
}
