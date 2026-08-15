//! Container-runtime detection.
//!
//! runtime-orbit doesn't care *which* runtime is installed, only that it speaks
//! the Docker Engine API over a unix socket — which Docker Desktop, OrbStack,
//! Rancher Desktop, colima, Lima, Podman and nerdctl/containerd all do. This
//! module finds the candidates and says which one is answering.
//!
//! The probe is a single POSIX snippet so the local and remote paths can't drift:
//! locally we run it through `sh -c`, on the donor we hand the same string to
//! `ssh`.

use crate::config::Config;
use crate::ssh;
use crate::util;

/// One runtime we found, and where its socket lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Engine {
    /// Human label, e.g. `OrbStack`.
    pub name: String,
    /// Absolute socket path.
    pub socket: String,
}

/// Sockets we look for, in priority order. `/var/run/docker.sock` first because
/// most engines symlink it, which makes it the most portable choice.
///
/// Emits `name<TAB>path<TAB>resolved`. The trailing `exit 0` matters: the loop's
/// last test is a socket that usually *doesn't* exist, and its non-zero status
/// would otherwise become the script's, which our command runner treats as
/// failure — the whole probe would come back empty.
const PROBE: &str = r#"
for entry in \
  "standard:/var/run/docker.sock" \
  "standard:/run/docker.sock" \
  "Docker Desktop:$HOME/.docker/run/docker.sock" \
  "OrbStack:$HOME/.orbstack/run/docker.sock" \
  "Rancher Desktop:$HOME/.rd/docker.sock" \
  "colima:$HOME/.colima/default/docker.sock" \
  "Lima:$HOME/.lima/default/sock/docker.sock" \
  "Podman:$HOME/.local/share/containers/podman/machine/podman.sock" \
  "Podman:/run/user/$(id -u 2>/dev/null)/podman/podman.sock" \
  "Podman:/run/podman/podman.sock" \
  "containerd:/run/containerd/containerd.sock" \
; do
  name=${entry%%:*}
  path=${entry#*:}
  if [ -S "$path" ]; then
    resolved=$(readlink "$path" 2>/dev/null)
    [ -z "$resolved" ] && resolved="$path"
    printf '%s\t%s\t%s\n' "$name" "$path" "$resolved"
  fi
done
exit 0
"#;

/// Which CLIs exist — used to explain "installed but not running".
const TOOLS: &str = r#"
for t in docker orbstack orb rdctl colima limactl podman nerdctl kubectl minikube k3s; do
  command -v "$t" >/dev/null 2>&1 && printf '%s\n' "$t"
done
exit 0
"#;

fn parse(out: &str) -> Vec<Engine> {
    let mut found: Vec<Engine> = Vec::new();
    let mut seen_resolved: Vec<String> = Vec::new();

    for line in out.lines() {
        let mut fields = line.split('\t');
        let (Some(name), Some(socket)) = (fields.next(), fields.next()) else {
            continue;
        };
        let (name, socket) = (name.trim(), socket.trim());
        if socket.is_empty() {
            continue;
        }
        // `/var/run/docker.sock` is usually a symlink to the real engine socket.
        // Dedupe on the target so one engine isn't listed twice, and use it to
        // name the engine behind the generic path.
        let resolved = fields
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(socket);
        if seen_resolved.iter().any(|s| s == resolved) {
            continue;
        }
        seen_resolved.push(resolved.to_string());

        let label = if name == "standard" {
            label_for_socket(resolved).to_string()
        } else {
            name.to_string()
        };
        found.push(Engine {
            name: label,
            socket: socket.to_string(),
        });
    }
    found
}

/// Runtimes with a live socket on this machine.
pub async fn detect_local() -> Vec<Engine> {
    let out = util::run("sh", &["-c", PROBE]).await.unwrap_or_default();
    parse(&out)
}

/// Runtimes with a live socket on the donor.
pub async fn detect_remote(cfg: &Config) -> Vec<Engine> {
    let out = ssh::remote_exec(cfg, PROBE).await.unwrap_or_default();
    parse(&out)
}

/// Runtime-related CLIs present on this machine.
pub async fn tools_local() -> Vec<String> {
    util::run("sh", &["-c", TOOLS])
        .await
        .unwrap_or_default()
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Runtime-related CLIs present on the donor.
pub async fn tools_remote(cfg: &Config) -> Vec<String> {
    ssh::remote_exec(cfg, TOOLS)
        .await
        .unwrap_or_default()
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The socket we'd pick on the donor: the first one the probe returned.
pub fn preferred(engines: &[Engine]) -> Option<&Engine> {
    engines.first()
}

/// Best-effort friendly name for whatever is behind a socket path, used when we
/// already know the path (from config) but not the product.
pub fn label_for_socket(socket: &str) -> &'static str {
    if socket.contains(".orbstack") {
        "OrbStack"
    } else if socket.contains(".rd/") {
        "Rancher Desktop"
    } else if socket.contains(".colima") {
        "colima"
    } else if socket.contains(".lima") {
        "Lima"
    } else if socket.contains("podman") {
        "Podman"
    } else if socket.contains("containerd") {
        "containerd"
    } else if socket.contains(".docker/run") {
        "Docker Desktop"
    } else {
        "Docker API"
    }
}
