//! `runtime-orbit dashboard` — one screen that answers "is this actually
//! helping?": both machines side by side, what's routed where, which containers
//! the donor is carrying, how much traffic the tunnel is moving, and how close
//! you are to your budgets.
//!
//! Redrawn in place on an interval. No TUI framework — a full repaint of ~30
//! lines is cheap and never leaves the terminal in a weird state on Ctrl-C.

use anyhow::Result;
use owo_colors::OwoColorize;
use serde_json::json;
use std::time::{Duration, Instant};

use crate::config::{self, Config};
use crate::docker_ctx;
use crate::engines;
use crate::forwarder;
use crate::metrics::{self, MachineSpecs, RemoteMetrics};
use crate::ssh;
use crate::util;

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
/// Width of each of the two machine columns.
const COL: usize = 38;

pub async fn run(interval: u64, once: bool, as_json: bool) -> Result<()> {
    let cfg = config::require_linked()?;
    let interval = interval.clamp(1, 3600);

    if as_json {
        let snap = Snapshot::gather(&cfg).await;
        println!("{}", serde_json::to_string_pretty(&snap.to_json(&cfg))?);
        return Ok(());
    }

    if once {
        let snap = Snapshot::gather(&cfg).await;
        print!("{}", snap.render(&cfg, None, interval));
        return Ok(());
    }

    // Alternate screen, hidden cursor — restored on every exit path below.
    print!("\x1b[?1049h\x1b[?25l");
    let result = live(&cfg, interval).await;
    print!("\x1b[?25h\x1b[?1049l");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    result
}

async fn live(cfg: &Config, interval: u64) -> Result<()> {
    let mut previous: Option<Snapshot> = None;
    loop {
        let snap = Snapshot::gather(cfg).await;
        let frame = snap.render(cfg, previous.as_ref(), interval);

        // Home the cursor and repaint, clearing each line as we go, so a shorter
        // frame can't leave debris from a taller one.
        print!("\x1b[H\x1b[2J{frame}");
        use std::io::Write;
        let _ = std::io::stdout().flush();

        previous = Some(snap);

        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
        }
    }
}

// ── one sample of the world ─────────────────────────────────────────────────

struct Snapshot {
    at: Instant,
    clock: String,
    local: MachineSpecs,
    donor: Option<MachineSpecs>,
    remote: Option<RemoteMetrics>,
    context: String,
    master_up: bool,
    forwarder_up: bool,
    ports: Vec<u16>,
    local_engine: Option<String>,
    local_running: usize,
}

impl Snapshot {
    async fn gather(cfg: &Config) -> Snapshot {
        let socket = config::local_docker_socket().ok();

        // The donor probe is an SSH round trip and the runtime probe is a socket
        // round trip; run them together so the refresh feels instant.
        let (local, donor, remote, context, master_up) = tokio::join!(
            metrics::local_vitals(),
            metrics::donor_vitals(cfg),
            async {
                match &socket {
                    Some(s) => metrics::remote_metrics(s).await,
                    None => None,
                }
            },
            async { docker_ctx::current_context().await.unwrap_or_default() },
            ssh::master_alive(cfg),
        );

        let ports = match &socket {
            Some(s) => forwarder::list_published(s).await.unwrap_or_default(),
            None => Vec::new(),
        };

        let local_engines = engines::detect_local().await;
        let local_engine = engines::preferred(&local_engines).map(|e| e.name.clone());

        Snapshot {
            at: Instant::now(),
            clock: util::run("date", &["+%H:%M:%S"]).await.unwrap_or_default(),
            local,
            donor,
            remote,
            context,
            master_up,
            forwarder_up: forwarder_alive(),
            ports,
            local_engine,
            local_running: local_container_count().await,
        }
    }

