//! FS2 command-line entry point.

use serde::Deserialize;
use std::{
    env,
    fmt::Write as _,
    io::{Read, Write},
    net::TcpStream,
    path::Path,
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

fn run_from_args(args: impl IntoIterator<Item = String>) -> Result<String, String> {
    let args = args.into_iter().collect::<Vec<_>>();
    match args.as_slice() {
        [] => Ok(help()),
        [one] if one == "--help" || one == "-h" => Ok(help()),
        [cmd, flag, backend] if cmd == "login" && flag == "--backend" => {
            dev_login(backend, "fs2-dev-cli", "dev-cli-public-key")
        }
        [cmd, flag, backend, name_flag, device_name]
            if cmd == "login" && flag == "--backend" && name_flag == "--device-name" =>
        {
            dev_login(backend, device_name, "dev-cli-public-key")
        }
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
    "fs2-devsync CLI\n\nCommands:\n  fs2 login --backend <url> [--device-name <name>]\n  fs2 git status [path]\n  fs2 git submodules status [path]\n"
        .to_owned()
}

#[derive(Debug, Deserialize)]
struct DevLoginResponse {
    access_token: String,
    token_type: String,
    user_id: String,
    device_id: String,
    warning: String,
}

fn dev_login(backend: &str, device_name: &str, public_key: &str) -> Result<String, String> {
    let (host, port, path) = parse_http_url(backend, "/v1/auth/dev-login")?;
    let body = serde_json::json!({
        "device_name": device_name,
        "platform": {"os": std::env::consts::OS, "arch": std::env::consts::ARCH},
        "public_key": public_key,
    })
    .to_string();
    let response = post_json(&host, port, &path, &body)?;
    let login = serde_json::from_str::<DevLoginResponse>(&response)
        .map_err(|error| format!("login response was not valid JSON: {error}"))?;
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

fn post_json(host: &str, port: u16, path: &str, body: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect((host, port))
        .map_err(|error| format!("login request failed: {error}"))?;
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|error| format!("login request failed: {error}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("login response failed: {error}"))?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or("login response was not valid HTTP")?;
    if !head.starts_with("HTTP/1.1 200") {
        return Err(format!("login request failed: {head}"));
    }
    Ok(body.to_owned())
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

    #[test]
    fn help_lists_git_status() -> Result<(), String> {
        let output = run_from_args(["--help".to_owned()])?;

        assert!(output.contains("fs2 login --backend <url> [--device-name <name>]"));
        assert!(output.contains("fs2 git status [path]"));
        assert!(output.contains("fs2 git submodules status [path]"));
        Ok(())
    }
}
