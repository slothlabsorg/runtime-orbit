//! `runtime-orbit status` — link, connection, forwarded ports, remote resource usage.

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::config;
use crate::docker_ctx;
use crate::forwarder;
use crate::metrics;
use crate::ssh;
use crate::util;

pub async fn run() -> Result<()> {
    let cfg = config::require_linked()?;
    util::header("runtime-orbit status");

    // Link
    util::info("donor", &cfg.ssh_target());
    util::info("adapter", &cfg.adapter.to_string());
    util::info("endpoint", &cfg.docker_endpoint());
    util::info("remote sock", &cfg.remote_socket);

    // Docker context
    let ctx = docker_ctx::current_context()
        .await
        .unwrap_or_else(|_| "?".into());
    let ctx_line = if ctx == cfg.context_name {
        format!("{ctx} {}", "(active — docker → donor)".green())
    } else {
        format!("{ctx} {}", "(local engine)".dimmed())
    };
    util::info("docker ctx", &ctx_line);

    // Connection
    let master = ssh::master_alive(&cfg).await;
    util::info("ssh master", &state(master));
    util::info("forwarder", &state(daemon_alive()));

    if !master {
        println!();
        util::warn("not connected — run `runtime-orbit up`.");
        return Ok(());
    }

    // Remote resources + ports (through the forwarded socket).
    print_remote(&cfg).await;
    Ok(())
}

fn state(up: bool) -> String {
    if up {
        format!("{}", "running".green())
    } else {
        format!("{}", "stopped".red())
    }
}

fn daemon_alive() -> bool {
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

async fn print_remote(cfg: &crate::config::Config) {
    let socket = match config::local_docker_socket() {
        Ok(s) => s,
        Err(_) => return,
    };

    if let Some(r) = metrics::remote_metrics(&socket).await {
        util::header("Donor runtime");
        util::info("version", &r.version);
        util::info("CPUs", &r.ncpu.to_string());
        util::info("memory", &metrics::fmt_gib(r.mem_total));
        util::info(
            "containers",
            &format!("{} running / {} total", r.running, r.containers),
        );
        util::info("images", &r.images.to_string());

        // The part that makes people fall in love: what your laptop ISN'T doing.
        util::header("Carried by the donor");
        let load = metrics::remote_load(cfg).await;
        util::info(
            "processor",
            &format!(
                "{} cores{}",
                r.ncpu,
                load.map(|l| format!(" · load {l}")).unwrap_or_default()
            ),
        );
        if r.offloaded_mem > 0 {
            util::info(
                "RAM in use by containers",
                &format!(
                    "{} (of {} on the donor)",
                    metrics::fmt_gib_u(r.offloaded_mem),
                    metrics::fmt_gib(r.mem_total)
                ),
            );
        }
        println!(
            "  {} {} running on {} — none of it on this laptop {}",
            "›".dimmed(),
            format!("{} containers", r.running).magenta(),
            cfg.donor_addr.white(),
            "♥".magenta(),
        );
    }

    util::header("Forwarded ports");
    match forwarder::list_published(&socket).await {
        Ok(ports) if !ports.is_empty() => {
            for p in ports {
                println!("  localhost:{p} {} donor:{p}", "→".dimmed());
            }
        }
        Ok(_) => println!(
            "  {}",
            "none — start a container with -p to see it here".dimmed()
        ),
        Err(e) => util::warn(&format!("could not read ports: {e}")),
    }
}
