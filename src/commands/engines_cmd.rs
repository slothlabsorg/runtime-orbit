//! `runtime-orbit engines` — which container runtimes are on each machine.
//!
//! Useful when a donor has three of them installed and you need to know which
//! socket runtime-orbit picked, or when `docker` works but the socket you expect
//! isn't there.

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::config::Config;
use crate::engines;
use crate::util;

pub async fn run() -> Result<()> {
    util::header("runtime-orbit engines");

    // This machine.
    println!("  {}", "THIS MACHINE".bold());
    let local = engines::detect_local().await;
    if local.is_empty() {
        println!(
            "  {} {}",
            "none running".yellow(),
            "(a runtime here is optional — you're borrowing one)".dimmed()
        );
    } else {
        for (i, e) in local.iter().enumerate() {
            let mark = if i == 0 {
                "→".green().to_string()
            } else {
                " ".into()
            };
            println!("  {mark} {:<30} {}", e.name.bold(), e.socket.dimmed());
        }
    }
    let tools = engines::tools_local().await;
    if !tools.is_empty() {
        util::info("CLIs found", &tools.join(", "));
    }

    // The donor, if we're linked to one.
    let Ok(cfg) = Config::load() else {
        println!(
            "\n  {}",
            "not linked to a donor yet — `runtime-orbit setup --ip <donor-ip>`".dimmed()
        );
        return Ok(());
    };

    println!("\n  {} {}", "DONOR".bold(), cfg.ssh_target().dimmed());
    let remote = engines::detect_remote(&cfg).await;
    if remote.is_empty() {
        println!(
            "  {} {}",
            "none reachable".yellow(),
            "(SSH down, or no runtime running there)".dimmed()
        );
    } else {
        for e in &remote {
            let chosen = e.socket == cfg.remote_socket;
            let mark = if chosen {
                "→".green().to_string()
            } else {
                " ".into()
            };
            let suffix = if chosen {
                " (in use)".green().to_string()
            } else {
                String::new()
            };
            println!(
                "  {mark} {:<30} {}{}",
                e.name.bold(),
                e.socket.dimmed(),
                suffix
            );
        }
    }
    let rtools = engines::tools_remote(&cfg).await;
    if !rtools.is_empty() {
        util::info("CLIs found", &rtools.join(", "));
    }

    println!(
        "\n  {}",
        "Any runtime that speaks the Docker Engine API over a unix socket works: \
         Docker Desktop, OrbStack, Rancher Desktop, colima, Lima, Podman, containerd."
            .dimmed()
    );
    Ok(())
}
