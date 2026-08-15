//! `runtime-orbit doctor` — the borrower's health check. Every line is a check
//! with a ✓ / ✗ / ! and a fix. All green means borrowing will work.

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::config::{self, Config};
use crate::docker_ctx;
use crate::engines;
use crate::forwarder;
use crate::metrics;
use crate::ssh;
use crate::util;

pub async fn run() -> Result<()> {
    util::header("runtime-orbit doctor");
    println!("  Checking this machine's ability to borrow a runtime.\n");
    let mut problems = 0u32;

    // [✓] Docker CLI — we route it, so it has to exist here.
    if util::succeeds("docker", &["version", "--format", "{{.Client.Version}}"]).await {
        let v = util::run("docker", &["version", "--format", "{{.Client.Version}}"])
            .await
            .unwrap_or_default();
        pass(&format!("Docker CLI ({})", v.trim()));
    } else {
        problems += 1;
        fail(
            "Docker CLI not found",
            "install the docker CLI (Docker Desktop, OrbStack, Rancher Desktop or colima all ship it)",
        );
    }

    // [✓] Linked to a donor.
    let cfg = match Config::load() {
        Ok(c) => {
            pass(&format!("Linked to donor {}", c.ssh_target()));
            c
        }
        Err(_) => {
            fail(
                "Not linked to a donor",
                "run `runtime-orbit setup --ip <donor-ip>` — it does everything, including \
                 authorizing this machine",
            );
            return verdict(problems + 1, None);
        }
    };

    // [✓] SSH.
    if ssh::test_connection(&cfg).await.is_ok() {
        pass("SSH to the donor works");
    } else {
        problems += 1;
        fail(
            "Cannot SSH to the donor",
            "check the donor is awake and lending (`runtime-orbit donor doctor` there), then \
             re-run `runtime-orbit setup --ip <donor-ip>` here to re-authorize",
        );
    }

    // [✓] The donor's runtime socket.
    let socket_ok = ssh::remote_exec(&cfg, &format!("test -S {} && echo yes", cfg.remote_socket))
        .await
        .map(|o| o.trim() == "yes")
        .unwrap_or(false);
    if socket_ok {
        pass(&format!(
            "Donor runtime reachable — {} ({})",
            engines::label_for_socket(&cfg.remote_socket),
            cfg.remote_socket
        ));
    } else {
        problems += 1;
        // Say which sockets *do* exist there; "not found" alone isn't actionable.
        let found = engines::detect_remote(&cfg).await;
        if let Some(e) = engines::preferred(&found) {
            fail(
                &format!(
                    "The socket we're configured for is gone ({})",
                    cfg.remote_socket
                ),
                &format!(
                    "the donor now has {} at {} — re-run `runtime-orbit link {}` to pick it up",
                    e.name,
                    e.socket,
                    cfg.ssh_target()
                ),
            );
        } else {
            fail(
                "No runtime running on the donor",
                "start Docker/OrbStack/Rancher/Podman there, or run `runtime-orbit donor doctor` \
                 on it for the full picture",
            );
        }
    }

    // [!] Port forwarding caveat for Windows donors.
    if !cfg.adapter.supports_socket_forward() {
        note(
            &format!(
                "Donor is {} — automatic port forwarding is limited",
                cfg.adapter
            ),
            "run `runtime-orbit donor setup` inside the WSL2 distro instead, or add ports \
             manually with `runtime-orbit ports add <port>`",
        );
    }

    // [✓] The forwarded socket actually answers.
    let mut connected = false;
    if ssh::master_alive(&cfg).await {
        if let Ok(sock) = config::local_docker_socket() {
            match forwarder::connect(&sock) {
                Ok(d) if d.ping().await.is_ok() => {
                    pass("Connection up — the forwarded socket responds");
                    connected = true;
                }
                Ok(_) => {
                    problems += 1;
                    fail(
                        "Forwarded socket not responding",
                        "run `runtime-orbit down` then `runtime-orbit up`",
                    );
                }
                Err(_) => {
                    problems += 1;
                    fail("Forwarded socket missing", "run `runtime-orbit up`");
                }
            }
        }
    } else {
        note(
            "Not up yet",
            "run `runtime-orbit up` to route docker to the donor",
        );
    }

    // [✓] Where docker points right now.
    let ctx = docker_ctx::current_context().await.unwrap_or_default();
    if ctx == cfg.context_name {
        pass("Docker is routed to the donor");
    } else {
        note(
            &format!("Docker is on `{ctx}` (this machine)"),
            "run `runtime-orbit up` to route it to the donor",
        );
    }

    // [!] A local RAM budget you can never reach is a silently dead setting.
    if let Some(t) = cfg.limits.local_ram_threshold_gb {
        let total = metrics::local_vitals_blocking_total();
        if total > 0.0 && t > total {
            note(
                &format!(
                    "Local RAM budget ({t:.1} GB) is above this machine's total RAM ({total:.1} GB)"
                ),
                "it can never trip, so nothing will ever route to the donor — \
                 `runtime-orbit limits set --local-ram-threshold <smaller>`",
            );
        }
    }

    // What you're borrowing, when we can see it.
    let mut borrowed = None;
    if connected {
        if let Ok(sock) = config::local_docker_socket() {
            if let Some(r) = metrics::remote_metrics(&sock).await {
                println!(
                    "\n  {} donor has {} cores · {} · {} images · {} running",
                    "◇".cyan(),
                    r.ncpu,
                    metrics::fmt_gib(r.mem_total),
                    r.images,
                    r.running,
                );
                borrowed = Some(cfg.ssh_target());
            }
        }
    }

    verdict(problems, borrowed)
}

fn verdict(problems: u32, donor: Option<String>) -> Result<()> {
    println!();
    if problems == 0 {
        let where_ = donor
            .map(|h| format!(" — docker runs on {h}"))
            .unwrap_or_default();
        println!(
            "{} {}{}",
            "•".green().bold(),
            "No issues found! runtime-orbit is ready".green().bold(),
            where_
        );
        println!("  {}", "See it live:  runtime-orbit dashboard".dimmed());
    } else {
        println!(
            "{} {}",
            "•".yellow().bold(),
            format!("{problems} issue(s) found — see the fixes above").yellow()
        );
    }
    Ok(())
}

fn pass(msg: &str) {
    println!("  {} {msg}", "[✓]".green().bold());
}
fn fail(msg: &str, fix: &str) {
    println!("  {} {}", "[✗]".red().bold(), msg.red());
    println!("      {} {fix}", "→".dimmed());
}
fn note(msg: &str, fix: &str) {
    println!("  {} {msg}", "[!]".yellow().bold());
    println!("      {} {fix}", "→".dimmed());
}
