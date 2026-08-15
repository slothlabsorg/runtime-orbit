//! `runtime-runtime-orbit.service` — run the borrow as a background login service so it
//! delegation + port forwarding come back automatically after a reboot/login.
//!
//! macOS → a launchd LaunchAgent. Linux → a systemd --user unit. Windows →
//! printed Task Scheduler guidance (the easiest option there).

use anyhow::{Context, Result};

use crate::config;
use crate::util;

const LABEL: &str = "org.slothlabs.runtime-orbit";

fn binary_path() -> Result<String> {
    Ok(std::env::current_exe()
        .context("locating the runtime-orbit binary")?
        .to_string_lossy()
        .into_owned())
}

/// A PATH that includes where docker/ssh usually live (login services get a
/// minimal PATH otherwise, and runtime-orbit shells out to both).
fn service_path() -> String {
    let mut dirs = vec![
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
        "/usr/bin".to_string(),
        "/bin".to_string(),
        "/usr/sbin".to_string(),
        "/sbin".to_string(),
    ];
    if let Some(home) = dirs::home_dir() {
        dirs.insert(0, home.join(".docker/bin").to_string_lossy().into_owned());
        dirs.insert(0, home.join(".orbstack/bin").to_string_lossy().into_owned());
    }
    dirs.join(":")
}

// ── macOS (launchd) ──────────────────────────────────────────────────────────
#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    fn plist_path() -> Result<std::path::PathBuf> {
        let home = dirs::home_dir().context("no home dir")?;
        Ok(home
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    pub async fn install() -> Result<()> {
        let bin = binary_path()?;
        let log = config::run_dir()?.join("service.log");
        let plist = plist_path()?;
        if let Some(parent) = plist.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{bin}</string>
    <string>up</string>
    <string>--foreground</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>{path}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
            LABEL = LABEL,
            bin = bin,
            path = service_path(),
            log = log.display(),
        );
        std::fs::write(&plist, contents).with_context(|| format!("writing {}", plist.display()))?;

        // Reload cleanly (ignore errors from a not-yet-loaded agent).
        let _ = util::capture("launchctl", &["unload", &plist.to_string_lossy()]).await;
        util::run("launchctl", &["load", "-w", &plist.to_string_lossy()])
            .await
            .context("launchctl load failed")?;
        util::ok(&format!("installed launchd agent {LABEL}"));
        util::info("plist", &plist.to_string_lossy());
        util::info("logs", &log.to_string_lossy());
        Ok(())
    }

    pub async fn uninstall() -> Result<()> {
        let plist = plist_path()?;
        let _ = util::capture("launchctl", &["unload", &plist.to_string_lossy()]).await;
        if plist.exists() {
            std::fs::remove_file(&plist)?;
        }
        util::ok("removed the launchd agent");
        Ok(())
    }

    pub async fn status() -> Result<()> {
        let listed = util::capture("launchctl", &["list"])
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(LABEL))
            .unwrap_or(false);
        report(listed, &plist_path()?.to_string_lossy());
        Ok(())
    }
}

// ── Linux (systemd --user) ─────────────────────────────────────────────────
#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    fn unit_path() -> Result<std::path::PathBuf> {
        let home = dirs::home_dir().context("no home dir")?;
        Ok(home.join(".config/systemd/user/runtime-orbit.service"))
    }

    pub async fn install() -> Result<()> {
        let bin = binary_path()?;
        let unit = unit_path()?;
        if let Some(parent) = unit.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = format!(
            "[Unit]\nDescription=runtime-orbit — borrowed container runtime + port forwarding\nAfter=network-online.target\n\n\
             [Service]\nType=simple\nExecStart={bin} up --foreground\nEnvironment=PATH={path}\nRestart=on-failure\nRestartSec=5\n\n\
             [Install]\nWantedBy=default.target\n",
            bin = bin,
            path = service_path(),
        );
        std::fs::write(&unit, contents).with_context(|| format!("writing {}", unit.display()))?;
        util::run("systemctl", &["--user", "daemon-reload"]).await?;
        util::run(
            "systemctl",
            &["--user", "enable", "--now", "runtime-orbit.service"],
        )
        .await?;
        util::ok("installed + started systemd user unit runtime-orbit.service");
        util::info("unit", &unit.to_string_lossy());
        util::info("logs", "journalctl --user -u runtime-orbit.service -f");
        Ok(())
    }

    pub async fn uninstall() -> Result<()> {
        let _ = util::capture(
            "systemctl",
            &["--user", "disable", "--now", "runtime-orbit.service"],
        )
        .await;
        let unit = unit_path()?;
        if unit.exists() {
            std::fs::remove_file(&unit)?;
        }
        let _ = util::capture("systemctl", &["--user", "daemon-reload"]).await;
        util::ok("removed the systemd user unit");
        Ok(())
    }

    pub async fn status() -> Result<()> {
        let active = util::capture(
            "systemctl",
            &["--user", "is-active", "runtime-orbit.service"],
        )
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "active")
        .unwrap_or(false);
        report(active, &unit_path()?.to_string_lossy());
        Ok(())
    }
}

// ── Other (Windows, etc.) ───────────────────────────────────────────────────
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::*;
    use owo_colors::OwoColorize;

    pub async fn install() -> Result<()> {
        let bin = binary_path()?;
        util::header("Windows — run runtime-orbit at login (Task Scheduler)");
        println!("  The easiest way on Windows is a scheduled task. In PowerShell:\n");
        println!(
            "      {}",
            format!(
                "schtasks /Create /TN runtime-orbit /SC ONLOGON /TR \"{bin} up --foreground\" /RL LIMITED"
            )
            .cyan()
        );
        println!(
            "\n  Remove it later with: {}",
            "schtasks /Delete /TN runtime-orbit /F".cyan()
        );
        Ok(())
    }

    pub async fn uninstall() -> Result<()> {
        util::info(
            "windows",
            "remove with: schtasks /Delete /TN runtime-orbit /F",
        );
        Ok(())
    }

    pub async fn status() -> Result<()> {
        util::info("windows", "check with: schtasks /Query /TN runtime-orbit");
        Ok(())
    }
}

fn report(installed_and_running: bool, location: &str) {
    if installed_and_running {
        util::ok("runtime-orbit.service is installed and running");
    } else {
        util::warn("runtime-orbit.service is not running (install it with `runtime-orbit.service install`)");
    }
    util::info("location", location);
}

pub async fn install() -> Result<()> {
    util::header("runtime-orbit.service install");
    platform::install().await
}
pub async fn uninstall() -> Result<()> {
    util::header("runtime-orbit.service uninstall");
    platform::uninstall().await
}
pub async fn status() -> Result<()> {
    util::header("runtime-orbit.service");
    platform::status().await
}
