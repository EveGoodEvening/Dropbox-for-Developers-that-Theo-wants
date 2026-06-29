//! fs2 CLI entry point.
//!
//! Commands: login, logout, workspace, status, device.

mod config;
mod doctor;
mod hydrate;

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
    /// Hydrate file content for a path (download blobs into the local cache).
    Hydrate {
        /// Workspace-relative path to hydrate.
        path: String,
        /// Recursively hydrate directories.
        #[arg(long)]
        recursive: bool,
        /// Pin hydrated files so they are not evicted.
        #[arg(long)]
        pin: bool,
    },
    /// Pin a path so its content is not evicted by cache pruning.
    Pin {
        /// Workspace-relative path to pin.
        path: String,
        /// Recursively pin directories.
        #[arg(long)]
        recursive: bool,
    },
    /// Unpin a path, allowing cache pruning to evict its content.
    Unpin {
        /// Workspace-relative path to unpin.
        path: String,
        /// Recursively unpin directories.
        #[arg(long)]
        recursive: bool,
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
    /// Git-aware operations.
    Git {
        #[command(subcommand)]
        action: GitCommands,
    },
    /// Dependency management.
    Deps {
        #[command(subcommand)]
        action: DepsCommands,
    },
    /// Debug and diagnostics.
    Debug {
        #[command(subcommand)]
        action: DebugCommands,
    },
}

