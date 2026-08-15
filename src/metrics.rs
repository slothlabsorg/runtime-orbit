//! Machine + connection metrics for `status`, `dashboard` and the routing policy.
//!
//! Everything here is best-effort: a missing `vm_stat`, an unreachable donor or
//! a runtime that doesn't report memory degrades a field to `None`, never an
//! error. Nothing in this module should be able to fail a command.

use std::collections::HashMap;
use std::path::Path;

use bollard::container::{ListContainersOptions, StatsOptions};
use futures_util::StreamExt;
use owo_colors::OwoColorize;

use crate::config::Config;
use crate::forwarder;
use crate::ssh;
use crate::util;

// ── one machine's vitals ────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct MachineSpecs {
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub cores: usize,
    pub mem_total: u64,
    pub mem_avail: u64,
    pub load1: Option<f64>,
    pub ip: Option<String>,
    pub uptime: Option<String>,
    /// Cumulative bytes in/out on the primary interface, for traffic deltas.
    pub net_rx: Option<u64>,
    pub net_tx: Option<u64>,
}

impl MachineSpecs {
    pub fn mem_used(&self) -> u64 {
        self.mem_total.saturating_sub(self.mem_avail)
    }

    /// Fraction of RAM in use, 0.0–1.0. `None` when we couldn't read memory.
    pub fn mem_used_frac(&self) -> Option<f64> {
        if self.mem_total == 0 {
            return None;
        }
        Some(self.mem_used() as f64 / self.mem_total as f64)
    }
}

/// A POSIX probe that prints `key=value` lines. Used verbatim on this machine
/// (through `sh -c`) and on the donor (through `ssh`), so the two can't drift.
const VITALS: &str = r#"
printf 'hostname=%s\n' "$(hostname 2>/dev/null)"
printf 'arch=%s\n' "$(uname -m 2>/dev/null)"
case "$(uname -s)" in
Darwin)
  printf 'os=macOS %s\n' "$(sw_vers -productVersion 2>/dev/null)"
  printf 'cores=%s\n' "$(sysctl -n hw.ncpu 2>/dev/null)"
  printf 'memtotal=%s\n' "$(sysctl -n hw.memsize 2>/dev/null)"
  pagesize=$(sysctl -n hw.pagesize 2>/dev/null || echo 4096)
  vm=$(vm_stat 2>/dev/null)
  pages=$(printf '%s\n' "$vm" | awk '
    /Pages free/        {gsub("\\.","",$NF); f=$NF}
    /Pages inactive/    {gsub("\\.","",$NF); i=$NF}
    /Pages speculative/ {gsub("\\.","",$NF); s=$NF}
    END {print f+i+s}')
  [ -n "$pages" ] && printf 'memavail=%s\n' "$((pages * pagesize))"
  printf 'load=%s\n' "$(sysctl -n vm.loadavg 2>/dev/null | awk '{print $2}')"
  printf 'uptime=%s\n' "$(uptime 2>/dev/null | sed -e 's/^ *//' -e 's/,.*//')"
  ifc=$(route -n get default 2>/dev/null | awk '/interface:/{print $2}')
  if [ -n "$ifc" ]; then
    printf 'ip=%s\n' "$(ipconfig getifaddr "$ifc" 2>/dev/null)"
    netstat -I "$ifc" -b 2>/dev/null | awk 'NR==2 {printf "rx=%s\ntx=%s\n", $7, $10}'
  fi
  ;;
*)
  printf 'os=%s\n' "$( (. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME") || uname -sr )"
  printf 'cores=%s\n' "$(nproc 2>/dev/null)"
  awk '/^MemTotal:/ {printf "memtotal=%s\n", $2*1024} /^MemAvailable:/ {printf "memavail=%s\n", $2*1024}' /proc/meminfo 2>/dev/null
  printf 'load=%s\n' "$(awk '{print $1}' /proc/loadavg 2>/dev/null)"
  printf 'uptime=%s\n' "$(uptime -p 2>/dev/null)"
  ifc=$(ip route 2>/dev/null | awk '/^default/{print $5; exit}')
  if [ -n "$ifc" ]; then
    printf 'ip=%s\n' "$(ip -4 -o addr show "$ifc" 2>/dev/null | awk '{split($4,a,"/"); print a[1]}')"
    awk -v i="$ifc:" '$1==i {printf "rx=%s\ntx=%s\n", $2, $10}' /proc/net/dev 2>/dev/null
  fi
  ;;