    fn to_json(&self, cfg: &Config) -> serde_json::Value {
        let machine = |m: &MachineSpecs| {
            json!({
                "hostname": m.hostname, "os": m.os, "arch": m.arch, "cores": m.cores,
                "ip": m.ip, "load1": m.load1, "uptime": m.uptime,
                "mem_total_bytes": m.mem_total, "mem_available_bytes": m.mem_avail,
                "mem_used_bytes": m.mem_used(),
            })
        };
        json!({
            "borrower": machine(&self.local),
            "donor": self.donor.as_ref().map(machine),
            "connection": {
                "target": cfg.ssh_target(),
                "ssh_port": cfg.ssh_port,
                "docker_context": self.context,
                "expected_context": cfg.context_name,
                "routed_to_donor": self.context == cfg.context_name,
                "ssh_master": self.master_up,
                "forwarder": self.forwarder_up,
                "remote_socket": cfg.remote_socket,
            },
            "borrowed": self.remote.as_ref().map(|r| json!({
                "engine_version": r.version,
                "cores": r.ncpu,
                "mem_total_bytes": r.mem_total,
                "containers_running": r.running,
                "images": r.images,
                "mem_used_by_containers_bytes": r.offloaded_mem,
                "cpu_percent_by_containers": r.offloaded_cpu_pct,
                "containers": r.rows.iter().map(|c| json!({
                    "name": c.name, "image": c.image, "cpu_percent": c.cpu_pct,
                    "mem_bytes": c.mem_bytes, "net_rx_bytes": c.net_rx,
                    "net_tx_bytes": c.net_tx, "ports": c.ports,
                })).collect::<Vec<_>>(),
            })),
            "forwarded_ports": self.ports,
            "limits": {
                "max_borrow_ram_gb": cfg.limits.max_borrow_ram_gb,
                "local_ram_threshold_gb": cfg.limits.local_ram_threshold_gb,
                "local_load_threshold": cfg.limits.local_load_threshold,
                "prefer": cfg.limits.prefer,
            },
        })
    }

