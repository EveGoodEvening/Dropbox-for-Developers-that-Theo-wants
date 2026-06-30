//! FS2 command-line entry point.

use fs2_core::{RuleAction, WorkspacePath};
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
        [cmd] if cmd == "status" => status_text("."),
        [cmd, flag] if cmd == "status" && flag == "--json" => status_json("."),
        [cmd, flag, path] if cmd == "status" && flag == "--path" => status_text(path),
        [cmd, flag, path, json_flag]
            if cmd == "status" && flag == "--path" && json_flag == "--json" =>
        {
            status_json(path)
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
    "fs2-devsync CLI\n\nCommands:\n  fs2 login --backend <url> [--device-name <name>]\n  fs2 status [--json]\n  fs2 doctor [path]\n  fs2 git status [path]\n  fs2 git submodules status [path]\n"
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
    Ok(out)
}

fn fs2_config_path(path: &Path) -> PathBuf {
    path.join(".fs2").join("config.toml")
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct EnvSummary {
    total: u64,
    secrets: u64,
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
        git_warnings: git_warnings(path),
    }
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

fn status_json(path: impl AsRef<Path>) -> Result<String, String> {
    serde_json::to_string_pretty(&status_for_path(path)).map_err(|error| error.to_string())
}

fn status_text(path: impl AsRef<Path>) -> Result<String, String> {
    render_status_text(&status_for_path(path))
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
            "FS2 status:\n  connection: offline\n  cursor lag: 0\n  pending uploads: 0\n  pending downloads: 0\n  cache size: 0 bytes\n  conflicts: 0\n  env: 0 records (0 secrets)\n  git warnings: none\n"
        );
        Ok(())
    }

    #[test]
    fn status_json_has_stable_golden_shape() -> Result<(), String> {
        let output =
            serde_json::to_string_pretty(&empty_status()).map_err(|error| error.to_string())?;

        assert_eq!(
            output,
            "{\n  \"connection_state\": \"offline\",\n  \"cursor_lag\": 0,\n  \"pending_uploads\": 0,\n  \"pending_downloads\": 0,\n  \"cache_size_bytes\": 0,\n  \"conflicts\": 0,\n  \"env_summary\": {\n    \"total\": 0,\n    \"secrets\": 0\n  },\n  \"git_warnings\": []\n}"
        );
        Ok(())
    }

    #[test]
    fn doctor_reports_default_git_protection_without_config(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let output = doctor(dir.path())?;

        assert_eq!(
            output,
            "FS2 doctor:\n  config: not found\n  git internals: protected by built-in defaults\n"
        );
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
        }
    }
}