esac
exit 0
"#;

fn parse_vitals(out: &str) -> MachineSpecs {
    let mut kv: HashMap<&str, &str> = HashMap::new();
    for line in out.lines() {
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim();
            if !v.is_empty() {
                kv.insert(k.trim(), v);
            }
        }
    }
    let num = |k: &str| kv.get(k).and_then(|v| v.parse::<u64>().ok());
    MachineSpecs {
        hostname: kv.get("hostname").unwrap_or(&"?").to_string(),
        os: kv.get("os").unwrap_or(&"?").to_string(),
        arch: kv.get("arch").unwrap_or(&"?").to_string(),
        cores: num("cores").unwrap_or(0) as usize,
        mem_total: num("memtotal").unwrap_or(0),
        mem_avail: num("memavail").unwrap_or(0),
        load1: kv.get("load").and_then(|v| v.parse::<f64>().ok()),
        ip: kv.get("ip").map(|s| s.to_string()),
        uptime: kv.get("uptime").map(|s| s.to_string()),
        net_rx: num("rx"),
        net_tx: num("tx"),
    }
}

/// Vitals for this machine.
pub async fn local_vitals() -> MachineSpecs {
    let out = util::run("sh", &["-c", VITALS]).await.unwrap_or_default();
    let mut s = parse_vitals(&out);
    if s.cores == 0 {
        s.cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0);
    }
    if s.os == "?" {
        s.os = std::env::consts::OS.to_string();
    }
    if s.arch == "?" {
        s.arch = std::env::consts::ARCH.to_string();
    }
    s
}

/// This machine's total RAM in GB, read synchronously. Returns `0.0` if unknown.
/// Used for sanity-checking budgets, where spawning the full probe is overkill.
pub fn local_vitals_blocking_total() -> f64 {
    let bytes = if let Ok(o) = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
    {
        String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse()
            .unwrap_or(0)
    } else {
        0u64
    };
    if bytes > 0 {
        return bytes as f64 / 1024.0 / 1024.0 / 1024.0;
    }
    if let Ok(txt) = std::fs::read_to_string("/proc/meminfo") {
        for line in txt.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                if let Some(kb) = rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    // kB → GiB
                    return kb as f64 / 1024.0 / 1024.0;
                }
            }
        }
    }
    0.0
}

/// Vitals for the donor, over one SSH round trip. `None` if SSH is down.
pub async fn donor_vitals(cfg: &Config) -> Option<MachineSpecs> {
    let out = ssh::remote_exec(cfg, VITALS).await.ok()?;
    if out.trim().is_empty() {
        return None;
    }
    let mut s = parse_vitals(&out);
    if s.ip.is_none() {
        s.ip = Some(cfg.donor_addr.clone());
    }
    Some(s)
}

// ── the container runtime on the other side ─────────────────────────────────

#[derive(Debug, Clone)]
pub struct RemoteMetrics {
    pub version: String,
    pub ncpu: i64,
    pub mem_total: i64,
    pub images: i64,
    pub containers: i64,
    pub running: i64,
    /// Memory currently used by running containers on the donor (bytes).
    pub offloaded_mem: u64,
    /// Summed CPU usage across running containers, in percent of one core.
    pub offloaded_cpu_pct: f64,
    /// Per-container detail, for the dashboard table.
    pub rows: Vec<ContainerRow>,
}

