//! The routing policy: given a workload, decide whether it runs on this machine
//! or on the donor — and be able to explain why.
//!
//! Evaluation order (first thing that decides, wins):
//!
//! 1. **Routing table** — explicit rules, top to bottom. `postgres:*  → local`
//!    keeps your database on the SSD you actually own.
//! 2. **Local thresholds** — once this machine has `local_ram_threshold_gb` of
//!    RAM in use (or a load average past `local_load_threshold`), new work goes
//!    to the donor. Below the threshold, work stays here: a 20 MB alpine
//!    container isn't worth a network hop.
//! 3. **Borrow ceiling** — if sending it over would push past
//!    `max_borrow_ram_gb`, it stays local instead. A budget you can't exceed is
//!    the only kind that means anything.
//! 4. **`prefer`** — the tiebreak. `auto` (default) delegates when no threshold
//!    is configured, and keeps work local when one is configured but untripped.

use crate::config::{Config, Limits};
use crate::metrics::{self, MachineSpecs, RemoteMetrics};

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Local,
    Donor,
}

impl Target {
    pub fn as_str(&self) -> &'static str {
        match self {
            Target::Local => "local",
            Target::Donor => "donor",
        }
    }

    pub fn parse(s: &str) -> Option<Target> {
        match s.trim().to_ascii_lowercase().as_str() {
            "local" | "here" | "this" => Some(Target::Local),
            "donor" | "donator" | "remote" | "lender" | "host" => Some(Target::Donor),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub target: Target,
    /// Human sentence explaining the choice, shown by `route explain`.
    pub reason: String,
    /// Which rule matched, if any (1-based, as printed by `route list`).
    pub rule: Option<usize>,
}

/// Inputs the policy reads. Gathered once so `explain` and `docker` agree.
pub struct Snapshot {
    pub local: MachineSpecs,
    pub remote: Option<RemoteMetrics>,
}

impl Snapshot {
    pub async fn gather() -> Snapshot {
        let local = metrics::local_vitals().await;
        let remote = match crate::config::local_docker_socket() {
            Ok(sock) => metrics::remote_metrics(&sock).await,
            Err(_) => None,
        };
        Snapshot { local, remote }
    }
}

/// Decide where `workload` (an image reference or container name) should run.
pub fn decide(cfg: &Config, workload: &str, snap: &Snapshot) -> Decision {
    let limits = &cfg.limits;

    // 1. Explicit rules win, in order.
    for (i, route) in cfg.routes.iter().enumerate() {
        if glob_match(&route.pattern, workload) {
            if let Some(target) = Target::parse(&route.target) {
                return Decision {
                    target,
                    reason: format!(
                        "rule #{} `{}` → {}{}",
                        i + 1,
                        route.pattern,
                        target.as_str(),
                        route
                            .note
                            .as_ref()
                            .map(|n| format!(" ({n})"))
                            .unwrap_or_default()
                    ),
                    rule: Some(i + 1),
                };
            }
        }
    }

    // 2. Local pressure thresholds.
    let used_gb = snap.local.mem_used() as f64 / GB;
    if let Some(t) = limits.local_ram_threshold_gb {
        if used_gb >= t {
            return with_ceiling(
                cfg,
                snap,
                Target::Donor,
                format!("local RAM in use {used_gb:.1} GB is at or past the {t:.1} GB budget"),
            );
        }
    }
    if let Some(t) = limits.local_load_threshold {
        if let Some(load) = snap.local.load1 {
            if load >= t {
                return with_ceiling(
                    cfg,
                    snap,
                    Target::Donor,
                    format!("local load {load:.2} is at or past the {t:.2} threshold"),
                );
            }
        }
    }

    // 3. No rule, no tripped threshold — fall back to `prefer`.
    match limits.prefer.as_str() {
        "local" => Decision {
            target: Target::Local,
            reason: "prefer = local".into(),
            rule: None,
        },
        "donor" => with_ceiling(cfg, snap, Target::Donor, "prefer = donor".into()),
        // auto
        _ => {
            if has_thresholds(limits) {
                Decision {
                    target: Target::Local,
                    reason: format!(
                        "under the local budget ({used_gb:.1} GB in use) — no need to borrow yet"
                    ),
                    rule: None,
                }
            } else {
                with_ceiling(
                    cfg,
                    snap,
                    Target::Donor,
                    "no local budget configured — delegating everything (the default)".into(),
                )
            }
        }
    }
}

fn has_thresholds(l: &Limits) -> bool {
    l.local_ram_threshold_gb.is_some() || l.local_load_threshold.is_some()
}

/// Apply the borrow ceiling to a decision that wants the donor. If the donor is
/// already carrying `max_borrow_ram_gb` for us, the work stays here.
fn with_ceiling(cfg: &Config, snap: &Snapshot, target: Target, reason: String) -> Decision {
    if target == Target::Donor {
        if let (Some(cap), Some(r)) = (cfg.limits.max_borrow_ram_gb, snap.remote.as_ref()) {
            let borrowed_gb = r.offloaded_mem as f64 / GB;
            if borrowed_gb >= cap {
                return Decision {
                    target: Target::Local,
                    reason: format!(
                        "would exceed the borrow ceiling — already using {borrowed_gb:.1} GB of the {cap:.1} GB cap"
                    ),
                    rule: None,
                };
            }
        }
    }
    Decision {
        target,
        reason,
        rule: None,
    }
}

/// Extract the thing a `docker` invocation is really about, for rule matching:
/// the image for `run`/`create`/`pull`, the tag for `build -t`, otherwise the
/// subcommand itself (so a rule can say `compose → donor`).
pub fn workload_from_args(args: &[String]) -> String {
    let mut it = args.iter().filter(|a| !a.starts_with('-'));
    let sub = it.next().cloned().unwrap_or_default();

    match sub.as_str() {
        "run" | "create" => image_after_flags(args, &sub).unwrap_or(sub),
        "pull" | "push" => it.next().cloned().unwrap_or(sub),
        "build" | "buildx" => {
            // `-t name:tag` is the most identifying thing a build has.
            args.windows(2)
                .find(|w| w[0] == "-t" || w[0] == "--tag")
                .map(|w| w[1].clone())
                .unwrap_or(sub)
        }
        _ => sub,
    }
}

/// The image argument of `docker run`: the first bare word after the subcommand
/// that isn't a flag or a flag's value.
fn image_after_flags(args: &[String], sub: &str) -> Option<String> {
    // Flags that consume the next argument. Anything else is either a boolean
    // flag, an `--opt=value`, or the image itself.
    const TAKES_VALUE: &[&str] = &[
        "-p",
        "--publish",
        "-v",
        "--volume",
        "-e",
        "--env",
        "--name",
        "-w",
        "--workdir",
        "--net",
        "--network",
        "-u",
        "--user",
        "--entrypoint",
        "-l",
        "--label",
        "--mount",
        "--memory",
        "-m",
        "--cpus",
        "--restart",
        "--platform",
        "--env-file",
        "--add-host",
        "--device",
        "--link",
        "--expose",
        "--health-cmd",
        "--pull",
        "--gpus",
        "--shm-size",
        "--tmpfs",
        "--ulimit",
        "--log-driver",
        "--ipc",
        "--pid",
        "--userns",
        "--cap-add",
        "--cap-drop",
        "--hostname",
        "-h",
    ];

    let start = args.iter().position(|a| a == sub)? + 1;
    let mut i = start;
    while i < args.len() {
        let a = &args[i];
        if a.starts_with('-') {
            if TAKES_VALUE.contains(&a.as_str()) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        return Some(a.clone());
    }
    None
}

/// Glob match supporting `*` (any run, including empty) and `?` (one char).
/// Case-insensitive, because image names in the wild are not consistent.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let t: Vec<char> = text.to_ascii_lowercase().chars().collect();
    is_match(&p, &t)
}

fn is_match(p: &[char], t: &[char]) -> bool {
    match p.first() {
        None => t.is_empty(),
        Some('*') => {
            // Either the star absorbs nothing, or it absorbs one more char.
            is_match(&p[1..], t) || (!t.is_empty() && is_match(p, &t[1..]))
        }
        Some('?') => !t.is_empty() && is_match(&p[1..], &t[1..]),
        Some(c) => t.first() == Some(c) && is_match(&p[1..], &t[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("postgres:*", "postgres:16"));
        assert!(glob_match("*redis*", "docker.io/library/redis:7"));
        assert!(glob_match("nginx", "NGINX"));
        assert!(!glob_match("postgres:*", "mysql:8"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("my-?pp", "my-app"));
    }

    #[test]
    fn finds_run_image() {
        let args: Vec<String> = "run -d -p 8080:80 --name web nginx:alpine"
            .split(' ')
            .map(String::from)
            .collect();
        assert_eq!(workload_from_args(&args), "nginx:alpine");
    }

    #[test]
    fn finds_build_tag() {
        let args: Vec<String> = "build -t acme/api:dev ."
            .split(' ')
            .map(String::from)
            .collect();
        assert_eq!(workload_from_args(&args), "acme/api:dev");
    }

    #[test]
    fn falls_back_to_subcommand() {
        let args: Vec<String> = vec!["compose".into(), "up".into()];
        assert_eq!(workload_from_args(&args), "compose");
    }
}
