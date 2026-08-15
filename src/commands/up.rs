//! `runtime-orbit up` — route docker to the donor, open the SSH master + socket forward,
//! and start the port reconciler. Detaches by default so you can keep working.

use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use std::process::Stdio;

use crate::config::{self, Config};
use crate::docker_ctx;
use crate::forwarder;
use crate::ssh;
use crate::util;

pub async fn run(foreground: bool) -> Result<()> {
    let mut cfg = config::require_linked()?;
    util::header("runtime-orbit up");

    // Already running?
    if ssh::master_alive(&cfg).await && config::pid_file()?.exists() {
        util::warn("already up. Use `runtime-orbit status`, `runtime-orbit dashboard` or `runtime-orbit down`.");
        return Ok(());
    }

    // Remember the current context so `runtime-orbit down` can restore it.
    let current = docker_ctx::current_context().await.unwrap_or_default();
    if current != cfg.context_name {
        cfg.previous_context = Some(current);
    }
    cfg.save()?;

    // Route docker to the donor.
    util::step(&format!(
        "Switching docker to context `{}`…",
        cfg.context_name
    ));
    docker_ctx::use_context(&cfg.context_name).await?;

    // Open the master connection + forward the remote docker socket locally.
    let local_sock = config::local_docker_socket()?;
    util::step("Opening SSH master + forwarding docker socket…");
    ssh::start_master(&cfg, &local_sock, &cfg.remote_socket).await?;
    util::ok(&format!("connected to {}", cfg.ssh_target()));

    if foreground {
        util::ok("forwarding ports (foreground — Ctrl-C to stop)");
        write_pid(std::process::id())?;
        // CLI dashboard: specs of both machines + what we're offloading, then
        // live logs stream below it.
        crate::metrics::print_dashboard(&cfg).await;
        println!("{}", "live logs:".dimmed());
        let res = tokio::select! {
            r = forwarder::run(&cfg, &local_sock) => r,
            _ = tokio::signal::ctrl_c() => Ok(()),
        };
        cleanup(&cfg).await;
        return res;
    }

    // Detached: spawn the hidden `forward` worker in its own process group.
    let exe = std::env::current_exe().context("locating orbit binary")?;
    let log = config::log_path()?;
    let logfile = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;

    let child = detached_worker(&exe, logfile)?;
    write_pid(child)?;

    util::ok("forwarder started in the background");
    util::info("logs", &log.to_string_lossy());
    println!();
    util::ok("Docker now runs on the donor. `docker run -p 8080:80 …` → curl localhost:8080");
    println!("  Stop anytime with `runtime-orbit down`.");
    util::funding_note();
    Ok(())
}

#[cfg(unix)]
fn detached_worker(exe: &std::path::Path, log: std::fs::File) -> Result<u32> {
    use std::os::unix::process::CommandExt;
    let log2 = log.try_clone()?;
    let child = std::process::Command::new(exe)
        .arg("forward")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .process_group(0) // detach from the controlling terminal
        .spawn()
        .context("spawning detached forwarder")?;
    Ok(child.id())
}

#[cfg(not(unix))]
fn detached_worker(exe: &std::path::Path, log: std::fs::File) -> Result<u32> {
    let log2 = log.try_clone()?;
    let child = std::process::Command::new(exe)
        .arg("forward")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .spawn()
        .context("spawning detached forwarder")?;
    Ok(child.id())
}

fn write_pid(pid: u32) -> Result<()> {
    std::fs::write(config::pid_file()?, pid.to_string())?;
    Ok(())
}

/// Foreground teardown when the reconciler exits.
async fn cleanup(cfg: &Config) {
    let _ = std::fs::remove_file(config::pid_file().unwrap_or_default());
    let _ = ssh::stop_master(cfg).await;
    if let Some(prev) = &cfg.previous_context {
        let _ = docker_ctx::use_context(prev).await;
    }
}