#[derive(Debug, Subcommand)]
enum DebugCommands {
    /// Collect diagnostics into a bundle directory.
    Bundle {
        /// Output directory path for the bundle.
        output: String,
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
    /// Import env vars from a .env file.
    Import {
        /// Path to the .env file.
        file: String,
        /// Project path.
        #[arg(long)]
        project: Option<String>,
        /// Environment name.
        #[arg(long)]
        env: String,
        /// Mark all values as secrets.
        #[arg(long)]
        all_secret: bool,
    },
    /// Materialize env vars to a .env file.
    Materialize {
        /// Output file path.
        #[arg(long, default_value = ".env.fs2")]
        output: String,
        /// Project path.
        #[arg(long)]
        project: Option<String>,
        /// Environment name.
        #[arg(long)]
        env: String,
    },
    /// Run a command with env vars injected.
    Exec {
        /// Project path.
        #[arg(long)]
        project: Option<String>,
        /// Environment name.
        #[arg(long)]
        env: String,
        /// Command and arguments after --.
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum GitCommands {
    /// Show Git status for a project.
    Status {
        /// Project path.
        path: String,
    },
    /// Materialize a Git repo from remote metadata.
    Materialize {
        /// Project path.
        path: String,
    },
    /// Show submodule status.
    Submodules {
        /// Project path.
        path: String,
    },
}

#[derive(Debug, Subcommand)]
enum DepsCommands {
    /// Show dependency status for a project.
    Status {
        /// Project path.
        path: String,
    },
    /// Install dependencies for a project.
    Install {
        /// Project path.
        path: String,
        /// Skip confirmation prompt.
        #[arg(long)]
        yes: bool,
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
        Some(Commands::Hydrate { path, recursive, pin }) => {
            let cfg = config::CliConfig::load()?;
            let client = ApiClient::new(&cfg.backend_url).with_token(cfg.token);
            let workspaces = client
                .list_workspaces(cfg.user_id)
                .await
                .map_err(|e| anyhow::anyhow!("failed to list workspaces: {e}"))?;
            if workspaces.is_empty() {
                anyhow::bail!("no workspaces found. Create one with `fs2 workspace create <name>`");
            }
            let ws_id = fs2_core::WorkspaceId::from_uuid(workspaces[0].id);
            let root_node_id = fs2_core::NodeId::from_uuid(workspaces[0].root_node_id);
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
            let db_path = format!("{home}/.fs2/state.sqlite");
            if let Some(parent) = std::path::Path::new(&db_path).parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let store = fs2_sync::LocalStore::open(std::path::Path::new(&db_path))?;
            hydrate::hydrate_path(&client, &store, ws_id, root_node_id, &path, recursive, pin)
                .await?;
        }
        Some(Commands::Pin { path, recursive }) => {
            let cfg = config::CliConfig::load()?;
            let client = ApiClient::new(&cfg.backend_url).with_token(cfg.token);
            let workspaces = client
                .list_workspaces(cfg.user_id)
                .await
                .map_err(|e| anyhow::anyhow!("failed to list workspaces: {e}"))?;
            if workspaces.is_empty() {
                anyhow::bail!("no workspaces found. Create one with `fs2 workspace create <name>`");
            }
            let ws_id = fs2_core::WorkspaceId::from_uuid(workspaces[0].id);
            let root_node_id = fs2_core::NodeId::from_uuid(workspaces[0].root_node_id);
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
            let db_path = format!("{home}/.fs2/state.sqlite");
            if let Some(parent) = std::path::Path::new(&db_path).parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let store = fs2_sync::LocalStore::open(std::path::Path::new(&db_path))?;
            hydrate::set_pin(&store, ws_id, root_node_id, &path, recursive, true)?;
        }
        Some(Commands::Unpin { path, recursive }) => {
            let cfg = config::CliConfig::load()?;
            let client = ApiClient::new(&cfg.backend_url).with_token(cfg.token);
            let workspaces = client
                .list_workspaces(cfg.user_id)
                .await
                .map_err(|e| anyhow::anyhow!("failed to list workspaces: {e}"))?;
            if workspaces.is_empty() {
                anyhow::bail!("no workspaces found. Create one with `fs2 workspace create <name>`");
            }
            let ws_id = fs2_core::WorkspaceId::from_uuid(workspaces[0].id);
            let root_node_id = fs2_core::NodeId::from_uuid(workspaces[0].root_node_id);
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
            let db_path = format!("{home}/.fs2/state.sqlite");
            if let Some(parent) = std::path::Path::new(&db_path).parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let store = fs2_sync::LocalStore::open(std::path::Path::new(&db_path))?;
            hydrate::set_pin(&store, ws_id, root_node_id, &path, recursive, false)?;
        }

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
                EnvCommands::Import {
                    file,
                    project,
                    env,
                    all_secret: _,
                } => {
                    let content = std::fs::read_to_string(&file)
                        .map_err(|e| anyhow::anyhow!("failed to read {file}: {e}"))?;
                    let entries = fs2_env::parse_dotenv(&content)
                        .map_err(|e| anyhow::anyhow!("failed to parse .env: {e}"))?;
                    let mut count = 0;
                    for entry in entries {
                        let encrypted_value = base64_encode(&entry.value);
                        client
                            .set_env_var(
                                ws_id,
                                project.as_deref(),
                                &env,
                                &entry.key,
                                &encrypted_value,
                            )
                            .await
                            .map_err(|e| anyhow::anyhow!("failed to set env var: {e}"))?;
                        count += 1;
                    }
                    println!("Imported {count} env vars from {file} for env {env}");
                }
                EnvCommands::Materialize {
                    output,
                    project,
                    env,
                } => {
                    let vars = client
                        .list_env_vars(ws_id, project.as_deref(), Some(&env))
                        .await
                        .map_err(|e| anyhow::anyhow!("failed to list env vars: {e}"))?;
                    if vars.is_empty() {
                        println!("No env vars found for env {env}.");
                        return Ok(());
                    }
                    // Note: In dev mode, we can't decrypt the values.
                    // In production, this would decrypt with the workspace secret key.
                    // For now, we write a placeholder file.
                    let mut content =
                        String::from("# fs2 env materialization (values encrypted)\n");
                    for v in &vars {
                        use std::fmt::Write;
                        let _ = writeln!(
                            content,
                            "# {} = {} (set, env: {})",
                            v.name, v.value_display, v.environment
                        );
                    }
                    std::fs::write(&output, content)
                        .map_err(|e| anyhow::anyhow!("failed to write {output}: {e}"))?;
                    // Set file permissions to 0600 on Unix.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o600))
                            .ok();
                    }
                    println!("Materialized {} env vars to {output}", vars.len());
                }
                EnvCommands::Exec {
                    project,
                    env,
                    command,
                } => {
                    if command.is_empty() {
                        anyhow::bail!("no command specified. Use: fs2 env exec --project <path> --env <env> -- <command>");
                    }
                    // List env vars (in production, decrypt and inject).
                    let _vars = client
                        .list_env_vars(ws_id, project.as_deref(), Some(&env))
                        .await
                        .map_err(|e| anyhow::anyhow!("failed to list env vars: {e}"))?;
                    // In dev mode, we can't decrypt. Run the command as-is.
                    // In production, this would set env vars from decrypted values.
                    let status = std::process::Command::new(&command[0])
                        .args(&command[1..])
                        .status()
                        .map_err(|e| anyhow::anyhow!("failed to run command: {e}"))?;
                    std::process::exit(status.code().unwrap_or(1));
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
        Some(Commands::Git { action }) => match action {
            GitCommands::Status { path } => {
                let meta = fs2_git::detect_git(std::path::Path::new(&path))
                    .map_err(|e| anyhow::anyhow!("failed to detect git: {e}"))?;
                if meta.is_repo {
                    println!("{path}: git repository");
                    if let Some(ref remote) = meta.remote_url {
                        println!("  remote: {remote}");
                    }
                    if let Some(ref branch) = meta.branch {
                        println!("  branch: {branch}");
                    }
                    if let Some(ref head) = meta.head_commit {
                        println!("  HEAD: {head}");
                    }
                    println!("  dirty: {}", if meta.dirty { "yes" } else { "no" });
                    if !meta.submodules.is_empty() {
                        println!("  submodules:");
                        for sub in &meta.submodules {
                            println!("    {} ({})", sub.path, sub.url);
                        }
                    }
                } else {
                    println!("{path}: not a git repository");
                }
            }
            GitCommands::Materialize { path } => {
                let meta = fs2_git::detect_git(std::path::Path::new(&path))
                    .map_err(|e| anyhow::anyhow!("failed to detect git: {e}"))?;
                if meta.is_repo {
                    println!("{path}: already a git repository");
                    return Ok(());
                }
                // In a real implementation, this would read remote metadata
                // from the sync state and run git clone. For now, we report
                // that materialization requires a known remote URL.
                println!("{path}: git materialization requires a known remote URL.");
                println!("  Use `git clone <remote> {path}` to set up the repository.");
            }
            GitCommands::Submodules { path } => {
                let subs = fs2_git::detect_gitmodules(std::path::Path::new(&path))
                    .map_err(|e| anyhow::anyhow!("failed to detect submodules: {e}"))?;
                if subs.is_empty() {
                    println!("{path}: no submodules");
                } else {
                    println!("{path}: {} submodule(s)", subs.len());
                    for sub in &subs {
                        println!("  {} ({})", sub.path, sub.url);
                    }
                }
            }
        },
        Some(Commands::Deps { action }) => match action {
            DepsCommands::Status { path } => {
                let pm = fs2_git::detect_package_manager(std::path::Path::new(&path));
                if pm == fs2_git::PackageManager::None {
                    println!("{path}: no package manager detected");
                } else {
                    println!("{path}: package manager = {}", pm.display_name());
                    let cmd = pm.install_command();
                    if !cmd.is_empty() {
                        println!("  install: {}", cmd.join(" "));
                    }
                }
            }
            DepsCommands::Install { path, yes } => {
                let pm = fs2_git::detect_package_manager(std::path::Path::new(&path));
                if pm == fs2_git::PackageManager::None {
                    anyhow::bail!("{path}: no package manager detected");
                }
                let cmd = pm.install_command();
                if cmd.is_empty() {
                    anyhow::bail!("no install command for {}", pm.display_name());
                }
                if !yes {
                    println!("About to run: {} in {path}", cmd.join(" "));
                    print!("Proceed? [y/N] ");
                    {
                        use std::io::Write;
                        std::io::stdout().flush()?;
                    }
                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    if !input.trim().eq_ignore_ascii_case("y") {
                        println!("Aborted.");
                        return Ok(());
                    }
                }
                let status = std::process::Command::new(cmd[0])
                    .args(&cmd[1..])
                    .current_dir(&path)
                    .status()
                    .map_err(|e| anyhow::anyhow!("failed to run install: {e}"))?;
                if status.success() {
                    println!("Dependencies installed successfully.");
                } else {
                    anyhow::bail!("install failed with exit code: {:?}", status.code());
                }
            }
        },
        Some(Commands::Debug { action }) => match action {
            DebugCommands::Bundle { output } => {
                let bundle_dir = std::path::PathBuf::from(&output);
                std::fs::create_dir_all(&bundle_dir)
                    .map_err(|e| anyhow::anyhow!("failed to create bundle dir {output}: {e}"))?;

                let home = std::env::var("HOME")
                    .map(std::path::PathBuf::from)
                    .map_err(|_| {
                        anyhow::anyhow!("cannot determine home directory (HOME not set)")
                    })?;
                let fs2_dir = home.join(".fs2");

                // 1. Redacted config.json
                let config_path = fs2_dir.join("config.json");
                if config_path.exists() {
                    let raw = std::fs::read_to_string(&config_path)
                        .map_err(|e| anyhow::anyhow!("failed to read config: {e}"))?;
                    let redacted = redact_secrets(&raw);
                    std::fs::write(bundle_dir.join("config.json"), redacted)
                        .map_err(|e| anyhow::anyhow!("failed to write redacted config: {e}"))?;
                }

                // 2. Logs directory (copied recursively if it exists)
                let logs_src = fs2_dir.join("logs");
                if logs_src.is_dir() {
                    let logs_dst = bundle_dir.join("logs");
                    copy_dir_recursive(&logs_src, &logs_dst)?;
                }

                // 3. Status JSON output
                let status_json = build_status_json().await;
                std::fs::write(bundle_dir.join("status.json"), status_json)
                    .map_err(|e| anyhow::anyhow!("failed to write status.json: {e}"))?;

                println!("Debug bundle written to {output}");
            }
        },
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

/// Redact secret values in a config JSON string.
///
/// Replaces values associated with sensitive keys (`token`, `secret`,
/// `private_key`, `privateKey`, `password`) with `REDACTED`. Works on the
/// raw text so it catches both pretty-printed and compact JSON.
fn redact_secrets(raw: &str) -> String {
    // Parse as JSON and walk the structure so we only redact values of
    // sensitive keys, leaving everything else intact.
    let parsed: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => {
            // Not valid JSON; fall back to a conservative regex-style redaction.
            return redact_raw_text(raw);
        }
    };
    let mut redacted = parsed;
    redact_json_value(&mut redacted);
    serde_json::to_string_pretty(&redacted).unwrap_or_else(|_| raw.to_owned())
}

/// Recursively redact sensitive values in a JSON value.
fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if is_sensitive_key(key) {
                    *val = serde_json::Value::String("REDACTED".to_owned());
                } else {
                    redact_json_value(val);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                redact_json_value(item);
            }
        }
        _ => {}
    }
}

/// Whether a key name refers to a sensitive value.
fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "token"
            | "secret"
            | "secrets"
            | "private_key"
            | "privatekey"
            | "private_keys"
            | "password"
            | "passwd"
            | "api_key"
            | "apikey"
            | "access_token"
            | "refresh_token"
    )
}

