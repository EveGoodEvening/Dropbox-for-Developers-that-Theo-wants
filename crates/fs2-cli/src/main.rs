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
        }
    }
}
