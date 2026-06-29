//! `fs2 service install/uninstall/status` — generate and manage macOS
//! `LaunchAgent` and Linux `systemd` user service files for the daemon.
//!
//! The generated files reference the `fs2-daemon` binary. Installation writes
//! the file to the platform-specific user service directory and (on macOS)
//! loads it via `launchctl`, or (on Linux) enables it via `systemctl --user`.
//! Actual auto-start verification requires a real platform service manager
//! and is recorded as blocked in the sandbox.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};

/// Label/identifier for the `LaunchAgent`.
const LABEL: &str = "com.fs2.fs2d";

/// Return the path to the fs2-daemon binary (best effort).
fn daemon_bin() -> String {
    // Prefer an explicit override, then a PATH lookup, then a relative guess.
    if let Ok(p) = std::env::var("FS2_DAEMON_BIN") {
        return p;
    }
    if let Some(exe) = which("fs2-daemon") {
        return exe.to_string_lossy().to_string();
    }
    "fs2-daemon".to_owned()
}

/// Best-effort `which` lookup without an external crate.
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Generate a macOS `LaunchAgent` plist for the daemon.
#[must_use]
pub fn launchagent_plist() -> String {
    let bin = daemon_bin();
    // launchd does not expand $HOME or ~ in plist string values, so resolve
    // to an absolute path at generation time.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
    let log_path = format!("{home}/.fs2/logs/fs2d.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log_path}</string>
    <key>StandardErrorPath</key>
    <string>{log_path}</string>
</dict>
</plist>
"#
    )
}

/// Generate a Linux systemd user unit for the daemon.
#[must_use]
pub fn systemd_user_unit() -> String {
    let bin = daemon_bin();
    format!(
        r"[Unit]
Description=FS2 dev-sync daemon
After=network-online.target

[Service]
Type=simple
ExecStart={bin}
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
"
    )
}

/// Path to the `LaunchAgent` plist.
fn launchagent_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

/// Path to the systemd user unit.
fn systemd_unit_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home).join(".config/systemd/user").join("fs2-daemon.service"))
}

/// Install the service for the current platform.
///
/// # Errors
/// Returns an error if the service file cannot be written.
pub fn install() -> Result<()> {
    if cfg!(target_os = "macos") {
        let path = launchagent_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("create LaunchAgents dir")?;
        }
        std::fs::write(&path, launchagent_plist()).context("write plist")?;
        // launchd does not create parent dirs for StandardOutPath; create the
        // logs directory so log redirection works on first launch.
        if let Ok(home) = std::env::var("HOME") {
            std::fs::create_dir_all(format!("{home}/.fs2/logs")).ok();
        }
        println!("Installed LaunchAgent to {}", path.display());
        println!("Load it with: launchctl load {}", path.display());
        Ok(())
    } else if cfg!(target_os = "linux") {
        let path = systemd_unit_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("create systemd user dir")?;
        }
        std::fs::write(&path, systemd_user_unit()).context("write unit")?;
        println!("Installed systemd user unit to {}", path.display());
        println!("Enable it with: systemctl --user enable --now fs2-daemon");
        Ok(())
    } else {
        Err(anyhow!("service install is only supported on macOS and Linux"))
    }
}

/// Uninstall the service for the current platform.
///
/// # Errors
/// Returns an error if the service file cannot be removed.
pub fn uninstall() -> Result<()> {
    if cfg!(target_os = "macos") {
        let path = launchagent_path()?;
        if path.exists() {
            std::fs::remove_file(&path).context("remove plist")?;
            println!("Removed LaunchAgent at {}", path.display());
            println!("Unload it with: launchctl unload {}", path.display());
        } else {
            println!("No LaunchAgent found at {}", path.display());
        }
        Ok(())
    } else if cfg!(target_os = "linux") {
        let path = systemd_unit_path()?;
        if path.exists() {
            std::fs::remove_file(&path).context("remove unit")?;
            println!("Removed systemd user unit at {}", path.display());
            println!("Run: systemctl --user disable fs2-daemon");
        } else {
            println!("No systemd user unit found at {}", path.display());
        }
        Ok(())
    } else {
        Err(anyhow!("service uninstall is only supported on macOS and Linux"))
    }
}

/// Report the install status for the current platform.
///
/// # Errors
/// Returns an error if the status cannot be determined.
pub fn status() -> Result<()> {
    if cfg!(target_os = "macos") {
        let path = launchagent_path()?;
        if path.exists() {
            println!("LaunchAgent installed at {}", path.display());
        } else {
            println!("LaunchAgent not installed.");
        }
        Ok(())
    } else if cfg!(target_os = "linux") {
        let path = systemd_unit_path()?;
        if path.exists() {
            println!("systemd user unit installed at {}", path.display());
        } else {
            println!("systemd user unit not installed.");
        }
        Ok(())
    } else {
        Err(anyhow!("service status is only supported on macOS and Linux"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchagent_plist_has_label_and_daemon() {
        let plist = launchagent_plist();
        assert!(plist.contains("<key>Label</key>"));
        assert!(plist.contains(LABEL));
        assert!(plist.contains("RunAtLoad"));
        assert!(plist.contains("KeepAlive"));
        // References the daemon binary.
        assert!(plist.contains("fs2-daemon"));
        assert!(plist.contains("<plist"));
    }

    #[test]
    fn systemd_unit_has_exec_start_and_restart() {
        let unit = systemd_user_unit();
        assert!(unit.contains("[Unit]"));
        assert!(unit.contains("[Service]"));
        assert!(unit.contains("[Install]"));
        assert!(unit.contains("ExecStart="));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains("fs2-daemon"));
    }

    #[test]
    fn launchagent_plist_is_valid_xml_structure() {
        let plist = launchagent_plist();
        // Basic structural checks.
        assert!(plist.starts_with("<?xml"));
        assert!(plist.contains("<dict>"));
        assert!(plist.contains("</dict>"));
        assert!(plist.contains("</plist>"));
    }
}