#[derive(Debug, Clone)]
pub struct ContainerRow {
    pub name: String,
    pub image: String,
    pub cpu_pct: f64,
    pub mem_bytes: u64,
    pub net_rx: u64,
    pub net_tx: u64,
    pub ports: Vec<u16>,
}

/// Query the donor's runtime through the forwarded socket. Best-effort.
pub async fn remote_metrics(socket: &Path) -> Option<RemoteMetrics> {
    let docker = forwarder::connect(socket).ok()?;
    let info = docker.info().await.ok()?;

    let mut offloaded_mem = 0u64;
    let mut offloaded_cpu_pct = 0.0f64;
    let mut rows: Vec<ContainerRow> = Vec::new();

    if let Ok(list) = docker
        .list_containers(Some(ListContainersOptions::<String> {
            all: false,
            ..Default::default()
        }))
        .await
    {
        for c in list.into_iter().take(40) {
            let Some(id) = c.id.clone() else { continue };
            let name = c
                .names
                .as_ref()
                .and_then(|n| n.first().cloned())
                .unwrap_or_else(|| id.chars().take(12).collect())
                .trim_start_matches('/')
                .to_string();
            let image = c.image.clone().unwrap_or_default();
            let ports: Vec<u16> = c
                .ports
                .clone()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|p| p.public_port)
                .collect();

            let mut stream = docker.stats(
                &id,
                Some(StatsOptions {
                    stream: false,
                    one_shot: true,
                }),
            );
            let (mut mem, mut cpu, mut rx, mut tx) = (0u64, 0.0f64, 0u64, 0u64);
            if let Some(Ok(s)) = stream.next().await {
                mem = s.memory_stats.usage.unwrap_or(0);
                cpu = cpu_percent(&s);
                if let Some(nets) = &s.networks {
                    for n in nets.values() {
                        rx += n.rx_bytes;
                        tx += n.tx_bytes;
                    }
                }
            }
            offloaded_mem += mem;
            offloaded_cpu_pct += cpu;
            rows.push(ContainerRow {
                name,
                image,
                cpu_pct: cpu,
                mem_bytes: mem,
                net_rx: rx,
                net_tx: tx,
                ports,
            });
        }
    }

    rows.sort_by_key(|c| std::cmp::Reverse(c.mem_bytes));

    Some(RemoteMetrics {
        version: info.server_version.unwrap_or_else(|| "?".into()),
        ncpu: info.ncpu.unwrap_or(0),
        mem_total: info.mem_total.unwrap_or(0),
        images: info.images.unwrap_or(0),
        containers: info.containers.unwrap_or(0),
        running: info.containers_running.unwrap_or(0),
        offloaded_mem,
        offloaded_cpu_pct,
        rows,
    })
}

/// Docker's own cpu% formula: container delta vs system delta, times cores.
///
/// A one-shot stat has no previous sample of its own, but the daemon still fills
/// `precpu_stats` from the container's last reading, so this is meaningful.
fn cpu_percent(s: &bollard::container::Stats) -> f64 {
    let cpu_delta =
        s.cpu_stats.cpu_usage.total_usage as f64 - s.precpu_stats.cpu_usage.total_usage as f64;
    let sys_delta = s.cpu_stats.system_cpu_usage.unwrap_or(0) as f64
        - s.precpu_stats.system_cpu_usage.unwrap_or(0) as f64;
    if cpu_delta <= 0.0 || sys_delta <= 0.0 {
        return 0.0;
    }
    let cores = s.cpu_stats.online_cpus.unwrap_or(1).max(1) as f64;
    (cpu_delta / sys_delta) * cores * 100.0
}

/// The donor's 1-minute load average. Kept for callers that only need this.
pub async fn remote_load(cfg: &Config) -> Option<String> {
    let out = ssh::remote_exec(cfg, "uptime").await.ok()?;
    let after = out.split("average").nth(1)?;
    let after = after.trim_start_matches('s').trim_start_matches(':').trim();
    let first = after
        .split(|c: char| c == ',' || c.is_whitespace())
        .find(|t| !t.is_empty())?;
    Some(first.to_string())
}

