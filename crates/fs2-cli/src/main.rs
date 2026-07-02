//! FS2 command-line entry point.

use fs2_core::{try_normalized_name, CasePolicy, NodeId, RuleAction, WorkspaceId, WorkspacePath};
use fs2_crypto::WorkspaceKeyStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env,
    fmt::Write as _,
    fs,
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

fn main() {
    match run_from_args(env::args().skip(1)) {
        Ok(output) => print!("{output}"),
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}

const CLI_CONFIG_ENV: &str = "FS2_CONFIG_HOME";
const CLI_KEYCHAIN_SERVICE: &str = "fs2-devsync.cli-token.v1";
const CLI_TOKEN_ACCOUNT: &str = "default";
const CLI_REFRESH_TOKEN_ACCOUNT: &str = "default-refresh";

fn run_from_args(args: impl IntoIterator<Item = String>) -> Result<String, String> {
    let args = args.into_iter().collect::<Vec<_>>();
    let credentials = SystemCredentialStore;
    let needs_config = matches!(
        args.as_slice(),
        [cmd, ..]
            if cmd == "login"
                || cmd == "logout"
                || cmd == "device"
                || cmd == "workspace"
                || cmd == "mount"
                || cmd == "status"
                || cmd == "doctor"
                || cmd == "debug"
    );
    let config_path = if needs_config {
        default_cli_config_path()?
    } else {
        PathBuf::new()
    };
    run_from_args_with_context(args, &credentials, &config_path)
}

fn run_from_args_with_context(
    args: impl IntoIterator<Item = String>,
    credentials: &dyn CredentialStore,
    config_path: &Path,
) -> Result<String, String> {
    let args = args.into_iter().collect::<Vec<_>>();
    match args.as_slice() {
        [] => Ok(help()),
        [one] if one == "--help" || one == "-h" => Ok(help()),
        [cmd, flag, backend] if cmd == "login" && flag == "--backend" => dev_login(
            backend,
            "fs2-dev-cli",
            "dev-cli-public-key",
            credentials,
            config_path,
        ),
        [cmd, flag, backend, name_flag, device_name]
            if cmd == "login" && flag == "--backend" && name_flag == "--device-name" =>
        {
            dev_login(
                backend,
                device_name,
                "dev-cli-public-key",
                credentials,
                config_path,
            )
        }
        [cmd] if cmd == "logout" => logout(credentials, config_path),
        [cmd, subcmd] if cmd == "device" && subcmd == "list" => {
            device_list(credentials, config_path)
        }
        [cmd, subcmd, name] if cmd == "workspace" && subcmd == "create" => {
            workspace_create(name, None, credentials, config_path)
        }
        [cmd, subcmd] if cmd == "workspace" && subcmd == "list" => {
            workspace_list(credentials, config_path)
        }
        [cmd, subcmd, path, name_flag, name]
            if cmd == "workspace" && subcmd == "init" && name_flag == "--name" =>
        {
            workspace_create(name, Some(Path::new(path)), credentials, config_path)
        }
        [cmd, workspace, path] if cmd == "mount" => {
            workspace_mount(workspace, Path::new(path), config_path)
        }
        [cmd] if cmd == "status" => status_text_with_config(".", config_path),
        [cmd, flag] if cmd == "status" && flag == "--json" => {
            status_json_with_config(".", config_path)
        }
        [cmd, flag, path] if cmd == "status" && flag == "--path" => {
            status_text_with_config(path, config_path)
        }
        [cmd, flag, path, json_flag]
            if cmd == "status" && flag == "--path" && json_flag == "--json" =>
        {
            status_json_with_config(path, config_path)
        }
        [cmd, subcmd] if cmd == "debug" && subcmd == "bundle" => debug_bundle(
            Path::new("."),
            Path::new("fs2-debug-bundle.tar.gz"),
            config_path,
        ),
        [cmd, subcmd, out_flag, out_path]
            if cmd == "debug" && subcmd == "bundle" && out_flag == "--out" =>
        {
            debug_bundle(Path::new("."), Path::new(out_path), config_path)
        }
        [cmd, subcmd, path] if cmd == "debug" && subcmd == "bundle" => debug_bundle(
            Path::new(path),
            Path::new("fs2-debug-bundle.tar.gz"),
            config_path,
        ),
        [cmd, subcmd, path, out_flag, out_path]
            if cmd == "debug" && subcmd == "bundle" && out_flag == "--out" =>
        {
            debug_bundle(Path::new(path), Path::new(out_path), config_path)
        }
        [cmd] if cmd == "doctor" => doctor_with_context(".", credentials, config_path),
        [cmd, subcmd, path] if cmd == "deps" && subcmd == "status" => deps_status(path),
        [cmd, subcmd, path] if cmd == "deps" && subcmd == "install" => deps_install(path, false),
        [cmd, subcmd, path, yes] if cmd == "deps" && subcmd == "install" && yes == "--yes" => {
            deps_install(path, true)
        }
        [cmd, path] if cmd == "doctor" => doctor_with_context(path, credentials, config_path),
        [cmd, subcmd, action] if cmd == "git" && subcmd == "submodules" && action == "status" => {
            git_submodules_status(".")
        }
        [cmd, subcmd, action, path]
            if cmd == "git" && subcmd == "submodules" && action == "status" =>
        {
            git_submodules_status(path)
        }
        [cmd, subcmd] if cmd == "git" && subcmd == "status" => git_status("."),
        [cmd, subcmd, path] if cmd == "git" && subcmd == "status" => git_status(path),
        _ => Err("unsupported command; try `fs2 --help`".to_owned()),
    }
}

fn help() -> String {
    "fs2-devsync CLI\n\nCommands:\n  fs2 login --backend <url> [--device-name <name>]\n  fs2 logout\n  fs2 device list\n  fs2 workspace create <name>\n  fs2 workspace list\n  fs2 workspace init <path> --name <name>\n  fs2 mount <workspace> <path>\n  fs2 status [--json]\n  fs2 doctor [path]\n  fs2 debug bundle [path] [--out <archive.tar.gz>]\n  fs2 deps status <path>\n  fs2 deps install <path> [--yes]\n  fs2 git status [path]\n  fs2 git submodules status [path]\n"
        .to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CliConfig {
    backend_url: String,
    user_id: String,
    device_id: String,
    device_name: String,
    token_type: String,
}

#[derive(Debug, Deserialize)]
struct DevLoginResponse {
    access_token: String,
    refresh_token: String,
    token_type: String,
    user_id: String,
    device_id: String,
    warning: String,
}

#[derive(Debug, Deserialize)]
struct DeviceListResponse {
    devices: Vec<CliDeviceRecord>,
}

#[derive(Debug, Deserialize)]
struct CliDeviceRecord {
    device_id: String,
    name: String,
    #[serde(default)]
    revoked: bool,
}

#[derive(Debug, Deserialize)]
struct CreateWorkspaceResponse {
    workspace_id: String,
    root_node_id: String,
    current_cursor: i64,
}

#[derive(Debug, Deserialize)]
struct WorkspaceListResponse {
    workspaces: Vec<CliWorkspaceSummary>,
}

#[derive(Debug, Deserialize)]
struct CliWorkspaceSummary {
    workspace_id: String,
    name: String,
    root_node_id: String,
    current_cursor: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LocalWorkspaceConfig {
    workspace_id: String,
    name: String,
    root_node_id: String,
    metadata_db: String,
    path: Option<String>,
    mount_path: Option<String>,
}

trait CredentialStore {
    fn set_access_token(&self, token: &str) -> Result<(), String>;
    fn set_refresh_token(&self, token: &str) -> Result<(), String>;
    fn get_access_token(&self) -> Result<Option<String>, String>;
    fn delete_tokens(&self) -> Result<(), String>;
}

#[derive(Debug)]
struct SystemCredentialStore;

impl CredentialStore for SystemCredentialStore {
    fn set_access_token(&self, token: &str) -> Result<(), String> {
        set_keychain_token(CLI_TOKEN_ACCOUNT, token, "access")
    }

    fn set_refresh_token(&self, token: &str) -> Result<(), String> {
        set_keychain_token(CLI_REFRESH_TOKEN_ACCOUNT, token, "refresh")
    }

    fn get_access_token(&self) -> Result<Option<String>, String> {
        get_keychain_token(CLI_TOKEN_ACCOUNT, "access")
    }

    fn delete_tokens(&self) -> Result<(), String> {
        delete_keychain_token(CLI_TOKEN_ACCOUNT, "access")?;
        delete_keychain_token(CLI_REFRESH_TOKEN_ACCOUNT, "refresh")
    }
}

fn set_keychain_token(account: &str, token: &str, label: &str) -> Result<(), String> {
    keyring::Entry::new(CLI_KEYCHAIN_SERVICE, account)
        .map_err(|error| format!("could not open OS keychain: {error}"))?
        .set_password(token)
        .map_err(|error| format!("could not store {label} token in OS keychain: {error}"))
}

fn get_keychain_token(account: &str, label: &str) -> Result<Option<String>, String> {
    match keyring::Entry::new(CLI_KEYCHAIN_SERVICE, account)
        .map_err(|error| format!("could not open OS keychain: {error}"))?
        .get_password()
    {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(format!(
            "could not read {label} token from OS keychain: {error}"
        )),
    }
}

fn delete_keychain_token(account: &str, label: &str) -> Result<(), String> {
    match keyring::Entry::new(CLI_KEYCHAIN_SERVICE, account)
        .map_err(|error| format!("could not open OS keychain: {error}"))?
        .delete_credential()
    {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(format!(
            "could not delete {label} token from OS keychain: {error}"
        )),
    }
}

fn dev_login(
    backend: &str,
    device_name: &str,
    public_key: &str,
    credentials: &dyn CredentialStore,
    config_path: &Path,
) -> Result<String, String> {
    let (host, port, path) = parse_http_url(backend, "/v1/auth/dev-login")?;
    let body = serde_json::json!({
        "device_name": device_name,
        "platform": {"os": std::env::consts::OS, "arch": std::env::consts::ARCH},
        "public_key": public_key,
    })
    .to_string();
    let response = request_json("POST", &host, port, &path, Some(&body), None)?;
    let login = serde_json::from_str::<DevLoginResponse>(&response)
        .map_err(|error| format!("login response was not valid JSON: {error}"))?;
    let config = CliConfig {
        backend_url: backend.to_owned(),
        user_id: login.user_id.clone(),
        device_id: login.device_id.clone(),
        device_name: device_name.to_owned(),
        token_type: login.token_type.clone(),
    };
    save_cli_config(config_path, &config)?;
    if let Err(error) = credentials.set_access_token(&login.access_token) {
        let _ = credentials.delete_tokens();
        let _ = fs::remove_file(config_path);
        return Err(error);
    }
    if let Err(error) = credentials.set_refresh_token(&login.refresh_token) {
        let _ = credentials.delete_tokens();
        let _ = fs::remove_file(config_path);
        return Err(error);
    }
    let mut out = String::new();
    writeln!(out, "Logged in to {backend}").map_err(|error| error.to_string())?;
    writeln!(out, "  user: {}", login.user_id).map_err(|error| error.to_string())?;
    writeln!(out, "  device: {}", login.device_id).map_err(|error| error.to_string())?;
    writeln!(out, "  token type: {}", login.token_type).map_err(|error| error.to_string())?;
    writeln!(
        out,
        "  access token: <redacted:{} bytes>",
        login.access_token.len()
    )
    .map_err(|error| error.to_string())?;
    writeln!(out, "  warning: {}", login.warning).map_err(|error| error.to_string())?;
    Ok(out)
}

fn logout(credentials: &dyn CredentialStore, config_path: &Path) -> Result<String, String> {
    credentials.delete_tokens()?;
    if config_path.exists() {
        fs::remove_file(config_path)
            .map_err(|error| format!("could not remove {}: {error}", config_path.display()))?;
    }
    Ok("Logged out\n".to_owned())
}

fn device_list(credentials: &dyn CredentialStore, config_path: &Path) -> Result<String, String> {
    let config = load_cli_config(config_path)?;
    let token = credentials
        .get_access_token()?
        .ok_or("not logged in; run `fs2 login --backend <url>`")?;
    let (host, port, path) = parse_http_url(&config.backend_url, "/v1/devices")?;
    let response = request_json("GET", &host, port, &path, None, Some(&token))?;
    let devices = serde_json::from_str::<DeviceListResponse>(&response)
        .map_err(|error| format!("device list response was not valid JSON: {error}"))?;
    let mut out = String::new();
    writeln!(out, "Devices:").map_err(|error| error.to_string())?;
    for device in devices.devices {
        let marker = if device.device_id == config.device_id {
            " (current)"
        } else {
            ""
        };
        let revoked = if device.revoked { " revoked" } else { "" };
        writeln!(
            out,
            "  - {} {}{}{}",
            device.device_id, device.name, marker, revoked
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(out)
}

fn workspace_create(
    name: &str,
    init_path: Option<&Path>,
    credentials: &dyn CredentialStore,
    config_path: &Path,
) -> Result<String, String> {
    let config = load_cli_config(config_path)?;
    let token = credentials
        .get_access_token()?
        .ok_or("not logged in; run `fs2 login --backend <url>`")?;
    let (host, port, path) = parse_http_url(&config.backend_url, "/v1/workspaces")?;
    let body = serde_json::json!({"name": name}).to_string();
    let response = request_json("POST", &host, port, &path, Some(&body), Some(&token))?;
    let created = serde_json::from_str::<CreateWorkspaceResponse>(&response)
        .map_err(|error| format!("workspace create response was not valid JSON: {error}"))?;
    let workspace = local_workspace_from_create(config_path, name, init_path, &created)?;
    initialize_local_workspace(&workspace)?;
    if let Some(path) = init_path {
        write_workspace_marker(path, name)?;
    }
    upsert_local_workspace(config_path, workspace.clone())?;
    let mut out = String::new();
    writeln!(out, "Workspace created: {name}").map_err(|error| error.to_string())?;
    writeln!(out, "  id: {}", workspace.workspace_id).map_err(|error| error.to_string())?;
    writeln!(out, "  root: {}", workspace.root_node_id).map_err(|error| error.to_string())?;
    writeln!(out, "  cursor: {}", created.current_cursor).map_err(|error| error.to_string())?;
    writeln!(out, "  metadata: {}", workspace.metadata_db).map_err(|error| error.to_string())?;
    if let Some(path) = workspace.path {
        writeln!(out, "  path: {path}").map_err(|error| error.to_string())?;
    }
    Ok(out)
}

fn workspace_list(credentials: &dyn CredentialStore, config_path: &Path) -> Result<String, String> {
    let config = load_cli_config(config_path)?;
    let token = credentials
        .get_access_token()?
        .ok_or("not logged in; run `fs2 login --backend <url>`")?;
    let (host, port, path) = parse_http_url(&config.backend_url, "/v1/workspaces")?;
    let response = request_json("GET", &host, port, &path, None, Some(&token))?;
    let remote = serde_json::from_str::<WorkspaceListResponse>(&response)
        .map_err(|error| format!("workspace list response was not valid JSON: {error}"))?;
    let locals = load_local_workspaces(config_path)?;
    let mut out = String::new();
    writeln!(out, "Workspaces:").map_err(|error| error.to_string())?;
    for workspace in remote.workspaces {
        let local = locals
            .iter()
            .find(|local| local.workspace_id == workspace.workspace_id);
        let local_marker = if local.is_some() { " local" } else { "" };
        writeln!(
            out,
            "  - {} {} root={} cursor={}{}",
            workspace.workspace_id,
            workspace.name,
            workspace.root_node_id,
            workspace.current_cursor,
            local_marker
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(out)
}

fn workspace_mount(
    workspace: &str,
    mount_path: &Path,
    config_path: &Path,
) -> Result<String, String> {
    let mut workspaces = load_local_workspaces(config_path)?;
    let index = resolve_local_workspace_index(&workspaces, workspace)?;
    let local = &mut workspaces[index];
    fs::create_dir_all(mount_path).map_err(|error| {
        format!(
            "could not create mount path {}: {error}",
            mount_path.display()
        )
    })?;
    local.mount_path = Some(mount_path.display().to_string());
    save_local_workspaces(config_path, &workspaces)?;
    Ok(format!(
        "Mount placeholder recorded for {} at {}\n  FUSE mounting is implemented in a later phase.\n",
        workspace,
        mount_path.display()
    ))
}

fn resolve_local_workspace_index(
    workspaces: &[LocalWorkspaceConfig],
    workspace: &str,
) -> Result<usize, String> {
    if let Some((index, _)) = workspaces
        .iter()
        .enumerate()
        .find(|(_, local)| local.workspace_id == workspace)
    {
        return Ok(index);
    }
    let mut name_matches = workspaces
        .iter()
        .enumerate()
        .filter(|(_, local)| local.name == workspace)
        .map(|(index, _)| index);
    let Some(index) = name_matches.next() else {
        return Err(format!("workspace not initialized locally: {workspace}"));
    };
    if name_matches.next().is_some() {
        return Err(format!(
            "workspace name is ambiguous; use a workspace id instead: {workspace}"
        ));
    }
    Ok(index)
}

fn local_workspace_from_create(
    config_path: &Path,
    name: &str,
    init_path: Option<&Path>,
    created: &CreateWorkspaceResponse,
) -> Result<LocalWorkspaceConfig, String> {
    let metadata_db = workspace_metadata_db_path(config_path, &created.workspace_id)?;
    Ok(LocalWorkspaceConfig {
        workspace_id: created.workspace_id.clone(),
        name: name.to_owned(),
        root_node_id: created.root_node_id.clone(),
        metadata_db: metadata_db.display().to_string(),
        path: init_path.map(|path| path.display().to_string()),
        mount_path: None,
    })
}

fn initialize_local_workspace(workspace: &LocalWorkspaceConfig) -> Result<(), String> {
    let workspace_id = workspace
        .workspace_id
        .parse::<WorkspaceId>()
        .map_err(|error| error.to_string())?;
    let root_node_id = workspace
        .root_node_id
        .parse::<NodeId>()
        .map_err(|error| error.to_string())?;
    let mut store = fs2_daemon::LocalStore::open(&workspace.metadata_db)
        .map_err(|error| format!("could not initialize local metadata DB: {error}"))?;
    store
        .initialize_workspace(workspace_id, &workspace.name, root_node_id)
        .map_err(|error| format!("could not initialize local workspace: {error}"))
}

fn write_workspace_marker(path: &Path, name: &str) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("could not create {}: {error}", path.display()))?;
    let fs2_dir = path.join(".fs2");
    fs::create_dir_all(&fs2_dir)
        .map_err(|error| format!("could not create {}: {error}", fs2_dir.display()))?;
    let config_path = fs2_dir.join("config.toml");
    if !config_path.exists() {
        let workspace_name = serde_json::to_string(name).map_err(|error| error.to_string())?;
        fs::write(
            &config_path,
            format!("version = 1\nworkspace_name = {workspace_name}\n"),
        )
        .map_err(|error| format!("could not write {}: {error}", config_path.display()))?;
    }
    Ok(())
}

fn workspace_metadata_db_path(config_path: &Path, workspace_id: &str) -> Result<PathBuf, String> {
    let base = config_path
        .parent()
        .ok_or("config path must have a parent directory")?;
    Ok(base
        .join("workspaces")
        .join(workspace_id)
        .join("metadata.sqlite"))
}

fn workspace_config_path(config_path: &Path) -> Result<PathBuf, String> {
    let base = config_path
        .parent()
        .ok_or("config path must have a parent directory")?;
    Ok(base.join("workspaces.json"))
}

fn load_local_workspaces(config_path: &Path) -> Result<Vec<LocalWorkspaceConfig>, String> {
    let path = workspace_config_path(config_path)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes =
        fs::read(&path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("invalid {}: {error}", path.display()))
}

fn save_local_workspaces(
    config_path: &Path,
    workspaces: &[LocalWorkspaceConfig],
) -> Result<(), String> {
    let path = workspace_config_path(config_path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(workspaces).map_err(|error| error.to_string())?;
    fs::write(&path, bytes).map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn upsert_local_workspace(
    config_path: &Path,
    workspace: LocalWorkspaceConfig,
) -> Result<(), String> {
    let mut workspaces = load_local_workspaces(config_path)?;
    if let Some(existing) = workspaces
        .iter_mut()
        .find(|existing| existing.workspace_id == workspace.workspace_id)
    {
        *existing = workspace;
    } else {
        workspaces.push(workspace);
    }
    save_local_workspaces(config_path, &workspaces)
}

fn default_cli_config_path() -> Result<PathBuf, String> {
    if let Ok(root) = env::var(CLI_CONFIG_ENV) {
        return Ok(PathBuf::from(root).join("config.json"));
    }
    if let Ok(root) = env::var("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(root).join("fs2").join("config.json"));
    }
    let home = env::var("HOME").map_err(|_| "HOME is not set; cannot locate fs2 config")?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("fs2")
        .join("config.json"))
}

fn save_cli_config(path: &Path, config: &CliConfig) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(config).map_err(|error| error.to_string())?;
    fs::write(path, bytes).map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn load_cli_config(path: &Path) -> Result<CliConfig, String> {
    let bytes =
        fs::read(path).map_err(|_| "not logged in; run `fs2 login --backend <url>`".to_owned())?;
    serde_json::from_slice(&bytes).map_err(|error| format!("invalid {}: {error}", path.display()))
}

fn parse_http_url(backend: &str, endpoint: &str) -> Result<(String, u16, String), String> {
    let rest = backend
        .strip_prefix("http://")
        .ok_or("only http:// development backends are supported by the bootstrap CLI")?;
    let (authority, prefix_path) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.is_empty() {
        return Err("backend URL must include a host".to_owned());
    }
    let (host, port) = authority
        .split_once(':')
        .map_or((authority, Ok(80_u16)), |(host, port)| {
            (host, port.parse::<u16>())
        });
    if host.is_empty() {
        return Err("backend URL host must not be empty".to_owned());
    }
    let port = port.map_err(|error| format!("invalid backend port: {error}"))?;
    let path = if prefix_path.is_empty() {
        endpoint.to_owned()
    } else {
        format!("/{prefix_path}{endpoint}")
    };
    Ok((host.to_owned(), port, path))
}

fn request_json(
    method: &str,
    host: &str,
    port: u16,
    path: &str,
    body: Option<&str>,
    bearer_token: Option<&str>,
) -> Result<String, String> {
    let mut stream =
        TcpStream::connect((host, port)).map_err(|error| format!("request failed: {error}"))?;
    let body = body.unwrap_or("");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    )
    .map_err(|error| format!("request failed: {error}"))?;
    if !body.is_empty() {
        write!(stream, "Content-Type: application/json\r\n")
            .map_err(|error| format!("request failed: {error}"))?;
    }
    if let Some(token) = bearer_token {
        write!(stream, "Authorization: Bearer {token}\r\n")
            .map_err(|error| format!("request failed: {error}"))?;
    }
    write!(stream, "\r\n{body}").map_err(|error| format!("request failed: {error}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("response failed: {error}"))?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or("response was not valid HTTP")?;
    if !head.starts_with("HTTP/1.1 2") && !head.starts_with("HTTP/1.0 2") {
        return Err(format!("request failed: {head}"));
    }
    Ok(body.to_owned())
}

#[cfg(test)]
fn doctor(path: impl AsRef<Path>) -> Result<String, String> {
    doctor_inner(path, None, None)
}

fn doctor_with_context(
    path: impl AsRef<Path>,
    credentials: &dyn CredentialStore,
    config_path: &Path,
) -> Result<String, String> {
    doctor_inner(path, Some(credentials), Some(config_path))
}

fn doctor_inner(
    path: impl AsRef<Path>,
    credentials: Option<&dyn CredentialStore>,
    cli_config_path: Option<&Path>,
) -> Result<String, String> {
    let root = path.as_ref();
    let config_path = fs2_config_path(root);
    let ignore_path = root.join(".fs2ignore");
    let mut out = String::new();
    out.push_str("FS2 doctor:\n");

    let config = if config_path.exists() {
        let config_text = fs::read_to_string(&config_path)
            .map_err(|error| format!("could not read {}: {error}", config_path.display()))?;
        writeln!(out, "  config: {}", config_path.display()).map_err(|error| error.to_string())?;
        fs2_rules::parse_config_toml(&config_text)
            .map_err(|error| format!("{}: {error}", config_path.display()))?
    } else {
        out.push_str("  config: not found\n");
        fs2_rules::Config::default()
    };

    let ignore_rules = if ignore_path.exists() {
        let ignore_text = fs::read_to_string(&ignore_path)
            .map_err(|error| format!("could not read {}: {error}", ignore_path.display()))?;
        fs2_rules::parse_fs2ignore(&ignore_text)
            .map_err(|error| format!("{}: {error}", ignore_path.display()))?
    } else {
        Vec::new()
    };

    let probe_paths = git_probe_paths(&config, &ignore_rules);
    let structural_sources = git_structural_syncable_sources(&config, &ignore_rules);
    let env_probe_paths = env_safety_probe_paths(root, &config, &ignore_rules);
    let case_policy = CasePolicy::Portable;
    let engine =
        fs2_rules::RuleEngine::new(config, ignore_rules).map_err(|error| error.to_string())?;
    append_fuse_doctor(&mut out)?;
    append_daemon_doctor(&mut out)?;
    append_cli_account_doctor(&mut out, credentials, cli_config_path)?;
    append_cache_doctor(root, &mut out)?;
    append_path_collision_doctor(root, case_policy, &engine, &mut out)?;
    append_env_safety_doctor(root, &engine, &env_probe_paths, &mut out)?;
    let confirmed_overrides = git_normal_overrides(&engine, &probe_paths)?;
    let possible_sources = structural_sources
        .into_iter()
        .filter(|source| !confirmed_overrides.contains(source))
        .collect::<Vec<_>>();
    if confirmed_overrides.is_empty() && possible_sources.is_empty() {
        out.push_str("  git internals: protected by built-in defaults\n");
    } else {
        if !confirmed_overrides.is_empty() {
            out.push_str(
                "  git internals: WARNING confirmed syncable rule matches .git internals\n",
            );
            for pattern in confirmed_overrides {
                writeln!(out, "    - {pattern}").map_err(|error| error.to_string())?;
            }
            out.push_str("    fix: remove syncable `.git` rules or add `:local-only .git/**` to `.fs2ignore`\n");
        }
        if !possible_sources.is_empty() {
            out.push_str("  git internals: CAUTION broad syncable rule may reach .git internals\n");
            for pattern in possible_sources {
                writeln!(out, "    - {pattern}").map_err(|error| error.to_string())?;
            }
            out.push_str("    fix: narrow broad normal/pin/lazy rules or add `:local-only **/.git/**` to `.fs2ignore`\n");
        }
    }
    append_dependency_doctor(root, &engine, &mut out)?;
    Ok(out)
}

fn fs2_config_path(path: &Path) -> PathBuf {
    path.join(".fs2").join("config.toml")
}

fn append_fuse_doctor(out: &mut String) -> Result<(), String> {
    let status = if cfg!(target_os = "linux") {
        let fuse = Path::new("/dev/fuse");
        if fuse.exists() {
            match fs::OpenOptions::new().read(true).write(true).open(fuse) {
                Ok(_) => "available and writable at /dev/fuse".to_owned(),
                Err(error) => format!(
                    "WARNING /dev/fuse exists but is not usable: {error}; install fuse3 and add your user to the fuse group before `fs2 mount`"
                ),
            }
        } else {
            "WARNING not available; install fuse3 and ensure /dev/fuse is accessible before `fs2 mount`".to_owned()
        }
    } else if cfg!(target_os = "macos") {
        if Path::new("/Library/Filesystems/macfuse.fs").exists()
            || Path::new("/Library/Filesystems/osxfuse.fs").exists()
        {
            "macFUSE installation detected".to_owned()
        } else {
            "WARNING macFUSE not detected; install macFUSE before `fs2 mount`".to_owned()
        }
    } else {
        "unsupported platform for MVP; use macOS or Linux".to_owned()
    };
    writeln!(out, "  fuse: {status}").map_err(|error| error.to_string())
}

fn append_daemon_doctor(out: &mut String) -> Result<(), String> {
    let socket_path = fs2_daemon_socket_path();
    if daemon_socket_connects(&socket_path) {
        writeln!(out, "  daemon: reachable at {}", socket_path.display())
            .map_err(|error| error.to_string())
    } else {
        writeln!(
            out,
            "  daemon: not running at {}; daemon startup is not implemented yet, so `fs2 mount` only records mount configuration in this MVP",
            socket_path.display()
        )
        .map_err(|error| error.to_string())
    }
}

fn fs2_daemon_socket_path() -> PathBuf {
    if let Some(path) = env::var_os("FS2_DAEMON_SOCKET") {
        return PathBuf::from(path);
    }
    if let Some(path) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(path).join("fs2").join("fs2d.sock");
    }
    env::temp_dir().join("fs2d.sock")
}

#[cfg(unix)]
fn daemon_socket_connects(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

#[cfg(not(unix))]
fn daemon_socket_connects(_path: &Path) -> bool {
    false
}

fn append_cli_account_doctor(
    out: &mut String,
    credentials: Option<&dyn CredentialStore>,
    config_path: Option<&Path>,
) -> Result<(), String> {
    let (Some(credentials), Some(config_path)) = (credentials, config_path) else {
        return Ok(());
    };
    let Ok(config) = load_cli_config(config_path) else {
        out.push_str("  backend: not configured; run `fs2 login --backend <url>`\n");
        out.push_str("  auth token: not configured; run `fs2 login --backend <url>`\n");
        return append_workspace_key_doctor(out, config_path);
    };

    append_backend_connection_doctor(out, &config)?;
    append_auth_token_doctor(out, credentials, &config)?;
    append_workspace_key_doctor(out, config_path)
}

fn append_backend_connection_doctor(out: &mut String, config: &CliConfig) -> Result<(), String> {
    match connect_backend(&config.backend_url) {
        Ok(()) => writeln!(out, "  backend: reachable at {}", config.backend_url),
        Err(error) => writeln!(
            out,
            "  backend: WARNING cannot reach {}: {error}; start the backend or rerun `fs2 login --backend <url>`",
            config.backend_url
        ),
    }
    .map_err(|error| error.to_string())
}

fn append_auth_token_doctor(
    out: &mut String,
    credentials: &dyn CredentialStore,
    config: &CliConfig,
) -> Result<(), String> {
    let Some(token) = credentials.get_access_token()? else {
        out.push_str("  auth token: missing; run `fs2 login --backend <url>`\n");
        return Ok(());
    };
    let (host, port, path) = parse_http_url(&config.backend_url, "/v1/devices")?;
    match request_json("GET", &host, port, &path, None, Some(&token)) {
        Ok(_) => out.push_str("  auth token: valid for backend device list\n"),
        Err(error) => writeln!(
            out,
            "  auth token: WARNING backend rejected or could not validate stored token: {error}; run `fs2 login --backend <url>`"
        )
        .map_err(|error| error.to_string())?,
    }
    Ok(())
}

fn append_workspace_key_doctor(out: &mut String, config_path: &Path) -> Result<(), String> {
    let workspaces = load_local_workspaces(config_path)?;
    if workspaces.is_empty() {
        out.push_str("  workspace keys: no local workspaces; run `fs2 workspace create <name>` or `fs2 workspace init <path> --name <name>`\n");
        return Ok(());
    }
    let key_store = fs2_crypto::KeyringWorkspaceKeyStore::new();
    let mut available = 0_usize;
    for workspace in &workspaces {
        let Ok(workspace_id) = workspace.workspace_id.parse::<WorkspaceId>() else {
            writeln!(
                out,
                "  workspace keys: WARNING {} has invalid workspace id; rerun `fs2 workspace init <path> --name {}`",
                workspace.name, workspace.name
            )
            .map_err(|error| error.to_string())?;
            continue;
        };
        if workspace.root_node_id.parse::<NodeId>().is_err()
            || !Path::new(&workspace.metadata_db).exists()
        {
            writeln!(
                out,
                "  workspace keys: WARNING {} has incomplete local metadata; rerun `fs2 workspace init <path> --name {}`",
                workspace.name, workspace.name
            )
            .map_err(|error| error.to_string())?;
            continue;
        }
        match key_store.load_workspace_keys(workspace_id) {
            Ok(Some(_)) => available += 1,
            Ok(None) => writeln!(
                out,
                "  workspace keys: WARNING {} keys are unavailable; key enrollment/recovery is not implemented yet, so do not mount this workspace on this device",
                workspace.name
            )
            .map_err(|error| error.to_string())?,
            Err(error) => writeln!(
                out,
                "  workspace keys: WARNING could not read keys for {}: {error}; unlock the OS keychain and rerun `fs2 doctor`",
                workspace.name
            )
            .map_err(|error| error.to_string())?,
        }
    }
    if available > 0 {
        writeln!(
            out,
            "  workspace keys: available for {available} workspace(s)"
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn connect_backend(backend_url: &str) -> Result<(), String> {
    let (host, port, _) = parse_http_url(backend_url, "/")?;
    let mut addrs = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|error| format!("could not resolve backend host: {error}"))?;
    let addr = addrs
        .next()
        .ok_or("could not resolve backend host to an address")?;
    TcpStream::connect_timeout(&addr, Duration::from_millis(500))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn append_cache_doctor(root: &Path, out: &mut String) -> Result<(), String> {
    let cache_dir = root.join(".fs2").join("cache");
    if !cache_dir.exists() {
        writeln!(
            out,
            "  cache: not initialized at {}; create it with `mkdir -p {}` before hydration",
            cache_dir.display(),
            cache_dir.display()
        )
        .map_err(|error| error.to_string())?;
        return Ok(());
    }
    if !cache_dir.is_dir() {
        writeln!(
            out,
            "  cache: WARNING {} is not a directory; move it aside and rerun `fs2 workspace init`",
            cache_dir.display()
        )
        .map_err(|error| error.to_string())?;
        return Ok(());
    }
    let probe = cache_dir.join(".fs2-doctor-write-test");
    match fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe)
    {
        Ok(_) => {
            fs::remove_file(&probe)
                .map_err(|error| format!("could not remove {}: {error}", probe.display()))?;
            writeln!(out, "  cache: writable at {}", cache_dir.display())
                .map_err(|error| error.to_string())?;
        }
        Err(error) => {
            writeln!(
                out,
                "  cache: WARNING cannot write {}: {error}; fix permissions with `chmod u+rwx {}`",
                cache_dir.display(),
                cache_dir.display()
            )
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn append_path_collision_doctor(
    root: &Path,
    policy: CasePolicy,
    engine: &fs2_rules::RuleEngine,
    out: &mut String,
) -> Result<(), String> {
    let mut collisions = Vec::new();
    collect_path_collisions(root, root, policy, engine, &mut collisions)?;
    if collisions.is_empty() {
        out.push_str("  path collisions: none under portable naming policy\n");
    } else {
        out.push_str("  path collisions: WARNING portable sibling collisions found; rename one of each pair before syncing\n");
        for collision in collisions {
            writeln!(out, "    - {collision}").map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn collect_path_collisions(
    root: &Path,
    dir: &Path,
    policy: CasePolicy,
    engine: &fs2_rules::RuleEngine,
    collisions: &mut Vec<String>,
) -> Result<(), String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            collisions.push(format!(
                "{}: cannot scan for collisions: {error}",
                display_relative(root, dir)
            ));
            return Ok(());
        }
    };
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut child_dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        let not_sync_hazard = path_is_not_sync_hazard(root, &path, file_type.is_dir(), engine)?;
        if file_type.is_dir() {
            child_dirs.push(path);
            if not_sync_hazard {
                continue;
            }
        } else if not_sync_hazard {
            continue;
        }
        if let Ok(normalized) = try_normalized_name(&name, policy) {
            if let Some(first) = seen.insert(normalized.to_string(), name.clone()) {
                collisions.push(format!(
                    "{}: `{first}` collides with `{name}`; rename one before `fs2 workspace init`",
                    display_relative(root, dir)
                ));
            }
        }
    }
    for child in child_dirs {
        collect_path_collisions(root, &child, policy, engine, collisions)?;
    }
    Ok(())
}

fn path_is_not_sync_hazard(
    root: &Path,
    path: &Path,
    is_dir: bool,
    engine: &fs2_rules::RuleEngine,
) -> Result<bool, String> {
    let relative = display_relative(root, path);
    let workspace_path = WorkspacePath::parse(&relative).map_err(|error| error.to_string())?;
    let resolution = engine
        .resolve(
            &workspace_path,
            if is_dir {
                fs2_rules::RulePathKind::Directory
            } else {
                fs2_rules::RulePathKind::File
            },
            fs2_rules::EvaluationPurpose::NewLocalCreate,
            None,
        )
        .map_err(|error| error.to_string())?;
    Ok(matches!(
        resolution.effective_rule.action,
        RuleAction::Generated
            | RuleAction::DependencyCache
            | RuleAction::LocalOnly
            | RuleAction::Ignore
    ))
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map_or_else(
            || ".".to_owned(),
            |relative| relative.to_string_lossy().replace('\\', "/"),
        )
}
fn env_safety_probe_paths(
    root: &Path,
    config: &fs2_rules::Config,
    ignore_rules: &[fs2_rules::IgnoreRule],
) -> Vec<String> {
    let mut probes = vec![
        ".env".to_owned(),
        ".env.local".to_owned(),
        ".env.development".to_owned(),
        ".env.production".to_owned(),
        ".env.test".to_owned(),
    ];
    let mut unsafe_recursive_env_globs = Vec::new();
    let mut safe_recursive_env_globs = Vec::new();
    for rule in &config.rules {
        track_recursive_env_glob(
            rule.action,
            &rule.pattern,
            &mut unsafe_recursive_env_globs,
            &mut safe_recursive_env_globs,
        );
        add_env_probe_from_rule(root, &rule.pattern, rule.action, &mut probes);
    }
    for rule in ignore_rules {
        track_recursive_env_glob(
            rule.rule.action,
            &rule.pattern,
            &mut unsafe_recursive_env_globs,
            &mut safe_recursive_env_globs,
        );
        add_env_probe_from_rule(root, &rule.pattern, rule.rule.action, &mut probes);
    }
    for pattern in unsafe_recursive_env_globs {
        if !safe_recursive_env_globs
            .iter()
            .any(|safe| recursive_env_safe_covers(safe, &pattern))
        {
            push_env_unbounded_warning(
                &format!("recursive rule `{pattern}` may sync unbounded `.env` descendants"),
                &mut probes,
            );
        }
    }
    probes
}

fn recursive_env_safe_covers(safe: &str, unsafe_pattern: &str) -> bool {
    if safe == unsafe_pattern || safe == "**" {
        return true;
    }
    if safe == "*/**" && unsafe_pattern.contains('/') {
        return true;
    }
    if let Some(prefix) = safe.strip_suffix("/**") {
        return unsafe_pattern == prefix
            || unsafe_pattern
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('/'));
    }
    false
}

fn track_recursive_env_glob(
    action: RuleAction,
    pattern: &str,
    unsafe_patterns: &mut Vec<String>,
    safe_patterns: &mut Vec<String>,
) {
    if expand_tracked_recursive_env_brace(action, pattern, unsafe_patterns, safe_patterns) {
        return;
    }
    if expand_tracked_recursive_env_class(action, pattern, unsafe_patterns, safe_patterns) {
        return;
    }
    let normalized = pattern.trim_start_matches('/').replace('\\', "/");
    if env_rule_is_safe(action) && normalized == ".env*" {
        push_unique_pattern(safe_patterns, "**".to_owned());
        return;
    }
    if !recursive_env_glob_pattern(pattern) {
        return;
    }
    let pattern = normalized;
    let target = if env_rule_is_safe(action) {
        safe_patterns
    } else {
        unsafe_patterns
    };
    push_unique_pattern(target, pattern.clone());
    if env_rule_is_safe(action) && pattern.contains(".env") {
        if let Some((base, suffix)) = pattern.split_once("/.env") {
            if suffix.starts_with('*') {
                push_unique_pattern(target, base.to_owned());
            }
        }
    }
}

fn expand_tracked_recursive_env_brace(
    action: RuleAction,
    pattern: &str,
    unsafe_patterns: &mut Vec<String>,
    safe_patterns: &mut Vec<String>,
) -> bool {
    let Some(open) = pattern.find('{') else {
        return false;
    };
    let Some(close_offset) = pattern[open + 1..].find('}') else {
        return false;
    };
    let close = open + 1 + close_offset;
    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    for alternative in pattern[open + 1..close].split(',') {
        let expanded = format!("{prefix}{alternative}{suffix}");
        track_recursive_env_glob(action, &expanded, unsafe_patterns, safe_patterns);
    }
    true
}

fn expand_tracked_recursive_env_class(
    action: RuleAction,
    pattern: &str,
    unsafe_patterns: &mut Vec<String>,
    safe_patterns: &mut Vec<String>,
) -> bool {
    let Some(open) = pattern.find('[') else {
        return false;
    };
    let Some(close_offset) = pattern[open + 1..].find(']') else {
        return false;
    };
    let close = open + 1 + close_offset;
    let class = &pattern[open + 1..close];
    if class.starts_with('!') || class.starts_with('^') {
        return false;
    }
    let alternatives = class_alternatives(class);
    if alternatives.is_empty() || alternatives.len() > 128 {
        return false;
    }
    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    for alternative in alternatives {
        let expanded = format!("{prefix}{alternative}{suffix}");
        track_recursive_env_glob(action, &expanded, unsafe_patterns, safe_patterns);
    }
    true
}

fn push_unique_pattern(patterns: &mut Vec<String>, pattern: String) {
    if !patterns.iter().any(|existing| existing == &pattern) {
        patterns.push(pattern);
    }
}

fn add_env_probe_from_rule(
    root: &Path,
    pattern: &str,
    action: RuleAction,
    probes: &mut Vec<String>,
) {
    if expand_env_rule_brace_pattern(root, pattern, action, probes) {
        return;
    }
    if expand_env_rule_class_pattern(root, pattern, action, probes) {
        return;
    }
    if !env_rule_is_safe(action) && recursive_env_pattern(pattern) {
        if let Some(probe) = recursive_env_warning_probe(pattern) {
            push_env_probe_warning(
                &probe,
                &format!("recursive rule `{pattern}` may sync unbounded `.env` descendants"),
                probes,
            );
        }
    }
    add_env_probe_from_pattern(root, pattern, probes);
}
fn recursive_env_pattern(pattern: &str) -> bool {
    let pattern = pattern.trim_start_matches('/').replace('\\', "/");
    if pattern.split('/').any(|component| component == ".git") {
        return false;
    }
    pattern.contains("**")
        || pattern.ends_with('/')
        || (!pattern.contains(".env") && pattern_contains_glob(&pattern))
}

fn recursive_env_glob_pattern(pattern: &str) -> bool {
    recursive_env_pattern(pattern)
}

fn recursive_env_warning_probe(pattern: &str) -> Option<String> {
    let normalized = pattern.trim_start_matches('/').replace('\\', "/");
    if normalized.contains(".env") {
        return sample_env_glob(&normalized.replace(
            "**",
            "fs2doctorprobe/nested/deeper/even/deeper/still/deeper/final",
        ));
    }
    let trimmed = normalized
        .trim_end_matches('/')
        .strip_suffix("/**")
        .or_else(|| normalized.trim_end_matches('/').strip_suffix("/*"))
        .unwrap_or_else(|| normalized.trim_end_matches('/'));
    if trimmed.is_empty() {
        Some("fs2doctorprobe/nested/deeper/.env.fs2doctorprobe".to_owned())
    } else {
        Some(format!(
            "{trimmed}/fs2doctorprobe/nested/deeper/.env.fs2doctorprobe"
        ))
    }
}

const ENV_PROBE_WARNING_PREFIX: &str = "__fs2doctor_env_warning__:";
const ENV_UNBOUNDED_WARNING_PREFIX: &str = "__fs2doctor_env_unbounded_warning__:";
const MAX_ENV_SAFETY_PROBES: usize = 512;
const DOTENV_PROBE_NAMES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.development",
    ".env.production",
    ".env.test",
    ".env.fs2doctorprobe",
];

fn push_env_probe_warning(probe: &str, warning: &str, probes: &mut Vec<String>) {
    let entry = format!("{ENV_PROBE_WARNING_PREFIX}{probe}\t{warning}");
    if !probes.iter().any(|existing| existing == &entry) {
        probes.push(entry);
    }
}

fn push_env_unbounded_warning(warning: &str, probes: &mut Vec<String>) {
    let entry = format!("{ENV_UNBOUNDED_WARNING_PREFIX}{warning}");
    if !probes.iter().any(|existing| existing == &entry) {
        probes.push(entry);
    }
}

fn add_env_probe_from_pattern(root: &Path, pattern: &str, probes: &mut Vec<String>) {
    if probes.len() >= MAX_ENV_SAFETY_PROBES {
        return;
    }
    let pattern = pattern.trim_start_matches('/');
    if expand_env_brace_pattern(root, pattern, probes) {
        return;
    }
    if expand_env_class_pattern(root, pattern, probes) {
        return;
    }
    let directory_pattern = pattern.ends_with('/');
    let pattern = pattern.trim_end_matches('/');
    let existing_literal_dir =
        !directory_pattern && !pattern_contains_glob(pattern) && root.join(pattern).is_dir();
    if !pattern.contains(".env") {
        for probe in sample_broad_env_probes(pattern, directory_pattern || existing_literal_dir) {
            push_env_probe(probe, probes);
        }
        return;
    }
    if pattern_contains_glob(pattern) {
        for probe in sample_env_glob_probes(pattern) {
            push_env_probe(probe, probes);
        }
        return;
    }
    push_env_probe(pattern.replace('\\', "/"), probes);
}

fn push_env_probe(probe: String, probes: &mut Vec<String>) {
    let Some(name) = probe.rsplit('/').next() else {
        return;
    };
    if !name.starts_with(".env") {
        return;
    }
    if probes.len() >= MAX_ENV_SAFETY_PROBES || probes.iter().any(|existing| existing == &probe) {
        return;
    }
    probes.push(probe);
}

fn sample_broad_env_probes(pattern: &str, directory_pattern: bool) -> Vec<String> {
    if !pattern_contains_glob(pattern) && !directory_pattern {
        return Vec::new();
    }
    let normalized = pattern.replace('\\', "/");
    let recursive = directory_pattern || pattern_contains_glob(&normalized);
    let trimmed = normalized
        .strip_suffix("/**")
        .or_else(|| normalized.strip_suffix("/*"))
        .unwrap_or(normalized.as_str());
    let mut env_patterns = Vec::new();
    if trimmed.is_empty() {
        for name in DOTENV_PROBE_NAMES {
            env_patterns.push((*name).to_owned());
        }
    } else {
        let mut prefixes = vec![trimmed.to_owned(), format!("{trimmed}/fs2doctorprobe")];
        if recursive {
            prefixes.push(format!("{trimmed}/fs2doctorprobe/nested"));
        }
        for prefix in prefixes {
            for name in DOTENV_PROBE_NAMES {
                env_patterns.push(format!("{prefix}/{name}"));
            }
        }
    }
    env_patterns
        .into_iter()
        .filter_map(|pattern| sample_env_glob(&pattern))
        .collect()
}

fn expand_env_rule_brace_pattern(
    root: &Path,
    pattern: &str,
    action: RuleAction,
    probes: &mut Vec<String>,
) -> bool {
    let Some(open) = pattern.find('{') else {
        return false;
    };
    let Some(close_offset) = pattern[open + 1..].find('}') else {
        return false;
    };
    let close = open + 1 + close_offset;
    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    for alternative in pattern[open + 1..close].split(',') {
        if probes.len() >= MAX_ENV_SAFETY_PROBES {
            break;
        }
        let expanded =
            expanded_env_rule_pattern(pattern, &format!("{prefix}{alternative}{suffix}"));
        add_env_probe_from_rule(root, &expanded, action, probes);
    }
    true
}

fn expand_env_rule_class_pattern(
    root: &Path,
    pattern: &str,
    action: RuleAction,
    probes: &mut Vec<String>,
) -> bool {
    let Some(open) = pattern.find('[') else {
        return false;
    };
    let Some(close_offset) = pattern[open + 1..].find(']') else {
        return false;
    };
    let close = open + 1 + close_offset;
    let class = &pattern[open + 1..close];
    if class.starts_with('!') || class.starts_with('^') {
        return false;
    }
    let alternatives = class_alternatives(class);
    if alternatives.is_empty() || alternatives.len() > 128 {
        return false;
    }
    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    for alternative in alternatives {
        if probes.len() >= MAX_ENV_SAFETY_PROBES {
            break;
        }
        let expanded =
            expanded_env_rule_pattern(pattern, &format!("{prefix}{alternative}{suffix}"));
        add_env_probe_from_rule(root, &expanded, action, probes);
    }
    true
}

fn expanded_env_rule_pattern(original: &str, expanded: &str) -> String {
    if !original.contains(".env") && !expanded.ends_with('/') {
        format!("{expanded}/")
    } else {
        expanded.to_owned()
    }
}

fn expand_env_brace_pattern(root: &Path, pattern: &str, probes: &mut Vec<String>) -> bool {
    let Some(open) = pattern.find('{') else {
        return false;
    };
    let Some(close_offset) = pattern[open + 1..].find('}') else {
        return false;
    };
    let close = open + 1 + close_offset;
    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    for alternative in pattern[open + 1..close].split(',') {
        if probes.len() >= MAX_ENV_SAFETY_PROBES {
            break;
        }
        let expanded = format!("{prefix}{alternative}{suffix}");
        add_env_probe_from_pattern(root, &expanded, probes);
    }
    true
}

fn expand_env_class_pattern(root: &Path, pattern: &str, probes: &mut Vec<String>) -> bool {
    let Some(open) = pattern.find('[') else {
        return false;
    };
    let Some(close_offset) = pattern[open + 1..].find(']') else {
        return false;
    };
    let close = open + 1 + close_offset;
    let class = &pattern[open + 1..close];
    if class.starts_with('!') || class.starts_with('^') {
        return false;
    }
    let alternatives = class_alternatives(class);
    if alternatives.is_empty() || alternatives.len() > 128 {
        return false;
    }
    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    for alternative in alternatives {
        if probes.len() >= MAX_ENV_SAFETY_PROBES {
            break;
        }
        let expanded = format!("{prefix}{alternative}{suffix}");
        add_env_probe_from_pattern(root, &expanded, probes);
    }
    true
}

fn class_alternatives(class: &str) -> Vec<char> {
    let chars = class.chars().collect::<Vec<_>>();
    let mut out = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if index + 2 < chars.len() && chars[index + 1] == '-' {
            let start = chars[index] as u32;
            let end = chars[index + 2] as u32;
            if start <= end {
                for value in start..=end {
                    if let Some(ch) = char::from_u32(value) {
                        out.push(ch);
                    }
                }
            }
            index += 3;
        } else {
            out.push(chars[index]);
            index += 1;
        }
    }
    out
}

fn pattern_contains_glob(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('[') || pattern.contains('{')
}

fn sample_env_glob_probes(pattern: &str) -> Vec<String> {
    let mut patterns = vec![pattern.to_owned()];
    if pattern.contains("/**/") {
        patterns.push(pattern.replace("/**/", "/"));
        patterns.push(pattern.replace("/**/", "/fs2doctorprobe/nested/"));
    }
    if pattern.starts_with("**/") {
        patterns.push(pattern.replacen("**/", "", 1));
        patterns.push(pattern.replacen("**/", "fs2doctorprobe/nested/", 1));
        patterns.push(pattern.replacen("**/", "fs2doctorprobe/nested/deeper/", 1));
    }
    patterns
        .into_iter()
        .filter_map(|pattern| sample_env_glob(&pattern))
        .fold(Vec::new(), |mut probes, probe| {
            if !probes.iter().any(|existing| existing == &probe) {
                probes.push(probe);
            }
            probes
        })
}

fn sample_env_glob(pattern: &str) -> Option<String> {
    if !pattern.contains(".env") {
        return None;
    }
    let normalized = pattern.replace('\\', "/");
    let mut chars = normalized.chars().peekable();
    let mut sample = String::new();
    while let Some(ch) = chars.next() {
        match ch {
            '*' => {
                if chars.peek() == Some(&'*') {
                    let _ = chars.next();
                }
                sample.push_str("fs2doctorprobe");
            }
            '?' => sample.push('x'),
            '[' => {
                let mut class = String::new();
                for inner in chars.by_ref() {
                    if inner == ']' {
                        break;
                    }
                    class.push(inner);
                }
                sample.push(sample_char_class(&class));
            }
            '{' => {
                let mut alternatives = String::new();
                for inner in chars.by_ref() {
                    if inner == '}' {
                        break;
                    }
                    alternatives.push(inner);
                }
                sample.push_str(alternatives.split(',').next().unwrap_or("a"));
            }
            _ => sample.push(ch),
        }
    }
    Some(sample)
}

fn sample_char_class(class: &str) -> char {
    class
        .strip_prefix('!')
        .or_else(|| class.strip_prefix('^'))
        .map_or_else(
            || class.chars().find(|ch| *ch != '-').unwrap_or('a'),
            sample_negated_char_class,
        )
}

fn sample_negated_char_class(negated: &str) -> char {
    ['0', 'a', 'A', '_', '-', 'z', '!', '~']
        .into_iter()
        .find(|candidate| !char_class_contains(negated, *candidate))
        .unwrap_or('0')
}

fn char_class_contains(class: &str, candidate: char) -> bool {
    let chars = class.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        if index + 2 < chars.len() && chars[index + 1] == '-' {
            if chars[index] <= candidate && candidate <= chars[index + 2] {
                return true;
            }
            index += 3;
        } else {
            if chars[index] == candidate {
                return true;
            }
            index += 1;
        }
    }
    false
}

fn append_env_safety_doctor(
    root: &Path,
    engine: &fs2_rules::RuleEngine,
    probe_paths: &[String],
    out: &mut String,
) -> Result<(), String> {
    let mut unsafe_env_files = Vec::new();
    collect_env_safety(root, root, engine, &mut unsafe_env_files)?;
    for probe_path in probe_paths {
        if let Some(payload) = probe_path.strip_prefix(ENV_PROBE_WARNING_PREFIX) {
            if let Some((probe, warning)) = payload.split_once('\t') {
                probe_env_safety_warning(engine, probe, warning, &mut unsafe_env_files)?;
            }
            continue;
        }
        if let Some(warning) = probe_path.strip_prefix(ENV_UNBOUNDED_WARNING_PREFIX) {
            if !unsafe_env_files.iter().any(|existing| existing == warning) {
                unsafe_env_files.push(warning.to_owned());
            }
            continue;
        }
        probe_env_safety_rule(engine, probe_path, &mut unsafe_env_files)?;
    }
    if unsafe_env_files.is_empty() {
        out.push_str("  env files: safe; `.env` files are secret or local-only when present\n");
    } else {
        out.push_str("  env files: WARNING plaintext `.env` files would sync normally; add `:secret .env*` or `:local-only .env*` to `.fs2ignore`\n");
        for path in unsafe_env_files {
            writeln!(out, "    - {path}").map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn collect_env_safety(
    root: &Path,
    dir: &Path,
    engine: &fs2_rules::RuleEngine,
    unsafe_env_files: &mut Vec<String>,
) -> Result<(), String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        let not_sync_hazard = path_is_not_sync_hazard(root, &path, file_type.is_dir(), engine)?;
        if !file_type.is_dir() && not_sync_hazard {
            continue;
        }
        if file_type.is_dir() {
            collect_env_safety(root, &path, engine, unsafe_env_files)?;
        } else if name.starts_with(".env") {
            let relative = display_relative(root, &path);
            let workspace_path =
                WorkspacePath::parse(&relative).map_err(|error| error.to_string())?;
            let resolution = engine
                .resolve(
                    &workspace_path,
                    fs2_rules::RulePathKind::File,
                    fs2_rules::EvaluationPurpose::NewLocalCreate,
                    None,
                )
                .map_err(|error| error.to_string())?;
            if !env_rule_is_safe(resolution.effective_rule.action) {
                unsafe_env_files.push(relative);
            }
        }
    }
    Ok(())
}

fn probe_env_safety_rule(
    engine: &fs2_rules::RuleEngine,
    path: &str,
    unsafe_env_files: &mut Vec<String>,
) -> Result<(), String> {
    let workspace_path = WorkspacePath::parse(path).map_err(|error| error.to_string())?;
    let resolution = engine
        .resolve(
            &workspace_path,
            fs2_rules::RulePathKind::File,
            fs2_rules::EvaluationPurpose::NewLocalCreate,
            None,
        )
        .map_err(|error| error.to_string())?;
    if matches!(
        resolution.source,
        fs2_rules::RuleSource::WorkspaceDefault { .. }
    ) {
        return Ok(());
    }
    if !env_rule_is_safe(resolution.effective_rule.action) {
        let entry = format!(
            "{path} rule resolves to {}",
            rule_action_name(resolution.effective_rule.action)
        );
        if !unsafe_env_files.iter().any(|existing| existing == &entry) {
            unsafe_env_files.push(entry);
        }
    }
    Ok(())
}

fn probe_env_safety_warning(
    engine: &fs2_rules::RuleEngine,
    path: &str,
    warning: &str,
    unsafe_env_files: &mut Vec<String>,
) -> Result<(), String> {
    let workspace_path = WorkspacePath::parse(path).map_err(|error| error.to_string())?;
    let resolution = engine
        .resolve(
            &workspace_path,
            fs2_rules::RulePathKind::File,
            fs2_rules::EvaluationPurpose::NewLocalCreate,
            None,
        )
        .map_err(|error| error.to_string())?;
    if matches!(
        resolution.source,
        fs2_rules::RuleSource::WorkspaceDefault { .. }
    ) || env_rule_is_safe(resolution.effective_rule.action)
    {
        return Ok(());
    }
    if !unsafe_env_files.iter().any(|existing| existing == warning) {
        unsafe_env_files.push(warning.to_owned());
    }
    Ok(())
}

const fn env_rule_is_safe(action: RuleAction) -> bool {
    matches!(
        action,
        RuleAction::Secret
            | RuleAction::LocalOnly
            | RuleAction::Ignore
            | RuleAction::Generated
            | RuleAction::DependencyCache
    )
}

fn append_dependency_doctor(
    root: &Path,
    engine: &fs2_rules::RuleEngine,
    out: &mut String,
) -> Result<(), String> {
    let dependency_roots =
        fs2_rules::discover_dependency_roots(root).map_err(|error| error.to_string())?;
    if dependency_roots.is_empty() {
        out.push_str("  dependencies: no package roots detected\n");
        return Ok(());
    }
    out.push_str("  dependencies:\n");
    let mut rendered_node_roots = Vec::new();
    let node_workspace_roots = dependency_roots
        .iter()
        .filter(|root| {
            root.ecosystem == fs2_rules::DependencyEcosystem::Node
                && !root.workspace_files.is_empty()
        })
        .collect::<Vec<_>>();
    for dependency_root in &dependency_roots {
        let display_root = dependency_display_root(root, &dependency_root.root);
        append_generated_directory_doctor(engine, &display_root, dependency_root, out)?;
        if dependency_root.ecosystem != fs2_rules::DependencyEcosystem::Node {
            continue;
        }
        if tooling_only_node_root(dependency_root) {
            continue;
        }
        if dependency_root_is_generated_or_local(engine, &display_root)? {
            continue;
        }
        let expected_workspace =
            nearest_node_workspace_ancestor(dependency_root, &node_workspace_roots);
        let self_expected_manager = node_expected_manager(dependency_root);
        let expected_manager =
            expected_workspace.map_or(self_expected_manager, node_expected_manager);
        let covered_workspace =
            covering_rendered_node_workspace_ancestor(dependency_root, &rendered_node_roots);
        append_package_manager_doctor(root, dependency_root, expected_manager, out)?;
        if covered_workspace.is_some() {
            continue;
        }
        let renders_guidance = dependency_root
            .generated_paths
            .iter()
            .any(|path| path == "node_modules/")
            && dependency_generated_by_effective_rule(engine, &display_root)?;
        if renders_guidance {
            let command = node_install_command(self_expected_manager).join(" ");
            writeln!(
                out,
                "    - {display_root}: node_modules/ is generated dependency cache; run `{command}` to recreate it locally"
            )
            .map_err(|error| error.to_string())?;
            rendered_node_roots.push(dependency_root);
        }
    }
    Ok(())
}

fn tooling_only_node_root(dependency_root: &fs2_rules::DependencyRoot) -> bool {
    !dependency_root.root.join("package.json").is_file()
        && dependency_root.lockfiles.is_empty()
        && !dependency_root.workspace_files.iter().any(|file| {
            matches!(
                file.as_str(),
                "pnpm-workspace.yaml" | "package.json#workspaces"
            )
        })
}

fn append_generated_directory_doctor(
    engine: &fs2_rules::RuleEngine,
    display_root: &str,
    dependency_root: &fs2_rules::DependencyRoot,
    out: &mut String,
) -> Result<(), String> {
    for generated_path in &dependency_root.generated_paths {
        let probe = generated_probe_path(display_root, generated_path);
        let workspace_path = WorkspacePath::parse(&probe).map_err(|error| error.to_string())?;
        let resolution = engine
            .resolve(
                &workspace_path,
                fs2_rules::RulePathKind::Directory,
                fs2_rules::EvaluationPurpose::NewLocalCreate,
                None,
            )
            .map_err(|error| error.to_string())?;
        if !matches!(
            resolution.effective_rule.action,
            RuleAction::Generated
                | RuleAction::DependencyCache
                | RuleAction::LocalOnly
                | RuleAction::Ignore
        ) {
            let display_path = if display_root == "." {
                generated_path.to_owned()
            } else {
                format!("{display_root}/{generated_path}")
            };
            writeln!(
                out,
                "    - {display_path}: WARNING generated directory would sync as {}; add `:generated {display_path}**` or `:local-only {display_path}**` to `.fs2ignore`",
                rule_action_name(resolution.effective_rule.action)
            )
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn generated_probe_path(display_root: &str, generated_path: &str) -> String {
    let probe_child = format!("{}/fs2-doctor-probe", generated_path.trim_end_matches('/'));
    if display_root == "." {
        probe_child
    } else {
        format!("{display_root}/{probe_child}")
    }
}

const fn rule_action_name(action: RuleAction) -> &'static str {
    match action {
        RuleAction::Ignore => "ignore",
        RuleAction::LocalOnly => "local-only",
        RuleAction::Generated => "generated",
        RuleAction::Lazy => "lazy",
        RuleAction::Pin => "pin",
        RuleAction::Normal => "normal",
        RuleAction::Secret => "secret",
        RuleAction::DependencyCache => "dependency-cache",
    }
}

fn append_package_manager_doctor(
    root: &Path,
    dependency_root: &fs2_rules::DependencyRoot,
    expected_manager: fs2_rules::PackageManager,
    out: &mut String,
) -> Result<(), String> {
    let mismatched_lockfiles = dependency_root
        .lockfiles
        .iter()
        .filter(|lockfile| {
            node_lockfile_manager(lockfile).is_some_and(|manager| manager != expected_manager)
        })
        .collect::<Vec<_>>();
    if mismatched_lockfiles.is_empty() {
        return Ok(());
    }
    let display_root = dependency_display_root(root, &dependency_root.root);
    let expected = node_manager_name(expected_manager);
    let command = node_install_command(expected_manager).join(" ");
    writeln!(
        out,
        "    - {display_root}: WARNING package manager mismatch; expected {expected} but found {}; remove stale lockfiles or run `{command}`",
        mismatched_lockfiles
            .into_iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    )
    .map_err(|error| error.to_string())
}

fn node_lockfile_manager(lockfile: &str) -> Option<fs2_rules::PackageManager> {
    match lockfile {
        "pnpm-lock.yaml" => Some(fs2_rules::PackageManager::Pnpm),
        "yarn.lock" => Some(fs2_rules::PackageManager::Yarn),
        "package-lock.json" => Some(fs2_rules::PackageManager::Npm),
        "bun.lockb" => Some(fs2_rules::PackageManager::Bun),
        _ => None,
    }
}

const fn node_manager_name(manager: fs2_rules::PackageManager) -> &'static str {
    match manager {
        fs2_rules::PackageManager::Npm => "npm",
        fs2_rules::PackageManager::Pnpm => "pnpm",
        fs2_rules::PackageManager::Yarn => "yarn",
        fs2_rules::PackageManager::Bun => "bun",
        _ => "configured manager",
    }
}

fn node_expected_manager(dependency_root: &fs2_rules::DependencyRoot) -> fs2_rules::PackageManager {
    if let Some(manager) = package_json_declared_manager(&dependency_root.root) {
        return manager;
    }
    if dependency_root
        .workspace_files
        .iter()
        .any(|file| file == "pnpm-workspace.yaml")
    {
        fs2_rules::PackageManager::Pnpm
    } else {
        dependency_root.manager
    }
}

fn package_json_declared_manager(root: &Path) -> Option<fs2_rules::PackageManager> {
    let text = fs::read_to_string(root.join("package.json")).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&text).ok()?;
    value
        .get("packageManager")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| {
            value
                .split_once('@')
                .map_or(Some(value), |(name, _)| Some(name))
        })
        .and_then(package_manager_name)
}

fn package_manager_name(name: &str) -> Option<fs2_rules::PackageManager> {
    match name {
        "npm" => Some(fs2_rules::PackageManager::Npm),
        "pnpm" => Some(fs2_rules::PackageManager::Pnpm),
        "yarn" => Some(fs2_rules::PackageManager::Yarn),
        "bun" => Some(fs2_rules::PackageManager::Bun),
        _ => None,
    }
}

const fn node_install_command(manager: fs2_rules::PackageManager) -> [&'static str; 2] {
    match manager {
        fs2_rules::PackageManager::Pnpm => ["pnpm", "install"],
        fs2_rules::PackageManager::Yarn => ["yarn", "install"],
        fs2_rules::PackageManager::Bun => ["bun", "install"],
        _ => ["npm", "install"],
    }
}

fn deps_status(path: impl AsRef<Path>) -> Result<String, String> {
    let root = path.as_ref();
    let dependency_roots =
        fs2_rules::discover_dependency_roots(root).map_err(|error| error.to_string())?;
    let parent_installed = read_dependency_install_state(root)?;
    let mut out = String::new();
    writeln!(out, "Dependencies:").map_err(|error| error.to_string())?;
    if dependency_roots.is_empty() {
        out.push_str("  none detected\n");
        return Ok(out);
    }
    for dependency_root in dependency_roots {
        let display_root = dependency_display_root(root, &dependency_root.root);
        let command = dependency_root.install_command.join(" ");
        let current_hash = dependency_lock_hash(&dependency_root)?;
        let root_installed = read_dependency_install_state(&dependency_root.root)?;
        let state_key = dependency_state_key(".", dependency_root.ecosystem);
        let parent_state_key = dependency_state_key(&display_root, dependency_root.ecosystem);
        let installed_hash = root_installed
            .get(&state_key)
            .or_else(|| parent_installed.get(&parent_state_key));
        let state = match (installed_hash, current_hash.as_ref()) {
            (Some(saved), Some(current)) if saved == current => "installed",
            (Some(_), Some(_)) => "lockfile changed; run `fs2 deps install <path> --yes`",
            (None, Some(_)) => "not installed by fs2; run `fs2 deps install <path>`",
            (_, None) => "no lockfile; install state unknown",
        };
        writeln!(
            out,
            "  - {display_root}: {} via `{command}`; {state}",
            dependency_ecosystem_name(dependency_root.ecosystem)
        )
        .map_err(|error| error.to_string())?;
        if !dependency_root.generated_paths.is_empty() {
            writeln!(
                out,
                "    generated: {}",
                dependency_root.generated_paths.join(", ")
            )
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(out)
}

fn deps_install(path: impl AsRef<Path>, yes: bool) -> Result<String, String> {
    let root = path.as_ref();
    let mut roots =
        fs2_rules::detect_dependency_roots_at(root).map_err(|error| error.to_string())?;
    if roots.is_empty() {
        return Err(format!("no dependency root detected at {}", root.display()));
    }
    if roots.len() > 1 {
        let ecosystems = roots
            .iter()
            .map(|root| dependency_ecosystem_name(root.ecosystem))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "multiple dependency roots detected at {}; cannot choose between {ecosystems}",
            root.display()
        ));
    }
    let dependency_root = roots.remove(0);
    let command = dependency_root.install_command.join(" ");
    if !yes {
        return Ok(format!(
            "Would run `{command}` in {}. Re-run with `--yes` to execute.\n",
            dependency_root.root.display()
        ));
    }
    let status = Command::new(&dependency_root.install_command[0])
        .args(&dependency_root.install_command[1..])
        .current_dir(&dependency_root.root)
        .status()
        .map_err(|error| format!("could not run `{command}`: {error}"))?;
    if !status.success() {
        return Err(format!("`{command}` exited with {status}"));
    }
    record_dependency_install_state(dependency_root)?;
    Ok(format!("Installed dependencies with `{command}`\n"))
}

fn record_dependency_install_state(
    dependency_root: fs2_rules::DependencyRoot,
) -> Result<(), String> {
    let refreshed_root = fs2_rules::detect_dependency_roots_at(&dependency_root.root)
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|root| root.ecosystem == dependency_root.ecosystem)
        .unwrap_or(dependency_root);
    let mut installed = read_dependency_install_state(&refreshed_root.root)?;
    if let Some(hash) = dependency_lock_hash(&refreshed_root)? {
        installed.insert(dependency_state_key(".", refreshed_root.ecosystem), hash);
        write_dependency_install_state(&refreshed_root.root, &installed)?;
    }
    Ok(())
}

fn dependency_state_key(path: &str, ecosystem: fs2_rules::DependencyEcosystem) -> String {
    format!("{path}:{}", dependency_ecosystem_name(ecosystem))
}

const fn dependency_ecosystem_name(ecosystem: fs2_rules::DependencyEcosystem) -> &'static str {
    match ecosystem {
        fs2_rules::DependencyEcosystem::Node => "node",
        fs2_rules::DependencyEcosystem::Rust => "rust",
        fs2_rules::DependencyEcosystem::Python => "python",
        fs2_rules::DependencyEcosystem::Go => "go",
    }
}

fn dependency_lock_hash(root: &fs2_rules::DependencyRoot) -> Result<Option<String>, String> {
    if root.lockfiles.is_empty() {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    for lockfile in &root.lockfiles {
        let path = root.root.join(lockfile);
        hasher.update(lockfile.as_bytes());
        hasher.update([0]);
        let bytes = fs::read(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        hasher.update(bytes);
        hasher.update([0]);
    }
    Ok(Some(format!("sha256:{:x}", hasher.finalize())))
}

fn dependency_install_state_path(root: &Path) -> PathBuf {
    let base = env::var_os("FS2_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_STATE_HOME").map(|path| PathBuf::from(path).join("fs2")))
        .or_else(|| env::var_os("HOME").map(|path| PathBuf::from(path).join(".local/state/fs2")))
        .unwrap_or_else(|| PathBuf::from(".fs2-local-state"));
    let identity = dependency_install_state_identity(root);
    base.join("deps").join(format!(
        "{}.json",
        hex_lower(Sha256::digest(identity.as_bytes()))
    ))
}

fn dependency_install_state_identity(root: &Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| {
            if root.is_absolute() {
                root.to_path_buf()
            } else {
                env::current_dir().map_or_else(|_| root.to_path_buf(), |cwd| cwd.join(root))
            }
        })
        .display()
        .to_string()
}

fn hex_lower(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

fn read_dependency_install_state(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let path = dependency_install_state_path(root);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|error| format!("{} is invalid JSON: {error}", path.display()))
}

fn write_dependency_install_state(
    root: &Path,
    state: &BTreeMap<String, String>,
) -> Result<(), String> {
    let path = dependency_install_state_path(root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(state).map_err(|error| error.to_string())?;
    fs::write(&path, text).map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn dependency_display_root(root: &Path, dependency_root: &Path) -> String {
    dependency_root
        .strip_prefix(root)
        .ok()
        .filter(|path| !path.as_os_str().is_empty())
        .map_or_else(|| ".".to_owned(), |path| path.display().to_string())
}

fn nearest_node_workspace_ancestor<'a>(
    dependency_root: &fs2_rules::DependencyRoot,
    workspace_roots: &'a [&fs2_rules::DependencyRoot],
) -> Option<&'a fs2_rules::DependencyRoot> {
    workspace_roots
        .iter()
        .copied()
        .filter(|candidate| {
            candidate.root != dependency_root.root
                && dependency_root.root.starts_with(&candidate.root)
                && workspace_declaration_covers(candidate, dependency_root)
        })
        .max_by_key(|candidate| candidate.root.components().count())
}

fn covering_rendered_node_workspace_ancestor<'a>(
    dependency_root: &fs2_rules::DependencyRoot,
    rendered_node_roots: &'a [&fs2_rules::DependencyRoot],
) -> Option<&'a fs2_rules::DependencyRoot> {
    rendered_node_roots.iter().copied().find(|candidate| {
        candidate.root != dependency_root.root
            && dependency_root.root.starts_with(&candidate.root)
            && workspace_declaration_covers(candidate, dependency_root)
    })
}

fn workspace_declaration_covers(
    candidate: &fs2_rules::DependencyRoot,
    dependency_root: &fs2_rules::DependencyRoot,
) -> bool {
    let Ok(relative) = dependency_root.root.strip_prefix(&candidate.root) else {
        return false;
    };
    let relative = relative.to_string_lossy().replace('\\', "/");
    if node_expected_manager(candidate) == fs2_rules::PackageManager::Pnpm
        && candidate
            .workspace_files
            .iter()
            .any(|file| file == "pnpm-workspace.yaml")
    {
        let patterns = pnpm_workspace_patterns(&candidate.root);
        if workspace_patterns_cover(&patterns, &relative) {
            return true;
        }
    }
    if node_expected_manager(candidate) != fs2_rules::PackageManager::Pnpm
        && candidate
            .workspace_files
            .iter()
            .any(|file| file == "package.json#workspaces")
    {
        return workspace_patterns_cover(
            &package_json_workspace_patterns(&candidate.root),
            &relative,
        );
    }
    false
}

fn package_json_workspace_patterns(root: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(root.join("package.json")) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    match value.get("workspaces") {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(serde_json::Value::Object(object)) => object
            .get("packages")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn pnpm_workspace_patterns(root: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(root.join("pnpm-workspace.yaml")) else {
        return Vec::new();
    };
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&text) else {
        return Vec::new();
    };
    let serde_yaml::Value::Mapping(mapping) = value else {
        return Vec::new();
    };
    let Some(packages) = mapping.get(serde_yaml::Value::String("packages".to_owned())) else {
        return Vec::new();
    };
    let serde_yaml::Value::Sequence(items) = packages else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(serde_yaml::Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn workspace_patterns_cover(patterns: &[String], relative: &str) -> bool {
    let included = patterns
        .iter()
        .filter(|pattern| !pattern.starts_with('!'))
        .any(|pattern| workspace_pattern_matches(pattern, relative));
    let excluded = patterns
        .iter()
        .filter_map(|pattern| pattern.strip_prefix('!'))
        .any(|pattern| workspace_pattern_matches(pattern, relative));
    included && !excluded
}

fn workspace_pattern_matches(pattern: &str, relative: &str) -> bool {
    let pattern = pattern.trim_start_matches("./").trim_end_matches('/');
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_or(relative == pattern, |glob| {
            glob.compile_matcher().is_match(relative)
        })
}

fn dependency_generated_by_effective_rule(
    engine: &fs2_rules::RuleEngine,
    dependency_root: &str,
) -> Result<bool, String> {
    let probe = if dependency_root == "." {
        "node_modules/pkg/index.js".to_owned()
    } else {
        format!("{dependency_root}/node_modules/pkg/index.js")
    };
    let path = WorkspacePath::parse(&probe).map_err(|error| error.to_string())?;
    let resolution = engine
        .resolve(
            &path,
            fs2_rules::RulePathKind::File,
            fs2_rules::EvaluationPurpose::ExistingOrRemoteLookup,
            None,
        )
        .map_err(|error| error.to_string())?;
    Ok(matches!(
        resolution.effective_rule.action,
        RuleAction::DependencyCache | RuleAction::Generated
    ))
}

fn dependency_root_is_generated_or_local(
    engine: &fs2_rules::RuleEngine,
    dependency_root: &str,
) -> Result<bool, String> {
    if dependency_root == "." {
        return Ok(false);
    }
    let path = WorkspacePath::parse(dependency_root).map_err(|error| error.to_string())?;
    let resolution = engine
        .resolve(
            &path,
            fs2_rules::RulePathKind::Directory,
            fs2_rules::EvaluationPurpose::ExistingOrRemoteLookup,
            None,
        )
        .map_err(|error| error.to_string())?;
    Ok(matches!(
        resolution.effective_rule.action,
        RuleAction::DependencyCache | RuleAction::Generated | RuleAction::LocalOnly
    ))
}

fn git_normal_overrides(
    engine: &fs2_rules::RuleEngine,
    probe_paths: &[String],
) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for probe in probe_paths {
        let path = WorkspacePath::parse(probe).map_err(|error| error.to_string())?;
        let resolution = engine
            .resolve(
                &path,
                fs2_rules::RulePathKind::File,
                fs2_rules::EvaluationPurpose::NewLocalCreate,
                None,
            )
            .map_err(|error| error.to_string())?;
        if is_git_syncable_action(resolution.effective_rule.action) {
            let source = describe_rule_source(&resolution.source);
            if !out.contains(&source) {
                out.push(source);
            }
        }
    }
    Ok(out)
}

const fn is_git_syncable_action(action: RuleAction) -> bool {
    matches!(
        action,
        RuleAction::Normal | RuleAction::Lazy | RuleAction::Pin
    )
}
fn git_probe_paths(
    config: &fs2_rules::Config,
    ignore_rules: &[fs2_rules::IgnoreRule],
) -> Vec<String> {
    let mut probes = vec![
        ".git/index".to_owned(),
        ".git/config".to_owned(),
        ".git/HEAD".to_owned(),
        ".git/objects/pack/pack-0123456789abcdef0123456789abcdef01234567.pack".to_owned(),
        "vendor/lib/.git/index".to_owned(),
        "vendor/lib/.git/config".to_owned(),
        "vendor/lib/.git/HEAD".to_owned(),
        "vendor/lib/.git/objects/pack/pack-0123456789abcdef0123456789abcdef01234567.pack"
            .to_owned(),
    ];
    for pattern in config
        .rules
        .iter()
        .filter(|rule| is_git_syncable_action(rule.action))
        .map(|rule| rule.pattern.as_str())
        .chain(ignore_rules.iter().filter_map(|rule| {
            is_git_syncable_action(rule.rule.action).then_some(rule.pattern.as_str())
        }))
    {
        append_git_probes_for_pattern(pattern, &mut probes);
    }
    probes
}

fn git_structural_syncable_sources(
    config: &fs2_rules::Config,
    ignore_rules: &[fs2_rules::IgnoreRule],
) -> Vec<String> {
    let mut out = Vec::new();
    for (index, rule) in config.rules.iter().enumerate() {
        if is_git_syncable_action(rule.action)
            && structural_git_pattern(&rule.pattern)
            && !config.rules[index + 1..]
                .iter()
                .any(|later| later.pattern == rule.pattern && !is_git_syncable_action(later.action))
        {
            push_unique_probe(&mut out, rule.pattern.clone());
        }
    }
    for (index, rule) in ignore_rules.iter().enumerate() {
        if is_git_syncable_action(rule.rule.action)
            && structural_git_pattern(&rule.pattern)
            && !config.rules.iter().any(|config_rule| {
                config_rule.pattern == rule.pattern && !is_git_syncable_action(config_rule.action)
            })
            && !ignore_rules[index + 1..].iter().any(|later| {
                later.pattern == rule.pattern && !is_git_syncable_action(later.rule.action)
            })
        {
            push_unique_probe(&mut out, rule.pattern.clone());
        }
    }
    out
}

fn structural_git_pattern(pattern: &str) -> bool {
    let cleaned = pattern.trim_start_matches('/');
    let components = cleaned
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    let git_index = components.iter().position(|component| *component == ".git");
    let prefix = git_index.map_or_else(
        || components.join("/"),
        |index| components[..index].join("/"),
    );
    let has_broad_prefix = prefix.contains(['*', '?', '[', '{']);
    let can_reach_git = git_index.is_some()
        || components
            .last()
            .is_some_and(|component| *component == "**")
        || cleaned.ends_with('/')
        || has_broad_prefix;
    can_reach_git && has_broad_prefix
}

fn append_git_probes_for_pattern(pattern: &str, probes: &mut Vec<String>) {
    let cleaned = pattern.trim_start_matches('/');
    let components = cleaned
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    let prefix_components = git_probe_prefix_components(cleaned, &components);
    let Some(concrete_prefixes) = concrete_component_sets(prefix_components) else {
        return;
    };
    if components.contains(&".git") {
        for concrete_probe in concrete_probes_from_git_pattern(&components) {
            push_unique_probe(probes, concrete_probe);
        }
    }
    for concrete_prefix_components in concrete_prefixes {
        let prefix = if concrete_prefix_components.is_empty() {
            String::new()
        } else {
            format!("{}/", concrete_prefix_components.join("/"))
        };
        push_unique_probe(probes, format!("{prefix}.git/index"));
        push_unique_probe(probes, format!("{prefix}.git/config"));
        push_unique_probe(probes, format!("{prefix}.git/HEAD"));
        push_unique_probe(
            probes,
            format!("{prefix}.git/objects/pack/pack-0123456789abcdef0123456789abcdef01234567.pack"),
        );
    }
}

fn git_probe_prefix_components<'a>(cleaned: &str, components: &'a [&str]) -> &'a [&'a str] {
    match components.iter().position(|component| *component == ".git") {
        Some(git_index) => &components[..git_index],
        None if components
            .last()
            .is_some_and(|component| *component == "**") =>
        {
            &components[..components.len() - 1]
        }
        None if cleaned.ends_with('/') => components,
        None => components,
    }
}

fn concrete_probes_from_git_pattern(components: &[&str]) -> Vec<String> {
    concrete_component_sets(components)
        .unwrap_or_default()
        .into_iter()
        .map(|components| components.join("/"))
        .collect()
}

fn concrete_component_sets(components: &[&str]) -> Option<Vec<Vec<String>>> {
    let mut sets = vec![Vec::new()];
    for component in components {
        let samples = concrete_component_samples(component)?;
        let mut next = Vec::new();
        for prefix in &sets {
            for sample in &samples {
                let mut candidate = prefix.clone();
                candidate.push(sample.clone());
                next.push(candidate);
                if next.len() >= 1024 {
                    break;
                }
            }
            if next.len() >= 1024 {
                break;
            }
        }
        sets = next;
    }
    Some(sets)
}

fn concrete_component_samples(component: &str) -> Option<Vec<String>> {
    if component == "**" {
        return Some(vec![
            "fs2doctorprobea".to_owned(),
            "fs2doctorprobeb".to_owned(),
        ]);
    }
    let mut samples = vec![String::new()];
    let mut chars = component.chars();
    while let Some(ch) = chars.next() {
        let alternatives = match ch {
            '\\' => vec![chars.next().unwrap_or('\\').to_string()],
            '*' => vec!["fs2doctorprobea".to_owned(), "fs2doctorprobeb".to_owned()],
            '?' => vec!["a".to_owned(), "b".to_owned()],
            '[' => class_samples(&read_until(&mut chars, ']')?)
                .into_iter()
                .map(|ch| ch.to_string())
                .collect(),
            '{' => {
                let body = read_until(&mut chars, '}')?;
                let mut out = Vec::new();
                for part in body.split(',').filter(|part| !part.is_empty()) {
                    out.extend(concrete_component_samples(part)?);
                    if out.len() >= 64 {
                        break;
                    }
                }
                out
            }
            _ => vec![ch.to_string()],
        };
        if alternatives.is_empty() {
            return None;
        }
        let mut next = Vec::new();
        for sample in &samples {
            for alternative in &alternatives {
                let mut candidate = sample.clone();
                candidate.push_str(alternative);
                next.push(candidate);
                if next.len() >= 64 {
                    break;
                }
            }
            if next.len() >= 64 {
                break;
            }
        }
        samples = next;
    }
    Some(samples)
}

fn read_until(chars: &mut impl Iterator<Item = char>, terminator: char) -> Option<String> {
    let mut out = String::new();
    for ch in chars.by_ref() {
        if ch == terminator {
            return Some(out);
        }
        out.push(ch);
    }
    None
}

fn class_samples(class: &str) -> Vec<char> {
    let mut chars = class.chars();
    let negated = matches!(chars.clone().next(), Some('!' | '^'));
    if negated {
        chars.next();
        let class_chars = chars.collect::<Vec<_>>();
        return "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_-@"
            .chars()
            .filter(|candidate| !class_matches(&class_chars, *candidate))
            .take(8)
            .collect();
    }
    let class_chars = chars.collect::<Vec<_>>();
    let mut out = Vec::new();
    let mut index = 0;
    while index < class_chars.len() {
        let ch = class_chars[index];
        if ch == '-' {
            out.push(ch);
            index += 1;
            continue;
        }
        out.push(ch);
        if class_chars.get(index + 1) == Some(&'-') {
            if let Some(end) = class_chars.get(index + 2).copied() {
                let mut codepoint = ch as u32 + 1;
                while codepoint <= end as u32 && out.len() < 64 {
                    if let Some(next) = char::from_u32(codepoint) {
                        out.push(next);
                    }
                    codepoint += 1;
                }
                index += 3;
                continue;
            }
        }
        index += 1;
    }
    out.sort_unstable();
    out.dedup();
    out.truncate(64);
    if out.is_empty() {
        out.push('a');
    }
    out
}

fn class_matches(class_chars: &[char], candidate: char) -> bool {
    let mut index = 0;
    while index < class_chars.len() {
        let ch = class_chars[index];
        if ch == '-' {
            if candidate == '-' {
                return true;
            }
            index += 1;
            continue;
        }
        if ch == candidate {
            return true;
        }
        if class_chars.get(index + 1) == Some(&'-') {
            if let Some(end) = class_chars.get(index + 2).copied() {
                if ch <= candidate && candidate <= end {
                    return true;
                }
                index += 3;
                continue;
            }
        }
        index += 1;
    }
    false
}

fn push_unique_probe(probes: &mut Vec<String>, probe: String) {
    if !probes.contains(&probe) {
        probes.push(probe);
    }
}

fn describe_rule_source(source: &fs2_rules::RuleSource) -> String {
    match source {
        fs2_rules::RuleSource::ConfigToml { pattern, .. }
        | fs2_rules::RuleSource::Fs2Ignore { pattern, .. }
        | fs2_rules::RuleSource::BuiltinProfile { pattern, .. } => pattern.clone(),
        fs2_rules::RuleSource::CliOverride { path } => format!("cli override for {path}"),
        fs2_rules::RuleSource::WorkspaceDefault { field } => format!("workspace default {field}"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct StatusOutput {
    connection_state: String,
    cursor_lag: u64,
    pending_uploads: u64,
    pending_downloads: u64,
    cache_size_bytes: u64,
    conflicts: u64,
    generated_dirs: Vec<String>,
    env_summary: EnvSummary,
    git_warnings: Vec<String>,
    pending_errors: Vec<PendingErrorStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct EnvSummary {
    total: u64,
    secrets: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PendingErrorStatus {
    workspace: String,
    op_id: String,
    retry_count: u32,
    last_error: String,
}
fn generated_status_paths(path: impl AsRef<Path>) -> Vec<String> {
    let root = path.as_ref();
    let Ok(dependency_roots) = fs2_rules::discover_dependency_roots(root) else {
        return Vec::new();
    };
    let Ok(engine) = status_rule_engine(root) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for dependency_root in dependency_roots {
        let display_root = dependency_display_root(root, &dependency_root.root);
        for generated_path in dependency_root.generated_paths {
            if !dependency_root
                .root
                .join(generated_path.trim_end_matches('/'))
                .is_dir()
            {
                continue;
            }
            let display_path = if display_root == "." {
                generated_path.trim_end_matches('/').to_owned()
            } else {
                format!("{display_root}/{}", generated_path.trim_end_matches('/'))
            };
            let probe = generated_probe_path(&display_root, &generated_path);
            let Ok(workspace_path) = WorkspacePath::parse(&probe) else {
                continue;
            };
            let Ok(resolution) = engine.resolve(
                &workspace_path,
                fs2_rules::RulePathKind::Directory,
                fs2_rules::EvaluationPurpose::NewLocalCreate,
                None,
            ) else {
                continue;
            };
            if generated_status_action(resolution.effective_rule.action) {
                paths.push(display_path);
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn status_rule_engine(root: &Path) -> Result<fs2_rules::RuleEngine, String> {
    let config_path = fs2_config_path(root);
    let config = if config_path.exists() {
        let text = fs::read_to_string(&config_path)
            .map_err(|error| format!("could not read {}: {error}", config_path.display()))?;
        fs2_rules::parse_config_toml(&text).map_err(|error| error.to_string())?
    } else {
        fs2_rules::Config::default()
    };
    let ignore_path = root.join(".fs2ignore");
    let ignore_rules = if ignore_path.exists() {
        let text = fs::read_to_string(&ignore_path)
            .map_err(|error| format!("could not read {}: {error}", ignore_path.display()))?;
        fs2_rules::parse_fs2ignore(&text).map_err(|error| error.to_string())?
    } else {
        Vec::new()
    };
    fs2_rules::RuleEngine::new(config, ignore_rules).map_err(|error| error.to_string())
}

const fn generated_status_action(action: RuleAction) -> bool {
    matches!(
        action,
        RuleAction::Generated | RuleAction::DependencyCache | RuleAction::LocalOnly
    )
}

fn status_for_path(path: impl AsRef<Path>) -> StatusOutput {
    StatusOutput {
        connection_state: "offline".to_owned(),
        cursor_lag: 0,
        pending_uploads: 0,
        pending_downloads: 0,
        cache_size_bytes: 0,
        conflicts: 0,
        generated_dirs: generated_status_paths(&path),
        env_summary: EnvSummary {
            total: 0,
            secrets: 0,
        },
        pending_errors: Vec::new(),
        git_warnings: git_warnings(path),
    }
}

fn status_for_path_with_config(
    path: impl AsRef<Path>,
    config_path: &Path,
) -> Result<StatusOutput, String> {
    let mut status = status_for_path(path);
    for workspace in load_local_workspaces(config_path)? {
        let workspace_id = workspace
            .workspace_id
            .parse::<WorkspaceId>()
            .map_err(|error| error.to_string())?;
        let store = fs2_daemon::LocalStore::open(&workspace.metadata_db)
            .map_err(|error| error.to_string())?;
        let pending = store
            .list_pending_ops(workspace_id)
            .map_err(|error| error.to_string())?;
        status.pending_uploads += pending.len() as u64;
        for pending in pending {
            if let Some(error) = pending.last_error {
                status.pending_errors.push(PendingErrorStatus {
                    workspace: workspace.name.clone(),
                    op_id: pending.operation.op_id.to_string(),
                    retry_count: pending.retry_count,
                    last_error: error,
                });
            }
        }
    }
    Ok(status)
}

fn git_warnings(path: impl AsRef<Path>) -> Vec<String> {
    match fs2_git::detect_repository(path) {
        Ok(status) => {
            let mut warnings = Vec::new();
            if status.is_dirty() {
                warnings.push(format!(
                    "git working tree has {} dirty entries",
                    status.dirty_entries.len()
                ));
            }
            if status.has_submodules() {
                warnings.push(format!(
                    "git repository has {} submodules",
                    status.submodules.len()
                ));
            }
            warnings
        }
        Err(_) => vec!["not inside a git repository".to_owned()],
    }
}
fn debug_bundle(root: &Path, out_path: &Path, config_path: &Path) -> Result<String, String> {
    if let Some(parent) = out_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let file = fs::File::create(out_path)
        .map_err(|error| format!("could not create {}: {error}", out_path.display()))?;
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);

    append_bundle_text(
        &mut archive,
        "README.txt",
        "FS2 debug bundle. All included config, status, and logs are redacted locally before archiving.\n",
    )?;
    let status = if config_status_uses_symlink(config_path) {
        "{\n  \"status\": \"skipped because config path contains a symlink\"\n}".to_owned()
    } else {
        status_json_with_config(root, config_path)
            .unwrap_or_else(|error| format!("{{\n  \"error\": {error:?}\n}}"))
    };
    append_bundle_text(
        &mut archive,
        "status/status.json",
        &redact_config_text(&status),
    )?;
    append_config_file(&mut archive, config_path, "config/config.json")?;
    if let Ok(workspaces_path) = workspace_config_path(config_path) {
        append_config_file(&mut archive, &workspaces_path, "config/workspaces.json")?;
    }
    append_config_file(
        &mut archive,
        &root.join(".fs2").join("config.toml"),
        "workspace/config.toml",
    )?;
    append_config_file(
        &mut archive,
        &root.join(".fs2ignore"),
        "workspace/fs2ignore",
    )?;
    append_log_dir(
        &mut archive,
        &root.join(".fs2").join("logs"),
        "logs/workspace",
    )?;
    if let Some(config_dir) = config_path.parent() {
        append_log_dir(&mut archive, &config_dir.join("logs"), "logs/cli")?;
    }

    archive
        .finish()
        .map_err(|error| format!("could not finalize {}: {error}", out_path.display()))?;
    Ok(format!("Debug bundle written to {}\n", out_path.display()))
}

fn append_config_file(
    archive: &mut tar::Builder<flate2::write::GzEncoder<fs::File>>,
    source: &Path,
    entry_name: &str,
) -> Result<(), String> {
    if path_has_symlink_component(source) {
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return append_bundle_text(
                archive,
                &format!("{entry_name}.error.txt"),
                &format!("could not inspect {}: {error}\n", source.display()),
            );
        }
    };
    if !metadata.is_file() {
        return Ok(());
    }
    let bytes = match fs::read(source) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return append_bundle_text(
                archive,
                &format!("{entry_name}.error.txt"),
                &format!("could not read {}: {error}\n", source.display()),
            );
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let redacted = redact_config_text(&text);
    append_bundle_text(archive, entry_name, &redacted)
}

fn redact_config_text(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text).map_or_else(
        |_| redact_text(text),
        |mut value| {
            redact_json_value(&mut value);
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| redact_text(text)) + "\n"
        },
    )
}

fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if key_is_sensitive(key) {
                    *value = serde_json::Value::String("<redacted>".to_owned());
                } else {
                    redact_json_value(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json_value(value);
            }
        }
        serde_json::Value::String(text) if line_is_sensitive(text) => {
            "<redacted>".clone_into(text);
        }
        _ => {}
    }
}

fn key_is_sensitive(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("passwd")
        || lower.contains("private")
        || lower.contains("authorization")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("_key")
        || lower.contains("-key")
        || lower == "key"
}

fn append_log_dir(
    archive: &mut tar::Builder<flate2::write::GzEncoder<fs::File>>,
    dir: &Path,
    entry_prefix: &str,
) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return append_bundle_text(
                archive,
                &format!("{entry_prefix}/read.error.txt"),
                &format!("could not inspect {}: {error}\n", dir.display()),
            );
        }
    };
    if path_has_symlink_component(dir) || metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return append_bundle_text(
                archive,
                &format!("{entry_prefix}/read.error.txt"),
                &format!("could not read {}: {error}\n", dir.display()),
            );
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            let child_prefix = format!("{entry_prefix}/{}", safe_bundle_name(&entry.file_name()));
            append_log_dir(archive, &path, &child_prefix)?;
        } else if file_type.is_file()
            && path.extension().and_then(|ext| ext.to_str()) == Some("log")
        {
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|error| format!("could not read {}: {error}\n", path.display()));
            append_bundle_text(
                archive,
                &format!("{entry_prefix}/{}", safe_bundle_name(&entry.file_name())),
                &redact_text(&text),
            )?;
        }
    }
    Ok(())
}

