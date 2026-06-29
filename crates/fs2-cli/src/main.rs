//! fs2 CLI entry point.
//!
//! Commands: login, logout, workspace, status, device.

mod config;
mod doctor;

use clap::{Parser, Subcommand};
use std::io::{self, Write};

use fs2_sync::ApiClient;

#[derive(Debug, Parser)]
#[command(
    name = "fs2",
    version,
    about = "Developer-focused cross-machine code sync",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Log in to a backend.
    Login {
        /// Backend URL.
        #[arg(long)]
        backend: String,
        /// Email for dev login.
        #[arg(long)]
        email: Option<String>,
    },
    /// Log out and clear local tokens.
    Logout,
    /// Device management.
    Device {
        #[command(subcommand)]
        action: DeviceCommands,
    },
    /// Workspace management.
    Workspace {
        #[command(subcommand)]
        action: WorkspaceCommands,
    },
    /// Show sync and workspace status.
    Status {
        /// Output JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run diagnostics checks.
    Doctor {
        /// Output JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum DeviceCommands {
    /// List registered devices.
    List,
}

#[derive(Debug, Subcommand)]
enum WorkspaceCommands {
    /// Create a new workspace.
    Create {
        /// Workspace name.
        name: String,
    },
    /// List workspaces.
    List,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Login { backend, email }) => {
            let client = ApiClient::new(&backend);
            // Health check.
            if !client.health().await? {
                anyhow::bail!("backend at {backend} is not healthy");
            }
            let email = email.unwrap_or_else(|| "dev@example.com".to_owned());
            let device_name = hostname::get().map_or_else(
                |_| "unknown".to_owned(),
                |h| h.to_string_lossy().to_string(),
            );
            let login = client
                .dev_login(&email, &device_name, "dev-fake-key")
                .await?;
            // Save config.
            let cfg = config::CliConfig {
                backend_url: backend,
                token: login.token,
                user_id: login.user_id,
                device_id: login.device_id,
            };
            cfg.save()?;
            println!("Logged in as {email} on device {device_name}");
            println!("Device ID: {}", login.device_id);
        }
        Some(Commands::Logout) => {
            config::CliConfig::clear()?;
            println!("Logged out.");
        }
        Some(Commands::Device { action }) => match action {
            DeviceCommands::List => {
                let cfg = config::CliConfig::load()?;
                let client = ApiClient::new(&cfg.backend_url).with_token(cfg.token.clone());
                let resp = client
                    .list_devices(cfg.user_id)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to list devices: {e}"))?;
                if resp.is_empty() {
                    println!("No devices registered.");
                } else {
                    println!("{:<36}  {:<20}  REVOKED", "ID", "NAME");
                    for d in resp {
                        println!("{:<36}  {:<20}  {}", d.id, d.name, d.revoked);
                    }
                }
            }
        },
        Some(Commands::Workspace { action }) => match action {
            WorkspaceCommands::Create { name } => {
                let cfg = config::CliConfig::load()?;
                let client = ApiClient::new(&cfg.backend_url).with_token(cfg.token.clone());
                let ws = client
                    .create_workspace(cfg.user_id, cfg.device_id, &name)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to create workspace: {e}"))?;
                println!("Created workspace: {} (id: {})", ws.name, ws.id);
                println!("Root node: {}", ws.root_node_id);
            }
            WorkspaceCommands::List => {
                let cfg = config::CliConfig::load()?;
                let client = ApiClient::new(&cfg.backend_url).with_token(cfg.token.clone());
                let workspaces = client
                    .list_workspaces(cfg.user_id)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to list workspaces: {e}"))?;
                if workspaces.is_empty() {
                    println!("No workspaces.");
                } else {
                    println!("{:<36}  {:<20}  CURSOR", "ID", "NAME");
                    for w in workspaces {
                        println!("{:<36}  {:<20}  {}", w.id, w.name, w.cursor);
                    }
                }
            }
        },
        Some(Commands::Status { json }) => {
            let cfg = config::CliConfig::load();
            if cfg.is_err() {
                if json {
                    println!(r#"{{"status":"not_logged_in"}}"#);
                } else {
                    println!("fs2: not logged in. Run `fs2 login --backend <url>` first.");
                }
                return Ok(());
            }
            let cfg = cfg?;
            let client = ApiClient::new(&cfg.backend_url).with_token(cfg.token);
            let healthy = client.health().await.unwrap_or(false);
            if json {
                let status = serde_json::json!({
                    "status": if healthy { "online" } else { "offline" },
                    "backend": cfg.backend_url,
                    "user_id": cfg.user_id,
                    "device_id": cfg.device_id,
                });
                let _ = io::stdout().flush();
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!(
                    "Backend: {} ({})",
                    cfg.backend_url,
                    if healthy { "online" } else { "offline" }
                );
                println!("User: {}", cfg.user_id);
                println!("Device: {}", cfg.device_id);
            }
        }
        Some(Commands::Doctor { json }) => {
            let results = doctor::run_checks();
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                let all_passed = results.iter().all(|r| r.passed);
                for r in &results {
                    let status = if r.passed { "PASS" } else { "FAIL" };
                    println!("{status} {}: {}", r.name, r.message);
                    if let Some(ref sugg) = r.suggestion {
                        println!("  -> {sugg}");
                    }
                }
                if all_passed {
                    println!("\nAll checks passed.");
                } else {
                    println!("\nSome checks failed. See suggestions above.");
                }
            }
        }
        None => {
            println!("fs2: see `fs2 --help`");
        }
    }
    Ok(())
}