// ── formatting ──────────────────────────────────────────────────────────────

pub fn fmt_gib(bytes: i64) -> String {
    if bytes <= 0 {
        return "?".into();
    }
    fmt_gib_u(bytes as u64)
}

pub fn fmt_gib_u(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / 1024.0 / 1024.0 / 1024.0)
}

/// Human bytes for traffic counters: 1.2 GB / 340 MB / 12 kB.
pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [(&str, f64); 4] = [
        ("GB", 1024.0 * 1024.0 * 1024.0),
        ("MB", 1024.0 * 1024.0),
        ("kB", 1024.0),
        ("B", 1.0),
    ];
    for (unit, scale) in UNITS {
        if bytes as f64 >= scale {
            return format!("{:.1} {unit}", bytes as f64 / scale);
        }
    }
    "0 B".into()
}

/// Per-second rate from a byte delta over `secs`.
pub fn fmt_rate(delta: u64, secs: f64) -> String {
    if secs <= 0.0 {
        return "—".into();
    }
    format!("{}/s", fmt_bytes((delta as f64 / secs) as u64))
}

/// A fixed-width unicode meter, e.g. `████████░░░░  62%`.
pub fn bar(frac: f64, width: usize) -> String {
    let frac = frac.clamp(0.0, 1.0);
    let filled = (frac * width as f64).round() as usize;
    let full = "█".repeat(filled);
    let empty = "░".repeat(width.saturating_sub(filled));
    let pct = format!("{:>3.0}%", frac * 100.0);
    // Green under 60%, amber to 85%, red above — the usual traffic-light read.
    let colored = if frac < 0.60 {
        format!("{}{}", full.green(), empty.dimmed())
    } else if frac < 0.85 {
        format!("{}{}", full.yellow(), empty.dimmed())
    } else {
        format!("{}{}", full.red(), empty.dimmed())
    };
    format!("{colored} {pct}")
}

/// Compact header printed above the live log in `up --foreground`.
pub async fn print_dashboard(cfg: &Config) {
    let local = local_vitals().await;
    let socket = crate::config::local_docker_socket().ok();
    let remote = match &socket {
        Some(s) => remote_metrics(s).await,
        None => None,
    };
    let donor = donor_vitals(cfg).await;

    let bar_line = "─".repeat(72);
    println!("{}", bar_line.cyan());
    println!(
        "  {}  {} · {} · {} · {} cores · {} RAM",
        "this machine".bold(),
        local.hostname.white(),
        local.ip.clone().unwrap_or_else(|| "—".into()),
        local.os,
        local.cores,
        fmt_gib_u(local.mem_total),
    );

    if let Some(d) = &donor {
        println!(
            "  {}         {} · {} · {} · {} cores · {} RAM ({} free)",
            "donor".bold(),
            cfg.ssh_target().white(),
            d.ip.clone().unwrap_or_else(|| cfg.donor_addr.clone()),
            d.os,
            d.cores,
            fmt_gib_u(d.mem_total),
            fmt_gib_u(d.mem_avail),
        );
    } else {
        println!(
            "  {}         {} · (unreachable — check SSH)",
            "donor".bold(),
            cfg.ssh_target().white()
        );
    }

    println!(
        "  {}       docker → context `{}` · socket {}",
        "routing".bold(),
        cfg.context_name,
        cfg.remote_socket,
    );

    if let Some(r) = &remote {
        println!(
            "  {}       {} running · {} images · engine {} · {} cores available",
            "borrowed".bold().magenta(),
            r.running.to_string().magenta(),
            r.images,
            r.version,
            r.ncpu,
        );
        if r.offloaded_mem > 0 {
            println!(
                "  {}       {} of RAM carried by the donor instead of this machine {}",
                " ".repeat(7),
                fmt_gib_u(r.offloaded_mem).magenta(),
                "♥".magenta(),
            );
        }
    }
    println!("{}", bar_line.cyan());
    let _ = util::FUNDING_URL;
}
