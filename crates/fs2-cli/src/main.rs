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
    /// Environment variable management.
    Env {
        #[command(subcommand)]
        action: EnvCommands,
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

#[derive(Debug, Subcommand)]
enum EnvCommands {
    /// Set an environment variable.
    Set {
        /// Variable name.
        name: String,
        /// Variable value.
        value: String,
        /// Project path.
        #[arg(long)]
        project: Option<String>,
        /// Environment name (dev, test, prod).
        #[arg(long)]
        env: String,
        /// Mark as secret (encrypted, redacted).
        #[arg(long)]
        secret: bool,
    },
    /// List environment variables.
    List {
        /// Project path.
        #[arg(long)]
        project: Option<String>,
        /// Environment name.
        #[arg(long)]
        env: Option<String>,
    },
    /// Unset (delete) an environment variable.
    Unset {
        /// Variable name.
        name: String,
        /// Project path.
        #[arg(long)]
        project: Option<String>,
        /// Environment name.
        #[arg(long)]
        env: String,
    },
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
        Some(Commands::Env { action }) => {
            let cfg = config::CliConfig::load()?;
            let client = ApiClient::new(&cfg.backend_url).with_token(cfg.token.clone());
            // For now, use the first workspace from the list.
            let workspaces = client
                .list_workspaces(cfg.user_id)
                .await
                .map_err(|e| anyhow::anyhow!("failed to list workspaces: {e}"))?;
            if workspaces.is_empty() {
                anyhow::bail!("no workspaces found. Create one with `fs2 workspace create <name>`");
            }
            let ws_id = fs2_core::WorkspaceId::from_uuid(workspaces[0].id);
            match action {
                EnvCommands::Set {
                    name,
                    value,
                    project,
                    env,
                    secret: _,
                } => {
                    // For dev mode, we store the value as-is (not encrypted).
                    // In production, this would be encrypted with the workspace secret key.
                    let encrypted_value = base64_encode(&value);
                    client
                        .set_env_var(ws_id, project.as_deref(), &env, &name, &encrypted_value)
                        .await
                        .map_err(|e| anyhow::anyhow!("failed to set env var: {e}"))?;
                    println!("Set {name} for env {env}");
                }
                EnvCommands::List { project, env } => {
                    let vars = client
                        .list_env_vars(ws_id, project.as_deref(), env.as_deref())
                        .await
                        .map_err(|e| anyhow::anyhow!("failed to list env vars: {e}"))?;
                    if vars.is_empty() {
                        println!("No env vars found.");
                    } else {
                        println!("{:<30}  {:<10}  {:<20}  VALUE", "NAME", "ENV", "PROJECT");
                        for v in vars {
                            println!(
                                "{:<30}  {:<10}  {:<20}  {}",
                                v.name,
                                v.environment,
                                v.project_path.unwrap_or_default(),
                                v.value_display
                            );
                        }
                    }
                }
                EnvCommands::Unset { name, project, env } => {
                    // List to find the env var id.
                    let vars = client
                        .list_env_vars(ws_id, project.as_deref(), Some(&env))
                        .await
                        .map_err(|e| anyhow::anyhow!("failed to list env vars: {e}"))?;
                    let var = vars
                        .iter()
                        .find(|v| v.name == name)
                        .ok_or_else(|| anyhow::anyhow!("env var {name} not found"))?;
                    client
                        .delete_env_var(ws_id, var.id)
                        .await
                        .map_err(|e| anyhow::anyhow!("failed to delete env var: {e}"))?;
                    println!("Unset {name} for env {env}");
                }
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

/// Simple base64 encoding for dev mode (no encryption).
fn base64_encode(data: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data.as_bytes())
}
