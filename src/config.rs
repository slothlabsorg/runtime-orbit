//! Persistent config at `~/.runtime-orbit/config.toml` plus derived runtime paths.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::host::HostKind;

pub const DEFAULT_CONTEXT: &str = "runtime-orbit";

/// Contexts written by earlier versions, still switched away from on `down`.
pub const LEGACY_CONTEXTS: &[&str] = &["orbit"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// SSH user on the donor (e.g. `dany`).
    #[serde(alias = "host_user")]
    pub donor_user: String,
    /// Donor address — IP or resolvable name on the LAN.
    #[serde(alias = "host_addr")]
    pub donor_addr: String,
    /// SSH port on the donor.
    #[serde(default = "default_ssh_port")]
    pub ssh_port: u16,
    /// Which adapter exposes the donor's runtime socket.
    pub adapter: HostKind,
    /// Absolute path to the runtime socket on the donor.
    pub remote_socket: String,
    /// Name of the docker context runtime-orbit manages.
    #[serde(default = "default_context")]
    pub context_name: String,
    /// Docker context that was active before `up`, restored on `down`.
    #[serde(default)]
    pub previous_context: Option<String>,
    /// RAM/CPU budgets.
    #[serde(default)]
    pub limits: Limits,
    /// Routing table, evaluated top to bottom — first match wins.
    #[serde(default)]
    pub routes: Vec<Route>,
}

fn default_ssh_port() -> u16 {
    22
}
fn default_context() -> String {
    DEFAULT_CONTEXT.to_string()
}

/// How much of the donor we're willing to use, and when to stop using this
/// machine. All optional: unset means "no limit / no threshold".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Limits {
    /// Ceiling on donor RAM we'll lean on, in GB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_borrow_ram_gb: Option<f64>,
    /// Once this much RAM is in use on *this* machine, route new work to the donor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_ram_threshold_gb: Option<f64>,
    /// Once the local 1-min load average exceeds this, route new work to the donor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_load_threshold: Option<f64>,
    /// Fallback target when no rule and no threshold decides: auto | local | donor.
    #[serde(default = "default_prefer")]
    pub prefer: String,
}

fn default_prefer() -> String {
    "auto".to_string()
}

/// One routing-table row. `pattern` is a glob matched against the image
/// reference and the container name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub pattern: String,
    /// `local` or `donor`.
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Config {
    /// A fresh config pointing at a donor, with everything else defaulted.
    pub fn for_donor(user: &str, addr: &str, ssh_port: u16) -> Self {
        Config {
            donor_user: user.to_string(),
            donor_addr: addr.to_string(),
            ssh_port,
            adapter: HostKind::Unix,
            remote_socket: "/var/run/docker.sock".into(),
            context_name: DEFAULT_CONTEXT.to_string(),
            previous_context: None,
            limits: Limits::default(),
            routes: Vec::new(),
        }
    }

    /// `user@host` target string used by ssh/docker.
    pub fn ssh_target(&self) -> String {
        format!("{}@{}", self.donor_user, self.donor_addr)
    }

    /// Docker context endpoint: the locally-forwarded unix socket.
    ///
    /// We deliberately do NOT use `ssh://user@host` — that makes docker run
    /// `docker system dial-stdio` on the remote, which needs the `docker` binary
    /// on the remote's non-interactive SSH PATH (a common breakage). Instead we
    /// forward the remote daemon socket and point docker straight at it.
    pub fn docker_endpoint(&self) -> String {
        match local_docker_socket() {
            Ok(p) => format!("unix://{}", p.display()),
            Err(_) => "unix://<unavailable>".to_string(),
        }
    }

    pub fn load() -> Result<Self> {
        migrate_legacy_dir();
        let path = config_path()?;
        let raw = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "no runtime-orbit config at {} — run `runtime-orbit setup --ip <donor-ip>` first",
                path.display()
            )
        })?;
        toml::from_str(&raw).context("config.toml is malformed")
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = toml::to_string_pretty(self)?;
        std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

// ---- paths -----------------------------------------------------------------

/// `~/.runtime-orbit` — everything lives here. We avoid the platform config dir
/// on purpose: macOS's `~/Library/Application Support` contains a space, which
/// is hostile to unix socket paths (`unix://…` endpoints, `ssh -L` specs).
pub fn config_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".runtime-orbit"))
}

/// Pre-0.2 location. Moved into place on first use if the new dir is absent.
fn legacy_config_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".orbit"))
}

/// Adopt a pre-0.2 `~/.orbit` directory so upgrades keep their link and key.
/// Best-effort and idempotent: only runs when the new dir doesn't exist yet.
pub fn migrate_legacy_dir() {
    let (Ok(new), Some(old)) = (config_dir(), legacy_config_dir()) else {
        return;
    };
    if new.exists() || !old.exists() {
        return;
    }
    if std::fs::rename(&old, &new).is_ok() {
        // Leave a breadcrumb so anyone poking at ~/.orbit knows where it went.
        let _ = std::fs::create_dir_all(&old);
        let _ = std::fs::write(
            old.join("MOVED.txt"),
            format!("runtime-orbit 0.2 moved this to {}\n", new.display()),
        );
    }
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// `~/.runtime-orbit/run` — sockets, pidfiles, logs. Created on demand.
pub fn run_dir() -> Result<PathBuf> {
    let dir = config_dir()?.join("run");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// SSH ControlMaster control socket.
pub fn control_socket() -> Result<PathBuf> {
    Ok(run_dir()?.join("control.sock"))
}

/// Local unix socket where the donor's runtime socket is forwarded.
pub fn local_docker_socket() -> Result<PathBuf> {
    Ok(run_dir()?.join("docker.sock"))
}

/// PID of the running forwarder.
pub fn pid_file() -> Result<PathBuf> {
    Ok(run_dir()?.join("runtime-orbit.pid"))
}

/// Forwarder log file.
pub fn log_path() -> Result<PathBuf> {
    Ok(run_dir()?.join("runtime-orbit.log"))
}

/// Path to the runtime-orbit-managed SSH key pair.
pub fn ssh_key_path() -> Result<PathBuf> {
    let dir = config_dir()?.join("keys");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("id_orbit_ed25519"))
}

/// Fail early with a friendly message if config is missing.
pub fn require_linked() -> Result<Config> {
    match Config::load() {
        Ok(c) => Ok(c),
        Err(_) => bail!(
            "this machine isn't linked to a donor yet — run `runtime-orbit setup --ip <donor-ip>`"
        ),
    }
}
