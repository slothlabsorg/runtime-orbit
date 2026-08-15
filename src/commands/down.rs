//! `runtime-orbit down` — stop the forwarder, close the SSH master, restore the context.

use anyhow::Result;

use crate::config::{self, Config};
use crate::docker_ctx;
use crate::ssh;
use crate::util;

pub async fn run() -> Result<()> {
    let mut cfg = config::require_linked()?;
    util::header("runtime-orbit down");

    // 1. Stop the detached forwarder.
    if let Ok(pid_path) = config::pid_file() {
        if let Ok(pid) = std::fs::read_to_string(&pid_path) {
            let pid = pid.trim();
            if !pid.is_empty() {
                let _ = util::capture("kill", &[pid]).await;
                util::ok("stopped the forwarder");
            }
            let _ = std::fs::remove_file(&pid_path);
        }
    }

    // 2. Close the SSH master (drops every forward).
    ssh::stop_master(&cfg).await?;
    let _ = std::fs::remove_file(config::local_docker_socket()?);
    util::ok("closed SSH master and forwards");

    // 3. Restore the previous docker context.
    restore_context(&mut cfg).await;

    cfg.previous_context = None;
    cfg.save()?;
    util::ok("done — docker is back on your local engine");
    util::funding_note();
    Ok(())
}

async fn restore_context(cfg: &mut Config) {
    // Never restore into a context we manage: its socket is the tunnel we just
    // tore down. Upgraders can have `orbit` saved here from a previous version.
    let target = cfg
        .previous_context
        .clone()
        .filter(|c| c != &cfg.context_name && !config::LEGACY_CONTEXTS.contains(&c.as_str()))
        .unwrap_or_else(|| "default".into());
    match docker_ctx::use_context(&target).await {
        Ok(()) => util::info("context", &target),
        Err(_) => {
            // The saved context vanished; fall back to default.
            let _ = docker_ctx::use_context("default").await;
            util::warn(&format!("context `{target}` gone — switched to `default`"));
        }
    }
}
