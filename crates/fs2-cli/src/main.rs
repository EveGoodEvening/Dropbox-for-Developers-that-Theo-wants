//! FS2 command-line entry point.

use fs2_core::{NodeId, RuleAction, WorkspaceId, WorkspacePath};
use serde::{Deserialize, Serialize};
use std::{
    env,
    fmt::Write as _,
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
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
        [cmd, ..] if cmd == "login" || cmd == "logout" || cmd == "device" || cmd == "workspace" || cmd == "mount" || cmd == "status"
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
        [cmd] if cmd == "doctor" => doctor("."),
        [cmd, path] if cmd == "doctor" => doctor(path),
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
    "fs2-devsync CLI\n\nCommands:\n  fs2 login --backend <url> [--device-name <name>]\n  fs2 logout\n  fs2 device list\n  fs2 workspace create <name>\n  fs2 workspace list\n  fs2 workspace init <path> --name <name>\n  fs2 mount <workspace> <path>\n  fs2 status [--json]\n  fs2 doctor [path]\n  fs2 git status [path]\n  fs2 git submodules status [path]\n"
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

fn doctor(path: impl AsRef<Path>) -> Result<String, String> {
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
    let engine =
        fs2_rules::RuleEngine::new(config, ignore_rules).map_err(|error| error.to_string())?;
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
        }
        if !possible_sources.is_empty() {
            out.push_str("  git internals: CAUTION broad syncable rule may reach .git internals\n");
            for pattern in possible_sources {
                writeln!(out, "    - {pattern}").map_err(|error| error.to_string())?;
            }
        }
    }
    append_dependency_doctor(root, &engine, &mut out)?;
    Ok(out)
}

fn fs2_config_path(path: &Path) -> PathBuf {
    path.join(".fs2").join("config.toml")
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
    for dependency_root in &dependency_roots {
        if dependency_root.ecosystem != fs2_rules::DependencyEcosystem::Node {
            continue;
        }
        if tooling_only_node_root(dependency_root) {
            continue;
        }
        let display_root = dependency_display_root(root, &dependency_root.root);
        if covered_by_rendered_node_workspace_ancestor(dependency_root, &rendered_node_roots) {
            continue;
        }
        if dependency_root_is_generated_or_local(engine, &display_root)? {
            continue;
        }
        let renders_guidance = dependency_root
            .generated_paths
            .iter()
            .any(|path| path == "node_modules/")
            && dependency_generated_by_effective_rule(engine, &display_root)?;
        if renders_guidance {
            let command = dependency_root.install_command.join(" ");
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

fn dependency_display_root(root: &Path, dependency_root: &Path) -> String {
    dependency_root
        .strip_prefix(root)
        .ok()
        .filter(|path| !path.as_os_str().is_empty())
        .map_or_else(|| ".".to_owned(), |path| path.display().to_string())
}

fn covered_by_rendered_node_workspace_ancestor(
    dependency_root: &fs2_rules::DependencyRoot,
    rendered_node_roots: &[&fs2_rules::DependencyRoot],
) -> bool {
    rendered_node_roots.iter().any(|candidate| {
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
    if candidate.manager == fs2_rules::PackageManager::Pnpm
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
    if candidate.manager != fs2_rules::PackageManager::Pnpm
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

fn status_for_path(path: impl AsRef<Path>) -> StatusOutput {
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
        assert!(output.contains("fs2 git status [path]"));
        assert!(output.contains("fs2 git submodules status [path]"));
        Ok(())
    }

    #[test]
    fn status_text_has_stable_golden_output() -> Result<(), String> {
        let output = render_status_text(&empty_status())?;

        assert_eq!(
            output,
            "FS2 status:\n  connection: offline\n  cursor lag: 0\n  pending uploads: 0\n  pending downloads: 0\n  cache size: 0 bytes\n  conflicts: 0\n  env: 0 records (0 secrets)\n  pending errors: none\n  git warnings: none\n"
        );
        Ok(())
    }

    #[test]
    fn status_json_has_stable_golden_shape() -> Result<(), String> {
        let output =
            serde_json::to_string_pretty(&empty_status()).map_err(|error| error.to_string())?;

        assert_eq!(
            output,
            "{\n  \"connection_state\": \"offline\",\n  \"cursor_lag\": 0,\n  \"pending_uploads\": 0,\n  \"pending_downloads\": 0,\n  \"cache_size_bytes\": 0,\n  \"conflicts\": 0,\n  \"env_summary\": {\n    \"total\": 0,\n    \"secrets\": 0\n  },\n  \"git_warnings\": [],\n  \"pending_errors\": []\n}"
        );
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
    fn doctor_reports_default_git_protection_without_config(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let output = doctor(dir.path())?;

        assert_eq!(
            output,
            "FS2 doctor:\n  config: not found\n  git internals: protected by built-in defaults\n  dependencies: no package roots detected\n"
        );
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
        assert!(!output.contains("WARNING"));
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
        assert!(!output.contains("WARNING"));
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
            git_warnings: Vec::new(),
            pending_errors: Vec::new(),
        }
    }
}