fn path_has_symlink_component(path: &Path) -> bool {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return true;
        }
    }
    false
}

fn append_bundle_text(
    archive: &mut tar::Builder<flate2::write::GzEncoder<fs::File>>,
    entry_name: &str,
    text: &str,
) -> Result<(), String> {
    let bytes = text.as_bytes();
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    archive
        .append_data(&mut header, entry_name, bytes)
        .map_err(|error| format!("could not append {entry_name}: {error}"))
}

fn safe_bundle_name(name: &std::ffi::OsStr) -> String {
    let name = name.to_string_lossy();
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

struct SensitiveBlock {
    end: char,
    depth: i32,
    nested_multiline: Option<char>,
}

fn redact_text(text: &str) -> String {
    let mut out = String::new();
    let mut sensitive_block: Option<SensitiveBlock> = None;
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(block) = sensitive_block.as_mut() {
            out.push_str("<redacted>\n");
            if let Some(quote) = block.nested_multiline {
                if multiline_quote_closes(quote, &lower) {
                    block.nested_multiline = None;
                }
            } else if (block.end == 'P' && pem_private_key_end(&lower))
                || (block.end == 'D' && has_unescaped_triple_double_quote(&lower))
                || (block.end == 'S' && lower.contains("'''"))
            {
                sensitive_block = None;
            } else if matches!(block.end, ']' | '}' | ')') {
                if let Some(quote) = unclosed_multiline_quote_on_line(&lower) {
                    block.nested_multiline = Some(quote);
                } else {
                    block.depth += delimiter_depth_delta(&lower, block.end);
                    if block.depth <= 0 {
                        sensitive_block = None;
                    }
                }
            }
            continue;
        }
        if line_is_sensitive(line) {
            sensitive_block = sensitive_multiline_block(&lower);
            if let Some((prefix, value)) = line.split_once(':') {
                out.push_str(&redact_colon_value(prefix, value));
                out.push('\n');
            } else if let Some((prefix, _)) = line.split_once('=') {
                out.push_str(prefix);
                out.push_str("=<redacted>\n");
            } else {
                out.push_str("<redacted>\n");
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn sensitive_multiline_block(lower: &str) -> Option<SensitiveBlock> {
    if pem_private_key_start(lower) {
        return Some(SensitiveBlock {
            end: 'P',
            depth: 1,
            nested_multiline: None,
        });
    }

    let mut best = if count_unescaped_triple_double_quotes(lower) % 2 == 1 {
        lower.find("\"\"\"").map(|index| (index, 'D', 1))
    } else {
        None
    };
    if lower.matches("'''").count() % 2 == 1 {
        let literal = lower.find("'''").map(|index| (index, 'S', 1));
        best = earliest_sensitive_block(best, literal);
    }

    for (start, end) in [('[', ']'), ('{', '}'), ('(', ')')] {
        let depth = delimiter_depth_delta(lower, end);
        if depth > 0 {
            let container = Some((lower.find(start).unwrap_or(usize::MAX), end, depth));
            best = earliest_sensitive_block(best, container);
        }
    }

    best.map(|(_, end, depth)| SensitiveBlock {
        end,
        depth,
        nested_multiline: matches!(end, ']' | '}' | ')')
            .then(|| unclosed_multiline_quote_on_line(lower))
            .flatten(),
    })
}

const fn earliest_sensitive_block(
    current: Option<(usize, char, i32)>,
    candidate: Option<(usize, char, i32)>,
) -> Option<(usize, char, i32)> {
    match (current, candidate) {
        (Some(current), Some(candidate)) if candidate.0 < current.0 => Some(candidate),
        (Some(current), _) => Some(current),
        (None, candidate) => candidate,
    }
}

fn unclosed_multiline_quote_on_line(line: &str) -> Option<char> {
    if count_unescaped_triple_double_quotes(line) % 2 == 1 {
        Some('D')
    } else if line.matches("'''").count() % 2 == 1 {
        Some('S')
    } else {
        None
    }
}

fn multiline_quote_closes(quote: char, line: &str) -> bool {
    match quote {
        'D' => has_unescaped_triple_double_quote(line),
        'S' => line.contains("'''"),
        _ => false,
    }
}

fn has_unescaped_triple_double_quote(line: &str) -> bool {
    count_unescaped_triple_double_quotes(line) > 0
}

fn count_unescaped_triple_double_quotes(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut index = 0;
    let mut count = 0;
    while let Some(relative) = line[index..].find("\"\"\"") {
        let candidate = index + relative;
        let preceding_backslashes = bytes[..candidate]
            .iter()
            .rev()
            .take_while(|&&byte| byte == b'\\')
            .count();
        if preceding_backslashes % 2 == 0 {
            count += 1;
        }
        index = candidate + 3;
    }
    count
}

fn delimiter_depth_delta(line: &str, end: char) -> i32 {
    let start = match end {
        ']' => '[',
        '}' => '{',
        ')' => '(',
        _ => return 0,
    };
    let mut depth = 0;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for ch in line.chars() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }
        if ch == '#' {
            break;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
        } else if ch == start {
            depth += 1;
        } else if ch == end {
            depth -= 1;
        }
    }
    depth
}

fn config_status_uses_symlink(config_path: &Path) -> bool {
    path_has_symlink_component(config_path)
        || workspace_config_path(config_path).is_ok_and(|path| path_has_symlink_component(&path))
}

fn redact_colon_value(prefix: &str, value: &str) -> String {
    if prefix.trim_start().starts_with('"') {
        let comma = if value.trim_end().ends_with(',') {
            ","
        } else {
            ""
        };
        format!("{prefix}: \"<redacted>\"{comma}")
    } else {
        format!("{prefix}: <redacted>")
    }
}

fn pem_private_key_start(lower: &str) -> bool {
    lower.contains("begin") && lower.contains("private key")
}

fn pem_private_key_end(lower: &str) -> bool {
    lower.contains("end") && lower.contains("private key")
}

fn line_is_sensitive(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("passwd")
        || lower.contains("private")
        || lower.contains("authorization")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("_key")
        || lower.contains("-key")
        || lower.contains("bearer ")
        || lower.split_whitespace().any(field_key_is_sensitive)
        || has_spaced_sensitive_key_field(&lower)
}

fn field_key_is_sensitive(field: &str) -> bool {
    field
        .split_once('=')
        .or_else(|| field.split_once(':'))
        .is_some_and(|(key, _)| {
            key_is_sensitive(key.trim_matches(|ch: char| ch == '"' || ch == '\''))
        })
}

fn has_spaced_sensitive_key_field(line: &str) -> bool {
    let fields = line.split_whitespace().collect::<Vec<_>>();
    fields.windows(2).any(|window| {
        key_is_sensitive(window[0].trim_matches(|ch: char| ch == '"' || ch == '\''))
            && matches!(window[1], "=" | ":")
    })
}

fn status_json_with_config(path: impl AsRef<Path>, config_path: &Path) -> Result<String, String> {
    serde_json::to_string_pretty(&status_for_path_with_config(path, config_path)?)
        .map_err(|error| error.to_string())
}

fn status_text_with_config(path: impl AsRef<Path>, config_path: &Path) -> Result<String, String> {
    render_status_text(&status_for_path_with_config(path, config_path)?)
}

fn render_status_text(status: &StatusOutput) -> Result<String, String> {
    let mut out = String::new();
    writeln!(out, "FS2 status:").map_err(|error| error.to_string())?;
    writeln!(out, "  connection: {}", status.connection_state)
        .map_err(|error| error.to_string())?;
    writeln!(out, "  cursor lag: {}", status.cursor_lag).map_err(|error| error.to_string())?;
    writeln!(out, "  pending uploads: {}", status.pending_uploads)
        .map_err(|error| error.to_string())?;
    writeln!(out, "  pending downloads: {}", status.pending_downloads)
        .map_err(|error| error.to_string())?;
    writeln!(out, "  cache size: {} bytes", status.cache_size_bytes)
        .map_err(|error| error.to_string())?;
    writeln!(out, "  conflicts: {}", status.conflicts).map_err(|error| error.to_string())?;
    if status.generated_dirs.is_empty() {
        out.push_str("  generated dirs: none\n");
    } else {
        out.push_str("  generated dirs:\n");
        for path in &status.generated_dirs {
            writeln!(out, "    - {path}").map_err(|error| error.to_string())?;
        }
    }
    writeln!(
        out,
        "  env: {} records ({} secrets)",
        status.env_summary.total, status.env_summary.secrets
    )
    .map_err(|error| error.to_string())?;
    if status.pending_errors.is_empty() {
        out.push_str("  pending errors: none\n");
    } else {
        out.push_str("  pending errors:\n");
        for error in &status.pending_errors {
            writeln!(
                out,
                "    - {} {} (retries={}): {}",
                error.workspace, error.op_id, error.retry_count, error.last_error
            )
            .map_err(|error| error.to_string())?;
        }
    }
    if status.git_warnings.is_empty() {
        out.push_str("  git warnings: none\n");
    } else {
        out.push_str("  git warnings:\n");
        for warning in &status.git_warnings {
            writeln!(out, "    - {warning}").map_err(|error| error.to_string())?;
        }
    }
    Ok(out)
}

fn git_status(path: impl AsRef<Path>) -> Result<String, String> {
    let status = fs2_git::detect_repository(path).map_err(|error| error.to_string())?;
    let mut out = String::new();
    out.push_str("Git:\n");
    writeln!(out, "  root: {}", status.worktree_root.display())
        .map_err(|error| error.to_string())?;
    writeln!(out, "  kind: {:?}", status.kind).map_err(|error| error.to_string())?;
    writeln!(
        out,
        "  branch: {}",
        status.current_branch.as_deref().unwrap_or("detached")
    )
    .map_err(|error| error.to_string())?;
    writeln!(
        out,
        "  head: {}",
        status.head_commit.as_deref().unwrap_or("unborn")
    )
    .map_err(|error| error.to_string())?;
    if status.remotes.is_empty() {
        out.push_str("  remotes: none\n");
    } else {
        out.push_str("  remotes:\n");
        for remote in status.remotes {
            writeln!(out, "    {}: {}", remote.name, remote.url)
                .map_err(|error| error.to_string())?;
        }
    }
    if status.dirty_entries.is_empty() {
        out.push_str("  dirty: clean\n");
    } else {
        writeln!(out, "  dirty: {} entries", status.dirty_entries.len())
            .map_err(|error| error.to_string())?;
        for entry in status.dirty_entries {
            writeln!(out, "    {} {}", entry.code, entry.path)
                .map_err(|error| error.to_string())?;
        }
    }
    if status.submodules.is_empty() {
        out.push_str("  submodules: none\n");
    } else {
        out.push_str("  submodules:\n");
        for submodule in status.submodules {
            writeln!(
                out,
                "    {} ({}) url={} commit={}",
                submodule.path,
                submodule.name,
                submodule.url.as_deref().unwrap_or("unknown"),
                submodule.commit.as_deref().unwrap_or("unknown")
            )
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(out)
}

fn git_submodules_status(path: impl AsRef<Path>) -> Result<String, String> {
    let status = fs2_git::detect_repository(path).map_err(|error| error.to_string())?;
    let mut out = String::new();
    out.push_str("Git submodules:\n");
    if status.submodules.is_empty() {
        out.push_str("  none\n");
    } else {
        for submodule in status.submodules {
            writeln!(
                out,
                "  {} name={} url={} commit={}",
                submodule.path,
                submodule.name,
                submodule.url.as_deref().unwrap_or("unknown"),
                submodule.commit.as_deref().unwrap_or("unknown")
            )
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, sync::Mutex, thread};

    type MultiRequestServer = (
        String,
        thread::JoinHandle<Result<Vec<String>, std::io::Error>>,
    );

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[derive(Debug, Default)]
    struct FakeCredentialStore {
        access_token: Mutex<Option<String>>,
        refresh_token: Mutex<Option<String>>,
    }

    impl CredentialStore for FakeCredentialStore {
        fn set_access_token(&self, token: &str) -> Result<(), String> {
            *self
                .access_token
                .lock()
                .map_err(|error| error.to_string())? = Some(token.to_owned());
            Ok(())
        }

        fn set_refresh_token(&self, token: &str) -> Result<(), String> {
            *self
                .refresh_token
                .lock()
                .map_err(|error| error.to_string())? = Some(token.to_owned());
            Ok(())
        }

        fn get_access_token(&self) -> Result<Option<String>, String> {
            self.access_token
                .lock()
                .map(|token| token.clone())
                .map_err(|error| error.to_string())
        }

        fn delete_tokens(&self) -> Result<(), String> {
            *self
                .access_token
                .lock()
                .map_err(|error| error.to_string())? = None;
            *self
                .refresh_token
                .lock()
                .map_err(|error| error.to_string())? = None;
            Ok(())
        }
    }

    impl FakeCredentialStore {
        fn get_refresh_token(&self) -> Result<Option<String>, String> {
            self.refresh_token
                .lock()
                .map(|token| token.clone())
                .map_err(|error| error.to_string())
        }
    }

    #[derive(Debug, Default)]
    struct FailingAccessCredentialStore {
        inner: FakeCredentialStore,
    }

    impl CredentialStore for FailingAccessCredentialStore {
        fn set_access_token(&self, _token: &str) -> Result<(), String> {
            Err("access token write failed".to_owned())
        }

        fn set_refresh_token(&self, token: &str) -> Result<(), String> {
            self.inner.set_refresh_token(token)
        }

        fn get_access_token(&self) -> Result<Option<String>, String> {
            self.inner.get_access_token()
        }

        fn delete_tokens(&self) -> Result<(), String> {
            self.inner.delete_tokens()
        }
    }

    impl FailingAccessCredentialStore {
        fn get_refresh_token(&self) -> Result<Option<String>, String> {
            self.inner.get_refresh_token()
        }
    }

    fn serve_json_once(
        body: &'static str,
    ) -> Result<(String, thread::JoinHandle<Result<String, std::io::Error>>), std::io::Error> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", listener.local_addr()?);
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept()?;
            let mut request_bytes = Vec::new();
            loop {
                let mut buffer = [0_u8; 1024];
                let read = stream.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                request_bytes.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request_bytes
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request_bytes[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                if request_bytes.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            let request_text = String::from_utf8_lossy(&request_bytes).into_owned();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )?;
            Ok(request_text)
        });
        Ok((url, handle))
    }

    fn serve_json_times(
        body: &'static str,
        count: usize,
    ) -> Result<MultiRequestServer, std::io::Error> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", listener.local_addr()?);
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..count {
                let (mut stream, _) = listener.accept()?;
                let mut request_bytes = Vec::new();
                loop {
                    let mut buffer = [0_u8; 1024];
                    let read = stream.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    request_bytes.extend_from_slice(&buffer[..read]);
                    let Some(header_end) = request_bytes
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&request_bytes[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("Content-Length: "))
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap_or(0);
                    if request_bytes.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
                if request_bytes.is_empty() {
                    continue;
                }
                requests.push(String::from_utf8_lossy(&request_bytes).into_owned());
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )?;
            }
            Ok(requests)
        });
        Ok((url, handle))
    }

    #[test]
    fn login_stores_config_and_redacts_token() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("config.json");
        let credentials = FakeCredentialStore::default();
        let (backend, server) = serve_json_once(
            r#"{"access_token":"super-secret-token","refresh_token":"refresh-secret-token","token_type":"Bearer","user_id":"user-1","device_id":"device-1","warning":"development-only auth"}"#,
        )?;

        let output = run_from_args_with_context(
            [
                "login".to_owned(),
                "--backend".to_owned(),
                backend.clone(),
                "--device-name".to_owned(),
                "laptop".to_owned(),
            ],
            &credentials,
            &config_path,
        )?;
        let request = server
            .join()
            .map_err(|_| std::io::Error::other("server thread panicked"))??;

        assert!(request.starts_with("POST /v1/auth/dev-login HTTP/1.1"));
        assert!(request.contains("\"device_name\":\"laptop\""));
        assert!(output.contains("Logged in to"));
        assert!(output.contains("access token: <redacted:18 bytes>"));
        assert!(!output.contains("super-secret-token"));
        assert_eq!(
            credentials.get_access_token()?.as_deref(),
            Some("super-secret-token")
        );
        assert_eq!(
            credentials.get_refresh_token()?.as_deref(),
            Some("refresh-secret-token")
        );
        assert_eq!(
            load_cli_config(&config_path)?,
            CliConfig {
                backend_url: backend,
                user_id: "user-1".to_owned(),
                device_id: "device-1".to_owned(),
                device_name: "laptop".to_owned(),
                token_type: "Bearer".to_owned(),
            }
        );
        Ok(())
    }

    #[test]
    fn failed_config_write_leaves_no_tokens() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let blocked_parent = dir.path().join("not-a-directory");
        fs::write(&blocked_parent, "file blocks config directory")?;
        let config_path = blocked_parent.join("config.json");
        let credentials = FakeCredentialStore::default();
        let (backend, server) = serve_json_once(
            r#"{"access_token":"super-secret-token","refresh_token":"refresh-secret-token","token_type":"Bearer","user_id":"user-1","device_id":"device-1","warning":"development-only auth"}"#,
        )?;

        let result = run_from_args_with_context(
            ["login".to_owned(), "--backend".to_owned(), backend],
            &credentials,
            &config_path,
        );
        let _request = server
            .join()
            .map_err(|_| std::io::Error::other("server thread panicked"))??;

        assert!(result.is_err());
        assert_eq!(credentials.get_access_token()?, None);
        assert_eq!(credentials.get_refresh_token()?, None);
        Ok(())
    }

    #[test]
    fn failed_access_token_write_clears_existing_tokens() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("config.json");
        let credentials = FailingAccessCredentialStore::default();
        credentials.inner.set_access_token("old-access")?;
        credentials.inner.set_refresh_token("old-refresh")?;
        let (backend, server) = serve_json_once(
            r#"{"access_token":"super-secret-token","refresh_token":"refresh-secret-token","token_type":"Bearer","user_id":"user-1","device_id":"device-1","warning":"development-only auth"}"#,
        )?;

        let result = run_from_args_with_context(
            ["login".to_owned(), "--backend".to_owned(), backend],
            &credentials,
            &config_path,
        );
        let _request = server
            .join()
            .map_err(|_| std::io::Error::other("server thread panicked"))??;

        assert!(result.is_err());
        assert_eq!(credentials.get_access_token()?, None);
        assert_eq!(credentials.get_refresh_token()?, None);
        assert!(!config_path.exists());
        Ok(())
    }

    #[test]
    fn device_list_uses_stored_bearer_token() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("config.json");
        let credentials = FakeCredentialStore::default();
        credentials.set_access_token("token-123")?;
        let (backend, server) = serve_json_once(
            r#"{"devices":[{"device_id":"device-1","user_id":"user-1","name":"laptop","platform":{},"public_key":"pk","revoked":false}]}"#,
        )?;
        save_cli_config(
            &config_path,
            &CliConfig {
                backend_url: backend,
                user_id: "user-1".to_owned(),
                device_id: "device-1".to_owned(),
                device_name: "laptop".to_owned(),
                token_type: "Bearer".to_owned(),
            },
        )?;

        let output = run_from_args_with_context(
            ["device".to_owned(), "list".to_owned()],
            &credentials,
            &config_path,
        )?;
        let request = server
            .join()
            .map_err(|_| std::io::Error::other("server thread panicked"))??;

        assert!(request.starts_with("GET /v1/devices HTTP/1.1"));
        assert!(request.contains("Authorization: Bearer token-123"));
        assert!(output.contains("device-1 laptop (current)"));
        Ok(())
    }

    #[test]
    fn workspace_init_creates_remote_and_local_metadata() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("config.json");
        let workspace_path = dir.path().join("code");
        let credentials = FakeCredentialStore::default();
        credentials.set_access_token("token-123")?;
        let (backend, server) = serve_json_once(
            r#"{"workspace_id":"00000000-0000-0000-0000-000000000101","root_node_id":"00000000-0000-0000-0000-000000000102","current_cursor":0}"#,
        )?;
        save_cli_config(
            &config_path,
            &CliConfig {
                backend_url: backend,
                user_id: "user-1".to_owned(),
                device_id: "device-1".to_owned(),
                device_name: "laptop".to_owned(),
                token_type: "Bearer".to_owned(),
            },
        )?;

        let output = run_from_args_with_context(
            [
                "workspace".to_owned(),
                "init".to_owned(),
                workspace_path.display().to_string(),
                "--name".to_owned(),
                "team \"alpha\"".to_owned(),
            ],
            &credentials,
            &config_path,
        )?;
        let request = server
            .join()
            .map_err(|_| std::io::Error::other("server thread panicked"))??;

        assert!(request.starts_with("POST /v1/workspaces HTTP/1.1"));
        assert!(request.contains("Authorization: Bearer token-123"));
        assert!(request.contains("\"name\":\"team \\\"alpha\\\"\""));
        assert!(output.contains("Workspace created: team \"alpha\""));
        let marker = fs::read_to_string(workspace_path.join(".fs2").join("config.toml"))?;
        let parsed_marker = fs2_rules::parse_config_toml(&marker)?;
        assert_eq!(
            parsed_marker.workspace_name.as_deref(),
            Some("team \"alpha\"")
        );
        let workspaces = load_local_workspaces(&config_path)?;
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].name, "team \"alpha\"");
        assert_eq!(
            workspaces[0].path,
            Some(workspace_path.display().to_string())
        );
        let workspace_id = workspaces[0].workspace_id.parse::<WorkspaceId>()?;
        let store = fs2_daemon::LocalStore::open(&workspaces[0].metadata_db)?;
        assert!(store.get_node_by_path(workspace_id, "")?.is_some());
        Ok(())
    }

    #[test]
    fn workspace_list_marks_local_entries_and_mount_records_path(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("config.json");
        let credentials = FakeCredentialStore::default();
        credentials.set_access_token("token-123")?;
        let (backend, server) = serve_json_once(
            r#"{"workspaces":[{"workspace_id":"00000000-0000-0000-0000-000000000201","name":"personal-code","root_node_id":"00000000-0000-0000-0000-000000000202","current_cursor":0}]}"#,
        )?;
        save_cli_config(
            &config_path,
            &CliConfig {
                backend_url: backend,
                user_id: "user-1".to_owned(),
                device_id: "device-1".to_owned(),
                device_name: "laptop".to_owned(),
                token_type: "Bearer".to_owned(),
            },
        )?;
        save_local_workspaces(
            &config_path,
            &[LocalWorkspaceConfig {
                workspace_id: "00000000-0000-0000-0000-000000000201".to_owned(),
                name: "personal-code".to_owned(),
                root_node_id: "00000000-0000-0000-0000-000000000202".to_owned(),
                metadata_db: dir.path().join("metadata.sqlite").display().to_string(),
                path: None,
                mount_path: None,
            }],
        )?;

        let output = run_from_args_with_context(
            ["workspace".to_owned(), "list".to_owned()],
            &credentials,
            &config_path,
        )?;
        let request = server
            .join()
            .map_err(|_| std::io::Error::other("server thread panicked"))??;
        assert!(request.starts_with("GET /v1/workspaces HTTP/1.1"));
        assert!(output
            .contains("personal-code root=00000000-0000-0000-0000-000000000202 cursor=0 local"));

        let mount_path = dir.path().join("mnt");
        let mount_output = run_from_args_with_context(
            [
                "mount".to_owned(),
                "personal-code".to_owned(),
                mount_path.display().to_string(),
            ],
            &credentials,
            &config_path,
        )?;
        assert!(mount_output.contains("Mount placeholder recorded"));
        assert_eq!(
            load_local_workspaces(&config_path)?[0].mount_path,
            Some(mount_path.display().to_string())
        );
        Ok(())
    }

    #[test]
    fn workspace_mount_prefers_ids_and_rejects_ambiguous_names(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("config.json");
        save_local_workspaces(
            &config_path,
            &[
                LocalWorkspaceConfig {
                    workspace_id: "00000000-0000-0000-0000-000000000301".to_owned(),
                    name: "00000000-0000-0000-0000-000000000302".to_owned(),
                    root_node_id: "00000000-0000-0000-0000-000000000303".to_owned(),
                    metadata_db: dir.path().join("first.sqlite").display().to_string(),
                    path: None,
                    mount_path: None,
                },
                LocalWorkspaceConfig {
                    workspace_id: "00000000-0000-0000-0000-000000000302".to_owned(),
                    name: "duplicate".to_owned(),
                    root_node_id: "00000000-0000-0000-0000-000000000304".to_owned(),
                    metadata_db: dir.path().join("second.sqlite").display().to_string(),
                    path: None,
                    mount_path: None,
                },
                LocalWorkspaceConfig {
                    workspace_id: "00000000-0000-0000-0000-000000000305".to_owned(),
                    name: "duplicate".to_owned(),
                    root_node_id: "00000000-0000-0000-0000-000000000306".to_owned(),
                    metadata_db: dir.path().join("third.sqlite").display().to_string(),
                    path: None,
                    mount_path: None,
                },
            ],
        )?;

        let id_mount = dir.path().join("id-mount");
        run_from_args_with_context(
            [
                "mount".to_owned(),
                "00000000-0000-0000-0000-000000000302".to_owned(),
                id_mount.display().to_string(),
            ],
            &FakeCredentialStore::default(),
            &config_path,
        )?;
        let workspaces = load_local_workspaces(&config_path)?;
        assert_eq!(workspaces[0].mount_path, None);
        assert_eq!(
            workspaces[1].mount_path,
            Some(id_mount.display().to_string())
        );

        let ambiguous = run_from_args_with_context(
            [
                "mount".to_owned(),
                "duplicate".to_owned(),
                dir.path().join("ambiguous").display().to_string(),
            ],
            &FakeCredentialStore::default(),
            &config_path,
        );
        assert!(matches!(ambiguous, Err(error) if error.contains("ambiguous")));
        Ok(())
    }

    #[test]
    fn logout_clears_token_and_config() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("config.json");
        let credentials = FakeCredentialStore::default();
        credentials.set_access_token("token-123")?;
        save_cli_config(
            &config_path,
            &CliConfig {
                backend_url: "http://127.0.0.1:3000".to_owned(),
                user_id: "user-1".to_owned(),
                device_id: "device-1".to_owned(),
                device_name: "laptop".to_owned(),
                token_type: "Bearer".to_owned(),
            },
        )?;

        let output = run_from_args_with_context(["logout".to_owned()], &credentials, &config_path)?;

        assert_eq!(output, "Logged out\n");
        assert_eq!(credentials.get_access_token()?, None);
        assert_eq!(credentials.get_refresh_token()?, None);
        assert!(!config_path.exists());
        Ok(())
    }

    #[test]
    fn help_lists_status_and_git_status() -> Result<(), String> {
        let output = run_from_args(["--help".to_owned()])?;

        assert!(output.contains("fs2 login --backend <url> [--device-name <name>]"));
        assert!(output.contains("fs2 status [--json]"));
        assert!(output.contains("fs2 doctor [path]"));
        assert!(output.contains("fs2 deps status <path>"));
        assert!(output.contains("fs2 deps install <path> [--yes]"));
        assert!(output.contains("fs2 git status [path]"));
        assert!(output.contains("fs2 git submodules status [path]"));
        Ok(())
    }

    #[test]
    fn debug_bundle_includes_status_config_logs_and_redacts(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("config.json");
        let out_path = dir.path().join("bundle.tar.gz");
        let workspace_logs = dir.path().join(".fs2").join("logs");
        let cli_logs = dir.path().join("logs");
        fs::create_dir_all(&workspace_logs)?;
        fs::create_dir_all(&cli_logs)?;
        fs::create_dir_all(dir.path().join(".fs2"))?;
        fs::write(
            workspace_logs.join("fs2d.log"),
            "normal event\nlevel=INFO access_token=later-secret\nprivate_key = \"\"\"\nblock-secret\n\"\"\"\naccess_token: super-secret-token\nAuthorization: Bearer hidden-token\n",
        )?;
        fs::write(cli_logs.join("cli.log"), "refresh_token=refresh-secret\n")?;
        fs::write(
            dir.path().join(".fs2").join("config.toml"),
            "normal = true\napi_secret = \"config-secret\"\n",
        )?;
        fs::write(dir.path().join(".fs2ignore"), ":secret .env\n")?;
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir()?;
            fs::write(
                outside.path().join("external.log"),
                "API_KEY=external-secret\n",
            )?;
            std::os::unix::fs::symlink(
                outside.path().join("external.log"),
                workspace_logs.join("external.log"),
            )?;
        }
        save_cli_config(
            &config_path,
            &CliConfig {
                backend_url: "http://127.0.0.1:9".to_owned(),
                user_id: "user-1".to_owned(),
                device_id: "00000000-0000-0000-0000-000000000101".to_owned(),
                device_name: "device-1".to_owned(),
                token_type: "Bearer".to_owned(),
            },
        )?;
        fs::write(
            dir.path().join("workspaces.json"),
            "{\"tokens\":[\"json-secret\"],\"nested\":{\"api_key\":\"nested-secret\"},\"note\":\"access_token=note-secret\",\"name\":\"safe\"}",
        )?;

        let output = run_from_args_with_context(
            [
                "debug".to_owned(),
                "bundle".to_owned(),
                dir.path().display().to_string(),
                "--out".to_owned(),
                out_path.display().to_string(),
            ],
            &FakeCredentialStore::default(),
            &config_path,
        )?;

        assert!(output.contains("Debug bundle written to"));
        let file = fs::File::open(&out_path)?;
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        let mut entries = std::collections::BTreeMap::new();
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.display().to_string();
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            entries.insert(path, text);
        }
        assert!(entries.contains_key("status/status.json"));
        assert!(entries.contains_key("config/config.json"));
        assert!(entries.contains_key("workspace/config.toml"));
        assert!(entries.contains_key("logs/workspace/fs2d.log"));
        assert!(entries.contains_key("logs/cli/cli.log"));
        let config_json = entries.get("config/config.json").ok_or("missing config")?;
        serde_json::from_str::<serde_json::Value>(config_json)?;
        assert!(!config_json.contains("Bearer"));
        let workspaces_json = entries
            .get("config/workspaces.json")
            .ok_or("missing workspaces config")?;
        serde_json::from_str::<serde_json::Value>(workspaces_json)?;
        assert!(workspaces_json.contains("safe"));
        assert!(!workspaces_json.contains("json-secret"));
        assert!(!workspaces_json.contains("nested-secret"));
        assert!(!workspaces_json.contains("note-secret"));
        assert_eq!(redact_config_text("\"secret-token\"\n"), "\"<redacted>\"\n");
        let combined = entries.values().cloned().collect::<String>();
        assert!(combined.contains("<redacted>"));
        assert!(!combined.contains("super-secret-token"));
        assert!(!combined.contains("later-secret"));
        assert!(!combined.contains("block-secret"));
        assert!(!combined.contains("hidden-token"));
        assert!(!combined.contains("refresh-secret"));
        assert!(!combined.contains("config-secret"));
        assert!(!combined.contains("external-secret"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn debug_bundle_skips_symlinked_log_root() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let config_path = dir.path().join("config.json");
        let out_path = dir.path().join("bundle.tar.gz");
        let outside_fs2 = outside.path().join("fs2-external");
        fs::create_dir_all(outside_fs2.join("logs"))?;
        fs::write(
            outside_fs2.join("logs").join("external.log"),
            "API_KEY=external-secret\n",
        )?;
        fs::write(
            outside.path().join("config.json"),
            "{\"API_KEY\":\"config-symlink-secret\"}",
        )?;
        std::os::unix::fs::symlink(&outside_fs2, dir.path().join(".fs2"))?;
        std::os::unix::fs::symlink(outside.path().join("config.json"), &config_path)?;

        run_from_args_with_context(
            [
                "debug".to_owned(),
                "bundle".to_owned(),
                dir.path().display().to_string(),
                "--out".to_owned(),
                out_path.display().to_string(),
            ],
            &FakeCredentialStore::default(),
            &config_path,
        )?;

        let file = fs::File::open(&out_path)?;
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        let mut combined = String::new();
        for entry in archive.entries()? {
            let mut entry = entry?;
            entry.read_to_string(&mut combined)?;
        }
        assert!(!combined.contains("external-secret"));
        assert!(!combined.contains("config-symlink-secret"));
        Ok(())
    }

    #[test]
    fn redaction_handles_common_secret_spellings() {
        let redacted = redact_text(
            "private_key: abc\nAuthorization: Bearer token\nplain=value\nSECRET_TOKEN=value\nAPI_KEY=sk_live\npassword=hunter2\ndevice_token=dtok\nlevel=INFO access_token=later\nlevel=INFO key=generic-key-secret\nlevel=INFO key = spaced-key-secret\naccess_tokens = [\n\"array-token\"\n]\naccess_token = { \"items\": [\n\"inner-token\"\n],\n\"refresh\": \"outer-token\"\n}\naccess_token = {\n\"marker\": \"}\",\n\"value\": \"quoted-delimiter-token\"\n}\naccess_token = {\n\"nested\": {\n\"ignored\": true\n},\n\"value\": \"nested-token\"\n}\naccess_token = '''\nliteral-token\n\"\"\"\nafter-double-triple-token\n'''\n\"tokens\": [\n\"json-token\"\n]\nprivate_key = \"\"\"\nblock-body\n\"\"\"\n-----BEGIN PRIVATE KEY-----\npem-body\n-----END PRIVATE KEY-----\n-----BEGIN RSA PRIVATE KEY-----\nrsa-body\n-----END RSA PRIVATE KEY-----\n-----BEGIN OPENSSH PRIVATE KEY-----\nopenssh-body\n-----END OPENSSH PRIVATE KEY-----\n",
        );

        assert!(redacted.contains("private_key: <redacted>"));
        assert!(redacted.contains("Authorization: <redacted>"));
        assert!(redacted.contains("plain=value"));
        assert!(!redacted.contains("Bearer token"));
        assert!(!redacted.contains("SECRET_TOKEN=value"));
        assert!(!redacted.contains("API_KEY=sk_live"));
        assert!(!redacted.contains("password=hunter2"));
        assert!(!redacted.contains("device_token=dtok"));
        assert!(!redacted.contains("access_token=later"));
        assert!(!redacted.contains("after-double-triple-token"));
        assert!(!redacted.contains("generic-key-secret"));
        assert!(!redacted.contains("spaced-key-secret"));
        assert!(!redacted.contains("array-token"));
        assert!(!redacted.contains("json-token"));
        assert!(!redacted.contains("nested-token"));
        assert!(!redacted.contains("inner-token"));
        assert!(!redacted.contains("outer-token"));
        assert!(!redacted.contains("quoted-delimiter-token"));
        assert!(!redacted.contains("literal-token"));
        assert!(!redacted.contains("block-body"));
        assert!(!redacted.contains("pem-body"));
        assert!(!redacted.contains("rsa-body"));
        assert!(!redacted.contains("openssh-body"));
        let escaped_triple = redact_text(
            r#"api_key = """
block
\"""
after-escaped-triple-token
"""
"#,
        );
        assert!(!escaped_triple.contains("after-escaped-triple-token"));
        let escaped_open = redact_text(
            r#"api_key = """prefix \"""
after-escaped-open-token
"""
"#,
        );
        assert!(!escaped_open.contains("after-escaped-open-token"));
        let multiline_container = redact_text(
            r#"access_tokens = ["""
inside-string
]
""",
after-string-token
]
"#,
        );
        assert!(!multiline_container.contains("after-string-token"));
        let comment_container = redact_text(
            r"access_tokens = [
# ] comment delimiter
comment-hidden-token
]
",
        );
        assert!(!comment_container.contains("comment-hidden-token"));
    }

    #[test]
    fn deps_status_reports_install_state_and_lockfile_changes(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let _guard = env_guard();
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"pnpm@9.0.0"}"#,
        )?;
        fs::write(dir.path().join("pnpm-lock.yaml"), "lock-v1")?;
        let state_dir = tempfile::tempdir()?;
        env::set_var("FS2_STATE_DIR", state_dir.path());

        let output = deps_status(dir.path())?;
        assert!(output.contains(".: node via `pnpm install`; not installed by fs2"));
        assert!(output.contains("generated: node_modules/"));

        let roots = fs2_rules::detect_dependency_roots_at(dir.path())?;
        let hash = dependency_lock_hash(&roots[0])?.ok_or("missing lock hash")?;
        let mut state = BTreeMap::new();
        state.insert(dependency_state_key(".", roots[0].ecosystem), hash);
        write_dependency_install_state(dir.path(), &state)?;
        let state_path = dependency_install_state_path(dir.path());
        assert!(!state_path.starts_with(dir.path()));
        assert!(state_path.starts_with(state_dir.path()));
        assert_eq!(
            dependency_install_state_path(dir.path()),
            dependency_install_state_path(&dir.path().canonicalize()?)
        );
        assert!(deps_status(dir.path())?.contains("; installed"));

        fs::write(dir.path().join("pnpm-lock.yaml"), "lock-v2")?;
        assert!(deps_status(dir.path())?.contains("lockfile changed"));
        env::remove_var("FS2_STATE_DIR");
        Ok(())
    }

    #[test]
    fn deps_status_reads_nested_root_install_state() -> Result<(), Box<dyn std::error::Error>> {
        let _guard = env_guard();
        let dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        env::set_var("FS2_STATE_DIR", state_dir.path());
        let app_dir = dir.path().join("apps").join("web");
        fs::create_dir_all(&app_dir)?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"workspaces":["apps/*"]}"#,
        )?;
        fs::write(
            app_dir.join("package.json"),
            r#"{"packageManager":"pnpm@9.0.0"}"#,
        )?;
        fs::write(app_dir.join("pnpm-lock.yaml"), "lock-v1")?;
        let roots = fs2_rules::detect_dependency_roots_at(&app_dir)?;
        let hash = dependency_lock_hash(&roots[0])?.ok_or("missing lock hash")?;
        let mut state = BTreeMap::new();
        state.insert(dependency_state_key(".", roots[0].ecosystem), hash);
        write_dependency_install_state(&app_dir, &state)?;

        let output = deps_status(dir.path())?;

        assert!(output.contains("apps/web: node via `pnpm install`; installed"));
        env::remove_var("FS2_STATE_DIR");
        Ok(())
    }

    #[test]
    fn deps_install_record_redetects_created_lockfile() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let _guard = env_guard();
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"pnpm@9.0.0"}"#,
        )?;
        let state_dir = tempfile::tempdir()?;
        env::set_var("FS2_STATE_DIR", state_dir.path());
        let stale_root = fs2_rules::detect_dependency_roots_at(dir.path())?
            .into_iter()
            .next()
            .ok_or("missing dependency root")?;
        assert!(stale_root.lockfiles.is_empty());

        fs::write(dir.path().join("pnpm-lock.yaml"), "created-by-install")?;
        record_dependency_install_state(stale_root)?;

        assert!(deps_status(dir.path())?.contains("; installed"));
        env::remove_var("FS2_STATE_DIR");
        Ok(())
    }

    #[test]
    fn deps_install_rejects_ambiguous_multi_ecosystem_root(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"npm@10.0.0"}"#,
        )?;
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )?;

        let error = match deps_install(dir.path(), false) {
            Ok(output) => {
                return Err(format!("ambiguous root unexpectedly succeeded: {output}").into())
            }
            Err(error) => error,
        };

        assert!(error.contains("multiple dependency roots detected"));
        assert!(error.contains("node"));
        assert!(error.contains("rust"));
        Ok(())
    }

    #[test]
    fn deps_install_without_yes_prints_safe_command() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"npm@10.0.0"}"#,
        )?;

        let output = deps_install(dir.path(), false)?;

        assert!(output.contains("Would run `npm install`"));
        assert!(output.contains("--yes"));
        Ok(())
    }

    #[test]
    fn status_text_has_stable_golden_output() -> Result<(), String> {
        let output = render_status_text(&empty_status())?;

        assert_eq!(
            output,
            "FS2 status:\n  connection: offline\n  cursor lag: 0\n  pending uploads: 0\n  pending downloads: 0\n  cache size: 0 bytes\n  conflicts: 0\n  generated dirs: none\n  env: 0 records (0 secrets)\n  pending errors: none\n  git warnings: none\n"
        );
        Ok(())
    }

    #[test]
    fn status_json_has_stable_golden_shape() -> Result<(), String> {
        let output =
            serde_json::to_string_pretty(&empty_status()).map_err(|error| error.to_string())?;

        assert_eq!(
            output,
            "{\n  \"connection_state\": \"offline\",\n  \"cursor_lag\": 0,\n  \"pending_uploads\": 0,\n  \"pending_downloads\": 0,\n  \"cache_size_bytes\": 0,\n  \"conflicts\": 0,\n  \"generated_dirs\": [],\n  \"env_summary\": {\n    \"total\": 0,\n    \"secrets\": 0\n  },\n  \"git_warnings\": [],\n  \"pending_errors\": []\n}"
        );
        Ok(())
    }

    #[test]
    fn status_reports_generated_dirs_separately() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"pnpm@9.0.0"}"#,
        )?;
        fs::create_dir_all(dir.path().join("node_modules"))?;

        let status = status_for_path(dir.path());
        let output = render_status_text(&status)?;

        assert_eq!(status.generated_dirs, vec!["node_modules".to_owned()]);
        assert!(output.contains("generated dirs:\n    - node_modules\n"));
        Ok(())
    }

    #[test]
    fn status_respects_generated_dir_normal_override() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"pnpm@9.0.0"}"#,
        )?;
        fs::create_dir_all(dir.path().join("node_modules"))?;
        fs::write(dir.path().join(".fs2ignore"), ":normal node_modules/\n")?;

        let status = status_for_path(dir.path());

        assert!(status.generated_dirs.is_empty());
        Ok(())
    }

    #[test]
    fn status_surfaces_pending_operation_errors() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("config.json");
        let metadata_db = dir.path().join("metadata.sqlite");
        let workspace_id = WorkspaceId::new_v4();
        let root_node_id = NodeId::new_v4();
        save_local_workspaces(
            &config_path,
            &[LocalWorkspaceConfig {
                workspace_id: workspace_id.to_string(),
                name: "pending-workspace".to_owned(),
                root_node_id: root_node_id.to_string(),
                metadata_db: metadata_db.display().to_string(),
                path: None,
                mount_path: None,
            }],
        )?;
        let mut store = fs2_daemon::LocalStore::open(&metadata_db)?;
        store.initialize_workspace(workspace_id, "pending-workspace", root_node_id)?;
        let operation = fs2_core::Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id,
            device_id: fs2_core::DeviceId::new_v4(),
            base_cursor: fs2_core::Cursor::new(0)?,
            kind: fs2_core::OperationKind::DeleteNode {
                node_id: NodeId::new_v4(),
                recursive: false,
            },
            created_at: chrono::Utc::now(),
        };
        store.put_pending_op(&operation)?;
        store.mark_pending_op_failed(&operation, "permanent failure")?;
        store.mark_pending_op_failed(&operation, "permanent failure")?;

        let status = status_for_path_with_config(dir.path(), &config_path)?;

        assert_eq!(status.pending_uploads, 1);
        assert_eq!(status.pending_errors.len(), 1);
        assert_eq!(status.pending_errors[0].workspace, "pending-workspace");
        assert_eq!(status.pending_errors[0].retry_count, 2);
        assert_eq!(status.pending_errors[0].last_error, "permanent failure");
        Ok(())
    }

    #[test]
    fn doctor_cli_reports_login_actions_when_not_configured(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("config.json");
        let credentials = FakeCredentialStore::default();

        let output = doctor_with_context(dir.path(), &credentials, &config_path)?;

        assert!(output.contains("backend: not configured; run `fs2 login --backend <url>`"));
        assert!(output.contains("auth token: not configured; run `fs2 login --backend <url>`"));
        assert!(output.contains("workspace keys: no local workspaces"));
        assert!(output.contains("fs2 workspace create <name>"));
        Ok(())
    }

    #[test]
    fn doctor_cli_validates_backend_token_and_workspace_records(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("config.json");
        let metadata_db = dir.path().join("metadata.sqlite");
        fs::write(&metadata_db, "")?;
        let credentials = FakeCredentialStore::default();
        credentials.set_access_token("doctor-token")?;
        let (backend, handle) = serve_json_times(r#"{"devices":[]}"#, 2)?;
        save_cli_config(
            &config_path,
            &CliConfig {
                backend_url: backend.clone(),
                user_id: "user-1".to_owned(),
                device_id: "00000000-0000-0000-0000-000000000101".to_owned(),
                device_name: "device-1".to_owned(),
                token_type: "Bearer".to_owned(),
            },
        )?;
        save_local_workspaces(
            &config_path,
            &[LocalWorkspaceConfig {
                workspace_id: "00000000-0000-0000-0000-000000000201".to_owned(),
                name: "personal-code".to_owned(),
                root_node_id: "00000000-0000-0000-0000-000000000202".to_owned(),
                metadata_db: metadata_db.display().to_string(),
                path: Some(dir.path().display().to_string()),
                mount_path: None,
            }],
        )?;

        let output = doctor_with_context(dir.path(), &credentials, &config_path)?;
        let requests = handle.join().map_err(|_| "server panicked")??;

        assert!(output.contains(&format!("backend: reachable at {backend}")));
        assert!(output.contains("auth token: valid for backend device list"));
        assert!(output.contains("workspace keys: WARNING"));
        assert!(output.contains("personal-code"));
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("GET /v1/devices HTTP/1.1"));
        assert!(requests[0].contains("Authorization: Bearer doctor-token"));
        assert!(!output.contains("doctor-token"));
        Ok(())
    }

    #[test]

    fn doctor_warns_for_node_package_manager_mismatch() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"pnpm@9.0.0"}"#,
        )?;
        fs::write(dir.path().join("package-lock.json"), "{}")?;

        let output = doctor(dir.path())?;

        assert!(output.contains("package manager mismatch"));
        assert!(output.contains("expected pnpm"));
        assert!(output.contains("package-lock.json"));
        assert!(output.contains("run `pnpm install`"));
        Ok(())
    }

    #[test]
    fn doctor_treats_pnpm_workspace_file_as_expected_manager(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("package.json"), r#"{"name":"root"}"#)?;
        fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - apps/*\n",
        )?;
        fs::write(dir.path().join("package-lock.json"), "{}")?;

        let output = doctor(dir.path())?;

        assert!(output.contains(".: WARNING package manager mismatch"));
        assert!(output.contains("expected pnpm"));
        assert!(output.contains("package-lock.json"));
        assert!(output.contains("run `pnpm install`"));
        assert!(output.contains("node_modules/ is generated dependency cache; run `pnpm install`"));
        assert!(!output.contains("run `npm install`"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_child_workspace_package_manager_mismatch(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let app_dir = dir.path().join("apps").join("api");
        fs::create_dir_all(&app_dir)?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"workspaces":["apps/*"],"packageManager":"pnpm@9.0.0"}"#,
        )?;
        fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - apps/*\n",
        )?;
        fs::write(app_dir.join("package.json"), r#"{"name":"api"}"#)?;
        fs::write(app_dir.join("package-lock.json"), "{}")?;

        let output = doctor(dir.path())?;

        assert!(output.contains("apps/api: WARNING package manager mismatch"));
        assert!(output.contains("package-lock.json"));
        assert!(
            output.contains("- .: node_modules/ is generated dependency cache; run `pnpm install`")
        );
        assert!(!output.contains("apps/api: node_modules/ is generated dependency cache"));
        Ok(())
    }

    #[test]
    fn doctor_uses_nearest_suppressed_workspace_manager_for_nested_mismatch(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let lib_dir = dir.path().join("packages").join("lib");
        let app_dir = lib_dir.join("examples").join("api");
        fs::create_dir_all(&app_dir)?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"workspaces":["packages/*"],"packageManager":"pnpm@9.0.0"}"#,
        )?;
        fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\n",
        )?;
        fs::write(
            lib_dir.join("package.json"),
            r#"{"workspaces":["examples/*"],"packageManager":"pnpm@9.0.0"}"#,
        )?;
        fs::write(
            lib_dir.join("pnpm-workspace.yaml"),
            "packages:\n  - examples/*\n",
        )?;
        fs::write(app_dir.join("package.json"), r#"{"name":"api"}"#)?;
        fs::write(app_dir.join("package-lock.json"), "{}")?;

        let output = doctor(dir.path())?;

        assert!(output.contains("packages/lib/examples/api: WARNING package manager mismatch"));
        assert!(output.contains("expected pnpm"));
        assert!(output.contains("package-lock.json"));
        Ok(())
    }
    #[cfg(unix)]
    #[test]
    fn daemon_socket_connects_to_unix_listener() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket_path = dir.path().join("fs2d.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket_path)?;
        let handle = thread::spawn(move || listener.accept().map(|_| ()));

        assert!(daemon_socket_connects(&socket_path));

        handle
            .join()
            .map_err(|_| "daemon socket server panicked")??;
        Ok(())
    }

    #[test]
    fn doctor_reports_default_git_protection_without_config(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let output = doctor(dir.path())?;

        assert!(output.starts_with("FS2 doctor:\n  config: not found\n"));
        assert!(output.contains("  fuse: "));
        assert!(output.contains("  daemon: not running at "));
        assert!(output.contains("daemon startup is not implemented yet"));
        assert!(output.contains("  cache: not initialized"));
        assert!(output.contains("  path collisions: none under portable naming policy"));
        assert!(output
            .contains("  env files: safe; `.env` files are secret or local-only when present"));
        assert!(output.contains("  git internals: protected by built-in defaults"));
        assert!(output.contains("  dependencies: no package roots detected"));
        Ok(())
    }

    #[test]
    fn doctor_ignores_path_collisions_inside_generated_dirs(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let generated = dir.path().join(".next").join("cache");
        fs::create_dir_all(&generated)?;
        fs::write(generated.join("Foo.js"), "one")?;
        fs::write(generated.join("foo.js"), "two")?;

        let output = doctor(dir.path())?;

        assert!(output.contains("path collisions: none under portable naming policy"));
        assert!(!output.contains("Foo.js"));
        assert!(!output.contains("foo.js"));
        Ok(())
    }

    #[test]
    fn doctor_does_not_warn_for_env_files_inside_generated_dirs(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let generated = dir.path().join(".next");
        fs::create_dir_all(&generated)?;
        fs::write(generated.join(".env"), "API_KEY=secret")?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: safe"));
        assert!(!output.contains("env files: WARNING"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_existing_envrc_file() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join(".envrc"), "export API_KEY=secret")?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: WARNING plaintext `.env` files would sync normally"));
        assert!(output.contains(".envrc"));
        Ok(())
    }

    #[test]
    fn doctor_accepts_recursive_secret_env_exception() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":normal apps/**\n:secret apps/**/.env*\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: safe"));
        assert!(!output.contains("env files: WARNING"));
        Ok(())
    }

    #[test]
    fn doctor_accepts_scoped_recursive_secret_env_exception(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":normal apps/*\n:secret .env*\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: safe"));
        assert!(!output.contains("env files: WARNING"));
        Ok(())
    }

    #[test]
    fn doctor_warns_when_recursive_exception_misses_dotenv_variant(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":normal apps/**\n:secret apps/**/.env\n:secret apps/**/.env.local\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: WARNING plaintext `.env` files would sync normally"));
        assert!(output.contains(".env.fs2doctorprobe"));
        Ok(())
    }

    #[test]
    fn doctor_warns_when_parent_glob_exception_is_only_shallow(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":normal apps/*\n:secret apps/.env*\n:secret apps/*/.env*\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: WARNING plaintext `.env` files would sync normally"));
        assert!(output.contains("apps/fs2doctorprobe/nested/.env.fs2doctorprobe"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_existing_literal_directory_rule_without_env_file(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::create_dir(dir.path().join("apps"))?;
        fs::write(dir.path().join(".fs2ignore"), ":normal apps\n")?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: WARNING plaintext `.env` files would sync normally"));
        assert!(output.contains("apps/.env rule resolves to normal"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_expanded_directory_rules_without_env_file(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":normal {apps,services}\n:normal workers/[ab]\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: WARNING plaintext `.env` files would sync normally"));
        assert!(output.contains("apps/.env rule resolves to normal"));
        assert!(output.contains("services/.env rule resolves to normal"));
        assert!(output.contains("workers/a/.env rule resolves to normal"));
        assert!(output.contains("workers/b/.env rule resolves to normal"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_syncable_env_rule_without_env_file(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":normal .env\n:normal .env.development\n:normal .env.test\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: WARNING plaintext `.env` files would sync normally"));
        assert!(output.contains(".env rule resolves to normal"));
        assert!(output.contains(".env.development rule resolves to normal"));
        assert!(output.contains(".env.test rule resolves to normal"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_broad_syncable_rule_without_env_file(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":normal apps/**\n:secret apps/.env\n:secret apps/*/.env\n:normal packages/*/\n:normal literal/\n:secret literal/.env\n:secret literal/*/.env\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: WARNING plaintext `.env` files would sync normally"));
        assert!(output.contains("apps/fs2doctorprobe/nested/.env rule resolves to normal"));
        assert!(output.contains("packages/fs2doctorprobe/.env rule resolves to normal"));
        assert!(output.contains("literal/fs2doctorprobe/nested/.env rule resolves to normal"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_recursive_env_glob_after_shallow_exceptions(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":normal apps/**/.env\n:secret apps/.env\n:secret apps/*/.env\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: WARNING plaintext `.env` files would sync normally"));
        assert!(output.contains("apps/fs2doctorprobe/nested/.env rule resolves to normal"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_leading_recursive_env_glob_and_dotgithub_rule(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":normal **/.env\n:secret .env\n:secret */.env\n:secret */*/.env\n:secret */*/*/.env\n:normal .github/**\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: WARNING plaintext `.env` files would sync normally"));
        assert!(output.contains("recursive rule `**/.env` may sync unbounded `.env` descendants"));
        assert!(
            output.contains("recursive rule `.github/**` may sync unbounded `.env` descendants")
        );
        Ok(())
    }

    #[test]
    fn doctor_accepts_recursive_env_glob_safe_override() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":normal **/.env\n:secret **/.env\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: safe"));
        assert!(!output.contains("env files: WARNING"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_uncovered_brace_env_alternative_without_file(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":normal {apps,services}/*/.env\n:secret apps/*/.env\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: WARNING plaintext `.env` files would sync normally"));
        assert!(output.contains("services/fs2doctorprobe/.env rule resolves to normal"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_globbed_env_override_without_env_file(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":secret .env*\n:normal apps/*/.env\n:normal services/[0-9]/.env\n:secret services/0/.env\n:normal broad/[0-9A-Za-z_]/.env\n:secret broad/0/.env\n:normal workers/[!a-z]/.env\n:normal more/[!0-9a-z_]/.env\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: WARNING plaintext `.env` files would sync normally"));
        assert!(output.contains("apps/fs2doctorprobe/.env rule resolves to normal"));
        assert!(output.contains("services/1/.env rule resolves to normal"));
        assert!(output.contains("broad/1/.env rule resolves to normal"));
        assert!(output.contains("workers/0/.env rule resolves to normal"));
        assert!(output.contains("more/A/.env rule resolves to normal"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_env_file_in_generated_dir_when_profile_disabled(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        let target_dir = dir.path().join("target");
        fs::create_dir_all(&config_dir)?;
        fs::create_dir_all(&target_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[profiles]\nrust = false\n",
        )?;
        fs::write(target_dir.join(".env"), "API_KEY=secret")?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: WARNING plaintext `.env` files would sync normally"));
        assert!(output.contains("target/.env"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_env_descendant_override_inside_generated_dir(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let app_dir = dir.path().join("target").join("app");
        fs::create_dir_all(&app_dir)?;
        fs::write(dir.path().join(".fs2ignore"), ":normal target/app/**\n")?;
        fs::write(app_dir.join(".env"), "API_KEY=secret")?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: WARNING plaintext `.env` files would sync normally"));
        assert!(output.contains("target/app/.env"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_collision_descendant_override_inside_generated_dir(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let app_dir = dir.path().join("target").join("app");
        fs::create_dir_all(&app_dir)?;
        fs::write(dir.path().join(".fs2ignore"), ":normal target/app/**\n")?;
        fs::write(app_dir.join("Foo.rs"), "one")?;
        fs::write(app_dir.join("foo.rs"), "two")?;

        let output = doctor(dir.path())?;

        assert!(output.contains("path collisions: WARNING"));
        assert!(output.contains("target/app"));
        assert!(output.contains("`Foo.rs`"));
        assert!(output.contains("`foo.rs`"));
        Ok(())
    }

    #[test]
    fn doctor_reports_cache_path_collisions_and_env_safety(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cache_dir = dir.path().join(".fs2").join("cache");
        fs::create_dir_all(&cache_dir)?;
        fs::write(dir.path().join("Readme.md"), "one")?;
        fs::write(dir.path().join("README.md"), "two")?;
        fs::write(dir.path().join(".env"), "API_KEY=secret")?;

        let output = doctor(dir.path())?;

        assert!(output.contains("cache: writable"));
        assert!(output.contains("path collisions: WARNING"));
        assert!(output.contains("`README.md`"));
        assert!(output.contains("`Readme.md`"));
        assert!(output.contains("env files: WARNING plaintext `.env` files would sync normally"));
        assert!(output.contains("add `:secret .env*` or `:local-only .env*`"));
        assert!(output.contains("    - .env"));
        assert!(!cache_dir.join(".fs2-doctor-write-test").exists());
        Ok(())
    }

    #[test]
    fn doctor_accepts_secret_env_rule() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join(".fs2ignore"), ":secret .env\n")?;
        fs::write(dir.path().join(".env"), "API_KEY=secret")?;

        let output = doctor(dir.path())?;

        assert!(output.contains("env files: safe"));
        assert!(!output.contains("env files: WARNING"));
        Ok(())
    }

    #[test]
    fn doctor_warns_when_generated_profile_is_disabled() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[profiles]\nrust = false\n",
        )?;
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("target/: WARNING generated directory would sync as"));
        assert!(output.contains("add `:generated target/**` or `:local-only target/**`"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_all_node_generated_paths_when_profile_disabled(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[profiles]\nnode = false\n",
        )?;
        fs::write(dir.path().join("package.json"), r#"{"name":"web"}"#)?;

        let output = doctor(dir.path())?;

        assert!(output.contains("coverage/: WARNING generated directory would sync as"));
        assert!(output.contains(".vercel/: WARNING generated directory would sync as"));
        Ok(())
    }

    #[test]
    fn doctor_explains_node_modules_generation_and_install_command(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"pnpm@9.0.0"}"#,
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("dependencies:"));
        assert!(output.contains("node_modules/ is generated dependency cache"));
        assert!(output.contains("run `pnpm install`"));
        Ok(())
    }

    #[test]
    fn doctor_skips_generated_dependency_roots() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let generated_dir = dir.path().join(".next").join("standalone");
        fs::create_dir_all(&generated_dir)?;
        fs::write(
            generated_dir.join("package.json"),
            r#"{"name":"standalone"}"#,
        )?;

        let output = doctor(dir.path())?;

        assert!(!output.contains(".next/standalone: node_modules/"));
        Ok(())
    }

    #[test]
    fn doctor_uses_workspace_root_install_command_once() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let app_dir = dir.path().join("apps").join("api");
        fs::create_dir_all(&app_dir)?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"pnpm@9.0.0","workspaces":["apps/*"]}"#,
        )?;
        fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'apps/*'\n",
        )?;
        fs::write(app_dir.join("package.json"), r#"{"name":"api"}"#)?;

        let output = doctor(dir.path())?;

        assert!(
            output.contains("- .: node_modules/ is generated dependency cache; run `pnpm install`")
        );
        assert!(!output.contains("apps/api: node_modules/"));
        assert!(!output.contains("run `npm install`"));
        Ok(())
    }

    #[test]
    fn doctor_does_not_use_package_json_workspaces_for_pnpm_without_pnpm_file(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let app_dir = dir.path().join("apps").join("api");
        fs::create_dir_all(&app_dir)?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"pnpm@9.0.0","workspaces":["apps/*"]}"#,
        )?;
        fs::write(
            app_dir.join("package.json"),
            r#"{"packageManager":"bun@1.0.0"}"#,
        )?;

        let output = doctor(dir.path())?;

        assert!(output
            .contains("apps/api: node_modules/ is generated dependency cache; run `bun install`"));
        Ok(())
    }

    #[test]
    fn doctor_keeps_child_guidance_when_workspace_root_rule_not_generated(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let app_dir = dir.path().join("apps").join("api");
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&app_dir)?;
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[profiles]\nnode = false\n[[rules]]\npattern = \"apps/*/node_modules/**\"\naction = \"dependency-cache\"\n",
        )?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"npm@10.0.0","workspaces":["apps/*"]}"#,
        )?;
        fs::write(
            app_dir.join("package.json"),
            r#"{"packageManager":"bun@1.0.0"}"#,
        )?;

        let output = doctor(dir.path())?;

        assert!(output
            .contains("apps/api: node_modules/ is generated dependency cache; run `bun install`"));
        Ok(())
    }

    #[test]
    fn doctor_keeps_grandchild_when_intermediate_workspace_is_suppressed(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let lib_dir = dir.path().join("packages").join("lib");
        let api_dir = lib_dir.join("examples").join("api");
        fs::create_dir_all(&api_dir)?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"npm@10.0.0","workspaces":["packages/*"]}"#,
        )?;
        fs::write(
            lib_dir.join("package.json"),
            r#"{"packageManager":"npm@10.0.0","workspaces":["examples/*"]}"#,
        )?;
        fs::write(
            api_dir.join("package.json"),
            r#"{"packageManager":"bun@1.0.0"}"#,
        )?;

        let output = doctor(dir.path())?;

        assert!(!output.contains("packages/lib: node_modules/"));
        assert!(output.contains("packages/lib/examples/api: node_modules/ is generated dependency cache; run `bun install`"));
        Ok(())
    }

    #[test]
    fn doctor_keeps_nested_package_outside_workspace_globs(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let package_dir = dir.path().join("packages").join("lib");
        let example_dir = dir.path().join("examples").join("api");
        fs::create_dir_all(&package_dir)?;
        fs::create_dir_all(&example_dir)?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"npm@10.0.0","workspaces":["packages/*"]}"#,
        )?;
        fs::write(package_dir.join("package.json"), r#"{"name":"lib"}"#)?;
        fs::write(
            example_dir.join("package.json"),
            r#"{"packageManager":"bun@1.0.0"}"#,
        )?;

        let output = doctor(dir.path())?;

        assert!(!output.contains("packages/lib: node_modules/"));
        assert!(output.contains(
            "examples/api: node_modules/ is generated dependency cache; run `bun install`"
        ));
        Ok(())
    }

    #[test]
    fn doctor_does_not_let_single_star_cross_directories() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let deep_dir = dir
            .path()
            .join("packages")
            .join("lib")
            .join("examples")
            .join("api");
        fs::create_dir_all(&deep_dir)?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"npm@10.0.0","workspaces":["packages/*"]}"#,
        )?;
        fs::write(
            deep_dir.join("package.json"),
            r#"{"packageManager":"bun@1.0.0"}"#,
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("packages/lib/examples/api: node_modules/ is generated dependency cache; run `bun install`"));
        Ok(())
    }

    #[test]
    fn doctor_keeps_nested_package_outside_pnpm_workspace_globs(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let package_dir = dir.path().join("packages").join("lib");
        let example_dir = dir.path().join("examples").join("api");
        fs::create_dir_all(&package_dir)?;
        fs::create_dir_all(&example_dir)?;
        fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n# workspace packages\n\n  - 'packages/*' # workspace packages\n",
        )?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"workspaces":["examples/*"]}"#,
        )?;
        fs::write(dir.path().join("package-lock.json"), "{}")?;
        fs::write(package_dir.join("package.json"), r#"{"name":"lib"}"#)?;
        fs::write(
            example_dir.join("package.json"),
            r#"{"packageManager":"bun@1.0.0"}"#,
        )?;

        let output = doctor(dir.path())?;

        assert!(!output.contains("packages/lib: node_modules/"));
        assert!(output.contains(
            "examples/api: node_modules/ is generated dependency cache; run `bun install`"
        ));
        Ok(())
    }

    #[test]
    fn doctor_does_not_apply_pnpm_workspace_to_npm_root() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let package_dir = dir.path().join("packages").join("lib");
        fs::create_dir_all(&package_dir)?;
        fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n",
        )?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"npm@10.0.0"}"#,
        )?;
        fs::write(
            package_dir.join("package.json"),
            r#"{"packageManager":"bun@1.0.0"}"#,
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains(
            "packages/lib: node_modules/ is generated dependency cache; run `bun install`"
        ));
        Ok(())
    }

    #[test]
    fn doctor_parses_inline_pnpm_workspace_packages() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let package_dir = dir.path().join("packages").join("lib");
        fs::create_dir_all(&package_dir)?;
        fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages: ['packages/*'] # workspace packages\n",
        )?;
        fs::write(package_dir.join("package.json"), r#"{"name":"lib"}"#)?;

        let output = doctor(dir.path())?;

        assert!(
            output.contains("- .: node_modules/ is generated dependency cache; run `pnpm install`")
        );
        assert!(!output.contains("packages/lib: node_modules/"));
        Ok(())
    }

    #[test]
    fn doctor_honors_pnpm_workspace_exclusions() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let test_dir = dir
            .path()
            .join("components")
            .join("lib")
            .join("test")
            .join("api");
        fs::create_dir_all(&test_dir)?;
        fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'components/**'\n  - '!**/test/**'\n",
        )?;
        fs::write(
            test_dir.join("package.json"),
            r#"{"packageManager":"bun@1.0.0"}"#,
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("components/lib/test/api: node_modules/ is generated dependency cache; run `bun install`"));
        Ok(())
    }

    #[test]
    fn doctor_handles_bare_workspace_wildcard() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let app_dir = dir.path().join("app");
        fs::create_dir_all(&app_dir)?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"npm@10.0.0","workspaces":["*"]}"#,
        )?;
        fs::write(app_dir.join("package.json"), r#"{"name":"app"}"#)?;

        let output = doctor(dir.path())?;

        assert!(
            output.contains("- .: node_modules/ is generated dependency cache; run `npm install`")
        );
        assert!(!output.contains("app: node_modules/"));
        Ok(())
    }

    #[test]
    fn doctor_keeps_nested_manager_when_ancestor_is_only_tooling_monorepo(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let app_dir = dir.path().join("apps").join("api");
        fs::create_dir_all(&app_dir)?;
        fs::write(dir.path().join("turbo.json"), "{}")?;
        fs::write(
            app_dir.join("package.json"),
            r#"{"packageManager":"pnpm@9.0.0"}"#,
        )?;

        let output = doctor(dir.path())?;

        assert!(output
            .contains("apps/api: node_modules/ is generated dependency cache; run `pnpm install`"));
        assert!(
            !output.contains("- .: node_modules/ is generated dependency cache; run `npm install`")
        );
        Ok(())
    }

    #[test]
    fn doctor_omits_node_modules_generation_when_profile_disabled(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[profiles]\nnode = false\n",
        )?;
        fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"pnpm@9.0.0"}"#,
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("dependencies:"));
        assert!(!output.contains("node_modules/ is generated dependency cache"));
        Ok(())
    }

    #[test]
    fn doctor_warns_when_config_overrides_git_internals_to_normal(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \".git/**\"\naction = \"normal\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output
            .contains("git internals: WARNING confirmed syncable rule matches .git internals"));
        assert!(output.contains("    - .git/**"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_packfile_specific_normal_rule() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \".git/objects/pack/pack-[0-9a-f]*.pack\"\naction = \"normal\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains(".git internals"));
        assert!(output.contains(".git/objects/pack/pack-[0-9a-f]*.pack"));
        Ok(())
    }

    #[test]
    fn doctor_reads_fs2ignore_git_normal_override() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join(".fs2ignore"), ":normal .git/**\n")?;

        let output = doctor(dir.path())?;

        assert!(output.contains(".git internals"));
        assert!(output.contains(".git/**"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_git_config_leaf_rules() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \"config\"\naction = \"normal\"\n",
        )?;
        fs::write(dir.path().join(".fs2ignore"), ":normal **/config\n")?;

        let output = doctor(dir.path())?;

        assert!(output.contains(".git internals"));
        assert!(output.contains("config"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_git_head_leaf_rule() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \"HEAD\"\naction = \"normal\"\n[[rules]]\npattern = \"HEAD/.git/**\"\naction = \"local-only\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains(".git internals"));
        assert!(output.contains("HEAD"));
        Ok(())
    }

    #[test]
    fn doctor_ignores_fs2ignore_structural_rule_shadowed_by_config(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \"third_party/*/.git/**\"\naction = \"local-only\"\n",
        )?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":normal third_party/*/.git/**\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("git internals: protected by built-in defaults"));
        assert!(!output.contains("WARNING"));
        Ok(())
    }

    #[test]
    fn doctor_uses_effective_rule_not_any_matching_normal_rule(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \".git/**\"\naction = \"normal\"\n[[rules]]\npattern = \".git/**\"\naction = \"local-only\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("git internals: protected by built-in defaults"));
        assert!(!output.contains("WARNING"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_nested_git_override() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \"third_party/foo/.git/**\"\naction = \"normal\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains(".git internals"));
        assert!(output.contains("third_party/foo/.git/**"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_normal_subtree_containing_git_dir() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \"third_party/foo/**\"\naction = \"normal\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains(".git internals"));
        assert!(output.contains("third_party/foo/**"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_globbed_explicit_git_prefix() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \"third_party/*/.git/**\"\naction = \"normal\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains(".git internals"));
        assert!(output.contains("third_party/*/.git/**"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_globbed_normal_subtree() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \"third_party/*/**\"\naction = \"normal\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains(".git internals"));
        assert!(output.contains("third_party/*/**"));
        Ok(())
    }

    #[test]
    fn doctor_globbed_probe_not_hidden_by_one_literal_exception(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \"third_party/*/.git/**\"\naction = \"normal\"\n[[rules]]\npattern = \"third_party/fs2doctorprobea/.git/**\"\naction = \"local-only\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains(".git internals"));
        assert!(output.contains("third_party/*/.git/**"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_question_and_class_globs() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \"third_party/?/.git/**\"\naction = \"normal\"\n[[rules]]\npattern = \"deps/[ab]/.git/**\"\naction = \"normal\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("third_party/?/.git/**"));
        assert!(output.contains("deps/[ab]/.git/**"));
        Ok(())
    }

    #[test]
    fn doctor_class_probe_not_hidden_by_one_literal_exception(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \"deps/[ab]/.git/**\"\naction = \"normal\"\n[[rules]]\npattern = \"deps/a/.git/**\"\naction = \"local-only\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains(".git internals"));
        assert!(output.contains("deps/[ab]/.git/**"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_negated_class_brace_and_escaped_globs(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \"deps/[!a]/.git/**\"\naction = \"normal\"\n[[rules]]\npattern = \"third_party/{foo,bar}/.git/**\"\naction = \"normal\"\n[[rules]]\npattern = 'literals/foo\\*/.git/**'\naction = \"normal\"\n[[rules]]\npattern = 'literals/foo\\?/.git/**'\naction = \"normal\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("deps/[!a]/.git/**"));
        assert!(output.contains("third_party/{foo,bar}/.git/**"));
        assert!(output.contains(r"literals/foo\*/.git/**"));
        assert!(output.contains(r"literals/foo\?/.git/**"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_pin_and_lazy_git_sync_rules() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \".git/**\"\naction = \"pin\"\n[[rules]]\npattern = \"third_party/foo/**\"\naction = \"lazy\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains(".git internals"));
        assert!(output.contains(".git/**"));
        assert!(output.contains("third_party/foo/**"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_cartesian_and_negated_class_gaps() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":normal deps/[ab]/[cd]/.git/**\n:local-only deps/a/c/.git/**\n:local-only deps/b/d/.git/**\n:normal deps/[!bc]/.git/**\n:normal vendor/{foo,bar,baz}/.git/**\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("deps/[ab]/[cd]/.git/**"));
        assert!(output.contains("deps/[!bc]/.git/**"));
        assert!(output.contains("vendor/{foo,bar,baz}/.git/**"));
        Ok(())
    }

    #[test]
    fn doctor_cautions_for_fully_shadowed_finite_class() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":normal deps/[ab]/.git/**\n:local-only deps/a/.git/**\n:local-only deps/b/.git/**\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("CAUTION broad syncable rule may reach .git internals"));
        assert!(!output.contains("WARNING"));
        Ok(())
    }

    #[test]
    fn doctor_cautions_for_shadowed_glob_parent_rule() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":normal third_party/*\n:local-only third_party/fs2doctorprobea/.git/**\n:local-only third_party/fs2doctorprobeb/.git/**\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("CAUTION broad syncable rule may reach .git internals"));
        assert!(output.contains("third_party/*"));
        assert!(!output.contains("git internals: WARNING"));
        Ok(())
    }

    #[test]
    fn doctor_cautions_for_shadowed_subtree_rule() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":normal third_party/foo/**\n:local-only third_party/foo/.git/**\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("CAUTION broad syncable rule may reach .git internals"));
        assert!(output.contains("third_party/foo/**"));
        assert!(!output.contains("git internals: WARNING"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_literal_bang_and_nested_brace_class(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \"/!repo/**\"\naction = \"normal\"\n[[rules]]\npattern = \"vendor/{foo,bar[ab]}/.git/**\"\naction = \"normal\"\n[[rules]]\npattern = \"vendor/foo/.git/**\"\naction = \"local-only\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("/!repo/**"));
        assert!(output.contains("vendor/{foo,bar[ab]}/.git/**"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_wider_classes_and_braces() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join(".fs2ignore"),
            ":normal deps/[!a-z]/.git/**\n:normal deps/[!0-9a-z]/.git/**\n:normal deps/[!A-Za-z0-9_-]/.git/**\n:normal deps/[-]/.git/**\n:normal deps/[abc]/.git/**\n:local-only deps/a/.git/**\n:local-only deps/b/.git/**\n:normal vendor/{foo,bar,baz,qux,zed}/.git/**\n:local-only vendor/foo/.git/**\n:local-only vendor/bar/.git/**\n:local-only vendor/baz/.git/**\n:local-only vendor/qux/.git/**\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains("deps/[!a-z]/.git/**"));
        assert!(output.contains("deps/[!0-9a-z]/.git/**"));
        assert!(output.contains("deps/[!A-Za-z0-9_-]/.git/**"));
        assert!(output.contains("deps/[-]/.git/**"));
        assert!(output.contains("deps/[abc]/.git/**"));
        assert!(output.contains("vendor/{foo,bar,baz,qux,zed}/.git/**"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_directory_style_subtree_rule() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join(".fs2ignore"), ":normal third_party/foo/\n")?;

        let output = doctor(dir.path())?;

        assert!(output.contains(".git internals"));
        assert!(output.contains("third_party/foo/"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_explicit_git_config_rule() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \".git/config\"\naction = \"normal\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains(".git internals"));
        assert!(output.contains(".git/config"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_explicit_git_refs_rule() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \"third_party/foo/.git/refs/**\"\naction = \"normal\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains(".git internals"));
        assert!(output.contains("third_party/foo/.git/refs/**"));
        Ok(())
    }

    #[test]
    fn doctor_warns_for_unsuffixed_directory_rule() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config_dir = dir.path().join(".fs2");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            "version = 1\n[[rules]]\npattern = \"third_party/foo\"\naction = \"normal\"\n",
        )?;

        let output = doctor(dir.path())?;

        assert!(output.contains(".git internals"));
        assert!(output.contains("third_party/foo"));
        Ok(())
    }

    fn empty_status() -> StatusOutput {
        StatusOutput {
            connection_state: "offline".to_owned(),
            cursor_lag: 0,
            pending_uploads: 0,
            pending_downloads: 0,
            cache_size_bytes: 0,
            conflicts: 0,
            env_summary: EnvSummary {
                total: 0,
                secrets: 0,
            },
            generated_dirs: Vec::new(),
            git_warnings: Vec::new(),
            pending_errors: Vec::new(),
        }
    }
}