    fn render(&self, cfg: &Config, prev: Option<&Snapshot>, interval: u64) -> String {
        let mut o = String::new();
        let secs = prev.map(|p| (self.at - p.at).as_secs_f64()).unwrap_or(0.0);

        // ── title ──
        let state = if self.context == cfg.context_name && self.master_up {
            "borrowing".green().bold().to_string()
        } else if self.master_up {
            "connected · docker is local".yellow().to_string()
        } else {
            "not connected".red().to_string()
        };
        o.push_str(&format!(
            "{}  {}   {}\n\n",
            "runtime-orbit".bold(),
            state,
            self.clock.dimmed()
        ));

        // ── two machines, side by side ──
        o.push_str(&format!(
            "  {}{}\n",
            pad(&"THIS MACHINE".bold().to_string(), COL),
            "DONOR".bold()
        ));
        let donor_missing = MachineSpecs {
            hostname: cfg.ssh_target(),
            os: "unreachable".into(),
            ..Default::default()
        };
        let d = self.donor.as_ref().unwrap_or(&donor_missing);

        let rows: Vec<(String, String)> = vec![
            (self.local.hostname.clone(), cfg.ssh_target()),
            (
                format!("{} · {}", self.local.os, self.local.arch),
                format!("{} · {}", d.os, d.arch),
            ),
            (
                format!(
                    "{} · {} cores",
                    self.local.ip.clone().unwrap_or_else(|| "—".into()),
                    self.local.cores
                ),
                format!(
                    "{} · {} cores",
                    d.ip.clone().unwrap_or_else(|| cfg.donor_addr.clone()),
                    d.cores
                ),
            ),
        ];
        for (l, r) in rows {
            o.push_str(&format!("  {}{}\n", pad(&l, COL), r));
        }

        // RAM meters — the number everyone actually came for.
        let lmem = mem_cell(&self.local);
        let rmem = mem_cell(d);
        o.push_str(&format!("  {}{}\n", pad(&lmem, COL), rmem));

        let lload = load_cell(&self.local);
        let rload = load_cell(d);
        o.push_str(&format!("  {}{}\n", pad(&lload, COL), rload));

        let lengine = format!(
            "engine {}",
            self.local_engine.clone().unwrap_or_else(|| "none".into())
        );
        let rengine = match &self.remote {
            Some(r) => format!(
                "engine {} · {}",
                engines::label_for_socket(&cfg.remote_socket),
                r.version
            ),
            None => "engine —".to_string(),
        };
        o.push_str(&format!(
            "  {}{}\n\n",
            pad(&lengine.dimmed().to_string(), COL),
            rengine.dimmed()
        ));

        // ── routing ──
        o.push_str(&format!("  {}\n", "ROUTING".bold()));
        let ctx_state = if self.context == cfg.context_name {
            format!("{} {}", self.context, "→ donor".green())
        } else {
            format!("{} {}", self.context, "→ this machine".yellow())
        };
        o.push_str(&kv("docker context", &ctx_state));
        o.push_str(&kv(
            "ssh",
            &format!(
                "{}:{} · master {} · forwarder {}",
                cfg.ssh_target(),
                cfg.ssh_port,
                onoff(self.master_up),
                onoff(self.forwarder_up)
            ),
        ));
        o.push_str(&kv(
            "ports",
            &if self.ports.is_empty() {
                "none published".dimmed().to_string()
            } else {
                self.ports
                    .iter()
                    .map(|p| format!("localhost:{p}"))
                    .collect::<Vec<_>>()
                    .join("  ")
            },
        ));
        o.push('\n');

        // ── what the donor is carrying for us ──
        if let Some(r) = &self.remote {
            o.push_str(&format!("  {}\n", "BORROWED RIGHT NOW".bold().magenta()));
            o.push_str(&kv(
                "carried by donor",
                &format!(
                    "{} · {:.0}% CPU · {} container(s)",
                    metrics::fmt_gib_u(r.offloaded_mem).magenta(),
                    r.offloaded_cpu_pct,
                    r.running
                ),
            ));
            o.push_str(&kv(
                "on this machine",
                &format!("{} container(s)", self.local_running),
            ));
            o.push('\n');

            if !r.rows.is_empty() {
                o.push_str(&format!(
                    "  {:<22} {:<26} {:>7} {:>10} {:>9}\n",
                    "CONTAINER".bold(),
                    "IMAGE".bold(),
                    "CPU".bold(),
                    "MEM".bold(),
                    "PORTS".bold()
                ));
                for c in r.rows.iter().take(8) {
                    o.push_str(&format!(
                        "  {:<22} {:<26} {:>6.1}% {:>10} {:>9}\n",
                        trunc(&c.name, 22),
                        trunc(&c.image, 26),
                        c.cpu_pct,
                        metrics::fmt_gib_u(c.mem_bytes),
                        if c.ports.is_empty() {
                            "—".to_string()
                        } else {
                            c.ports
                                .iter()
                                .map(|p| p.to_string())
                                .collect::<Vec<_>>()
                                .join(",")
                        }
                    ));
                }
                if r.rows.len() > 8 {
                    o.push_str(&format!(
                        "  {}\n",
                        format!("… and {} more", r.rows.len() - 8).dimmed()
                    ));
                }
                o.push('\n');
            }
        }

        // ── traffic ──
        o.push_str(&format!("  {}\n", "TRAFFIC".bold()));
        let tunnel = match (&self.remote, prev.and_then(|p| p.remote.as_ref())) {
            (Some(now), Some(before)) if secs > 0.0 => {
                let (rx, tx) = (sum_rx(now), sum_tx(now));
                let (prx, ptx) = (sum_rx(before), sum_tx(before));
                format!(
                    "↓ {}   ↑ {}   {}",
                    metrics::fmt_rate(rx.saturating_sub(prx), secs),
                    metrics::fmt_rate(tx.saturating_sub(ptx), secs),
                    format!(
                        "({} in / {} out total)",
                        metrics::fmt_bytes(rx),
                        metrics::fmt_bytes(tx)
                    )
                    .dimmed()
                )
            }
            (Some(now), _) => format!(
                "{} in / {} out total {}",
                metrics::fmt_bytes(sum_rx(now)),
                metrics::fmt_bytes(sum_tx(now)),
                "(rates on next refresh)".dimmed()
            ),
            _ => "—".to_string(),
        };
        o.push_str(&kv("containers", &tunnel));

        let nic = match (
            self.donor.as_ref().and_then(|d| d.net_rx),
            self.donor.as_ref().and_then(|d| d.net_tx),
            prev.and_then(|p| p.donor.as_ref().and_then(|d| d.net_rx)),
            prev.and_then(|p| p.donor.as_ref().and_then(|d| d.net_tx)),
        ) {
            (Some(rx), Some(tx), Some(prx), Some(ptx)) if secs > 0.0 => format!(
                "↓ {}   ↑ {}",
                metrics::fmt_rate(rx.saturating_sub(prx), secs),
                metrics::fmt_rate(tx.saturating_sub(ptx), secs)
            ),
            (Some(rx), Some(tx), _, _) => format!(
                "{} in / {} out since boot",
                metrics::fmt_bytes(rx),
                metrics::fmt_bytes(tx)
            ),
            _ => "—".to_string(),
        };
        o.push_str(&kv("donor network", &nic));
        o.push('\n');

        // ── budgets ──
        let l = &cfg.limits;
        if l.max_borrow_ram_gb.is_some()
            || l.local_ram_threshold_gb.is_some()
            || l.local_load_threshold.is_some()
        {
            o.push_str(&format!("  {}\n", "BUDGETS".bold()));
            if let (Some(cap), Some(r)) = (l.max_borrow_ram_gb, self.remote.as_ref()) {
                let used = r.offloaded_mem as f64 / GB;
                o.push_str(&kv(
                    "borrow ceiling",
                    &format!(
                        "{}  {:.1} / {:.1} GB",
                        metrics::bar(used / cap, 16),
                        used,
                        cap
                    ),
                ));
            }
            if let Some(t) = l.local_ram_threshold_gb {
                let used = self.local.mem_used() as f64 / GB;
                let verdict = if used >= t {
                    "new work → donor".green().to_string()
                } else {
                    "new work stays here".dimmed().to_string()
                };
                o.push_str(&kv(
                    "local RAM budget",
                    &format!(
                        "{}  {:.1} / {:.1} GB  {}",
                        metrics::bar(used / t, 16),
                        used,
                        t,
                        verdict
                    ),
                ));
            }
            if let (Some(t), Some(load)) = (l.local_load_threshold, self.local.load1) {
                o.push_str(&kv(
                    "local load budget",
                    &format!("{}  {:.2} / {:.2}", metrics::bar(load / t, 16), load, t),
                ));
            }
            o.push_str(&kv("prefer", &l.prefer));
            if !cfg.routes.is_empty() {
                o.push_str(&kv(
                    "routing rules",
                    &format!("{} — see `runtime-orbit route list`", cfg.routes.len()),
                ));
            }
            o.push('\n');
        }

        // ── savings, the reason any of this exists ──
        if let Some(r) = &self.remote {
            if r.offloaded_mem > 0 {
                o.push_str(&format!(
                    "  {} {} of RAM and {:.0}% CPU are on {} instead of here {}\n",
                    "→".magenta(),
                    metrics::fmt_gib_u(r.offloaded_mem).bold().magenta(),
                    r.offloaded_cpu_pct,
                    cfg.donor_addr,
                    "♥".magenta()
                ));
                if self.local.mem_total > 0 {
                    let would_be = (self.local.mem_used() + r.offloaded_mem) as f64 / GB;
                    let total = self.local.mem_total as f64 / GB;
                    if would_be > total * 0.9 {
                        o.push_str(&format!(
                            "  {} without the donor this machine would need ~{:.1} GB of its {:.1} GB\n",
                            "→".yellow(),
                            would_be,
                            total
                        ));
                    }
                }
            }
        }

        o.push_str(&format!(
            "\n  {}\n",
            format!("Ctrl-C to exit · refreshing every {interval}s").dimmed()
        ));
        o
    }
}

