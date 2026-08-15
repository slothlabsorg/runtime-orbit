//! `runtime-orbit ports` — list docker-published forwards, or add/remove manual ones.

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::config;
use crate::forwarder;
use crate::ssh;
use crate::util;

pub async fn list() -> Result<()> {
    let _cfg = config::require_linked()?;
    util::header("forwarded ports");
    let socket = config::local_docker_socket()?;
    match forwarder::list_published(&socket).await {
        Ok(ports) if !ports.is_empty() => {
            for p in ports {
                println!("  localhost:{p} {} donor:{p}", "→".dimmed());
            }
        }
        Ok(_) => println!(
            "  {}",
            "none (run a container with -p, or `runtime-orbit ports add <port>`)".dimmed()
        ),
        Err(_) => util::warn("orbit is not up — run `runtime-orbit up` first."),
    }
    Ok(())
}

pub async fn add(port: u16) -> Result<()> {
    let cfg = config::require_linked()?;
    if !ssh::master_alive(&cfg).await {
        anyhow::bail!("orbit is not up — run `runtime-orbit up` first.");
    }
    ssh::add_forward(&cfg, port).await?;
    util::ok(&format!("forwarding localhost:{port} → donor:{port}"));
    Ok(())
}

pub async fn rm(port: u16) -> Result<()> {
    let cfg = config::require_linked()?;
    ssh::cancel_forward(&cfg, port).await?;
    util::ok(&format!("stopped forwarding {port}"));
    Ok(())
}