/// Fallback text redaction for non-JSON config files.
fn redact_raw_text(raw: &str) -> String {
    // Redact `"<sensitive-key>": "<value>"` patterns.
    let sensitive = [
        "token",
        "secret",
        "secrets",
        "private_key",
        "privateKey",
        "private_keys",
        "password",
        "passwd",
        "api_key",
        "apiKey",
        "access_token",
        "refresh_token",
    ];
    let mut out = raw.to_owned();
    for key in sensitive {
        // Match "key": "value"  (JSON-style)
        let pattern_json = format!("\"{key}\":");
        if let Some(pos) = out.find(&pattern_json) {
            let after = &out[pos + pattern_json.len()..];
            if let Some(start) = after.find('"') {
                let value_start = pos + pattern_json.len() + start + 1;
                if let Some(end_rel) = out[value_start..].find('"') {
                    let value_end = value_start + end_rel;
                    out.replace_range(value_start..value_end, "REDACTED");
                }
            }
        }
        // Match key=value (dotenv-style)
        let pattern_dotenv = format!("{key}=");
        if let Some(pos) = out.find(&pattern_dotenv) {
            let value_start = pos + pattern_dotenv.len();
            let rest = &out[value_start..];
            let value_end = rest.find('\n').map_or(out.len(), |e| value_start + e);
            out.replace_range(value_start..value_end, "REDACTED\n");
        }
    }
    out
}