// ── cells & helpers ─────────────────────────────────────────────────────────

fn mem_cell(m: &MachineSpecs) -> String {
    match m.mem_used_frac() {
        Some(frac) => format!(
            "RAM {}  {:.1}/{:.1} GB",
            metrics::bar(frac, 10),
            m.mem_used() as f64 / GB,
            m.mem_total as f64 / GB
        ),
        None => "RAM —".to_string(),
    }
}

fn load_cell(m: &MachineSpecs) -> String {
    let load = m
        .load1
        .map(|l| format!("load {l:.2}"))
        .unwrap_or_else(|| "load —".into());
    match &m.uptime {
        Some(u) => format!("{load} · {}", trunc(u, 22)),
        None => load,
    }
}

fn kv(label: &str, value: &str) -> String {
    format!("  {:<18} {}\n", label.dimmed(), value)
}

fn onoff(up: bool) -> String {
    if up {
        "up".green().to_string()
    } else {
        "down".red().to_string()
    }
}

fn sum_rx(r: &RemoteMetrics) -> u64 {
    r.rows.iter().map(|c| c.net_rx).sum()
}
fn sum_tx(r: &RemoteMetrics) -> u64 {
    r.rows.iter().map(|c| c.net_tx).sum()
}

/// Visible width of a string, ignoring ANSI escape sequences.
fn vis_len(s: &str) -> usize {
    let mut n = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for x in chars.by_ref() {
                if x.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            n += 1;
        }
    }
    n
}

/// Pad to `width` visible columns — ANSI-aware, so coloured cells still line up.
fn pad(s: &str, width: usize) -> String {
    let len = vis_len(s);
    if len >= width {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(width - len))
}

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    format!("{}…", s.chars().take(keep).collect::<String>())
}

fn forwarder_alive() -> bool {
    let Ok(path) = config::pid_file() else {
        return false;
    };
    let Ok(pid) = std::fs::read_to_string(&path) else {
        return false;
    };
    let pid = pid.trim();
    !pid.is_empty()
        && std::process::Command::new("kill")
            .args(["-0", pid])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

/// Containers running on *this* machine's own engine, for the comparison. We ask
/// a specific context so the answer doesn't change when docker is routed away.
async fn local_container_count() -> usize {
    let out = util::run("docker", &["--context", "default", "ps", "-q"])
        .await
        .unwrap_or_default();
    out.lines().filter(|l| !l.trim().is_empty()).count()
}
