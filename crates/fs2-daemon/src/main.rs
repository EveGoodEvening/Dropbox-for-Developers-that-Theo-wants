//! fs2-daemon binary entry point.
//!
//! Runs the local daemon with sync loops.

use std::sync::Arc;

use fs2_core::WorkspaceId;
use fs2_daemon::sync::{InboundLoop, OutboundQueue, SyncState};
use fs2_sync::{ApiClient, LocalStore};
use tokio::sync::watch;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "fs2_daemon=debug".into()),
        )
        .init();

    // Load CLI config to get backend URL and token.
    let cfg = fs2_cli::config::CliConfig::load()?;
    let client = ApiClient::new(&cfg.backend_url).with_token(cfg.token);

    // Open local store.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
    let db_path = format!("{home}/.fs2/state.sqlite");
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let store = Arc::new(LocalStore::open(std::path::Path::new(&db_path))?);

    // For now, use a placeholder workspace ID.
    // In a real implementation, this would come from the workspace config.
    let workspace_id = WorkspaceId::from_uuid(uuid::Uuid::nil());

    let state = SyncState::new();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Start sync loops.
    let outbound = OutboundQueue::new(client.clone(), store.clone(), state.clone());
    let inbound = InboundLoop::new(client, store, state.clone());

    let outbound_handle = tokio::spawn(outbound.run(workspace_id, shutdown_rx.clone()));
    let inbound_handle = tokio::spawn(inbound.run(workspace_id, shutdown_rx));

    // Wait for shutdown signal.
    tokio::signal::ctrl_c().await?;
    tracing::info!("shutdown signal received");
    let _ = shutdown_tx.send(true);

    let _ = outbound_handle.await;
    let _ = inbound_handle.await;

    Ok(())
}