/// Recursively copy a directory tree.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)
        .map_err(|e| anyhow::anyhow!("failed to create {}: {e}", dst.display()))?;
    for entry in std::fs::read_dir(src)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", src.display()))?
    {
        let entry = entry.map_err(|e| anyhow::anyhow!("dir entry error: {e}"))?;
        let path = entry.path();
        let name = entry.file_name();
        let dst_child = dst.join(&name);
        if path.is_dir() {
            copy_dir_recursive(&path, &dst_child)?;
        } else if path.is_file() {
            // Redact log file contents too, in case tokens leak into logs.
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            let redacted = redact_raw_text(&content);
            std::fs::write(&dst_child, redacted)
                .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", dst_child.display()))?;
        }
    }
    Ok(())
}

/// Build a status JSON string for the debug bundle.
async fn build_status_json() -> String {
    // Attempt to load config; if missing, report not_logged_in.
    let cfg = config::CliConfig::load();
    let status = match cfg {
        Ok(c) => {
            let client = ApiClient::new(&c.backend_url).with_token(c.token.clone());
            let healthy = client.health().await.unwrap_or(false);
            serde_json::json!({
                "status": if healthy { "online" } else { "offline" },
                "backend": c.backend_url,
                "user_id": c.user_id,
                "device_id": c.device_id,
            })
        }
        Err(_) => {
            serde_json::json!({
                "status": "not_logged_in",
            })
        }
    };
    serde_json::to_string_pretty(&status).unwrap_or_else(|_| "{}".to_owned())
}
