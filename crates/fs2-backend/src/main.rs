//! FS2 backend service entry point.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    fs2_backend::init_tracing();
    let config = fs2_backend::BackendConfig::from_env()?;
    fs2_backend::serve(config, shutdown_signal()).await?;
    Ok(())
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(%error, "failed to listen for shutdown signal");
    }
}
