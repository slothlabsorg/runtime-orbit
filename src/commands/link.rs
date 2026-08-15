//! `runtime-orbit link <user@host>` — the low-level half of `setup`.
//!
//! Ensures the SSH key, detects the donor's runtime socket and adapter, then
//! creates the `runtime-orbit` docker context. `setup` calls this after it has
//! already sorted out authorization; running it directly is for scripts and for
//! re-pointing at a donor whose socket moved.

use anyhow::{Context, Result};
use owo_colors::OwoColorize;

use crate::config::{self, Config};
use crate::docker_ctx;
use crate::engines;
use crate::host::HostKind;
use crate::metrics;
use crate::ssh;
use crate::util;

pub async fn run(target: &str, port: u16, socket_override: Option<String>) -> Result<()> {
    util::header("runtime-orbit link");

    let (user, donor) = parse_target(target).await?;

    // Provisional config so ssh helpers know where to connect. Preserve any
    // budgets and routing rules the user already set.
    let existing = Config::load().ok();
    let mut cfg = Config::for_donor(&user, &donor, port);
    if let Some(prev) = &existing {
        cfg.limits = prev.limits.clone();
        cfg.routes = prev.routes.clone();
    }
    if let Some(s) = &socket_override {
        cfg.remote_socket = s.clone();
    }

    // 1. SSH key.
    util::step("Ensuring the runtime-orbit SSH key…");
    let pubkey = ssh::ensure_key().await?;
    util::ok("key ready");

    // 2. Connectivity — authorize in-app if the probe fails.
    util::step(&format!("Connecting to {}…", cfg.ssh_target()));
    if ssh::test_connection(&cfg).await.is_err() {
        util::warn("not authorized yet — installing this machine's key on the donor.");
        let label = metrics::local_vitals().await.hostname;
        ssh::install_key_interactive(&cfg, &pubkey, &label)
            .await
            .context("could not authorize this machine on the donor")?;
        ssh::test_connection(&cfg).await.context(
            "the key was installed but SSH still refuses it — run `runtime-orbit doctor`",
        )?;
    }
    util::ok("SSH connection works");

    // 3. Detect the donor's adapter + runtime socket.
    let os = ssh::remote_exec(&cfg, "uname -s 2>/dev/null || echo unknown")
        .await
        .unwrap_or_default();
    cfg.adapter = if os.contains("Darwin") || os.contains("Linux") {
        HostKind::Unix
    } else {
        HostKind::WindowsWsl
    };

    if socket_override.is_none() {
        let found = engines::detect_remote(&cfg).await;
        match engines::preferred(&found) {
            Some(e) => {
                cfg.remote_socket = e.socket.clone();
                util::ok(&format!(
                    "donor is {} — runtime {} at {}",
                    os.trim(),
                    e.name,
                    e.socket
                ));
                if found.len() > 1 {
                    let others: Vec<String> =
                        found.iter().skip(1).map(|e| e.name.clone()).collect();
                    util::info("also available", &others.join(", "));
                }
            }
            None => {
                util::warn(&format!(
                    "no runtime socket found on the donor. Is Docker/OrbStack/Rancher/Podman \
                     running there? Run `runtime-orbit donor doctor` on {} to check.",
                    cfg.donor_addr
                ));
                util::info("assuming", &cfg.remote_socket);
            }
        }
    } else {
        util::ok(&format!(
            "donor is {} — using the socket you specified: {}",
            os.trim(),
            cfg.remote_socket
        ));
    }

    // 4. Create the docker context pointing at the (to-be) forwarded socket.
    util::step(&format!("Creating docker context `{}`…", cfg.context_name));
    docker_ctx::create_or_update(
        &cfg.context_name,
        &cfg.docker_endpoint(),
        &format!("runtime-orbit → {}", cfg.ssh_target()),
    )
    .await?;
    util::ok("context created (activated on `runtime-orbit up`)");

    cfg.save()?;
    util::ok(&format!(
        "saved config to {}",
        config::config_path()?.display()
    ));

    util::header("Next");
    println!(
        "  Run {} to route docker to the donor and forward ports.",
        "runtime-orbit up".bold().green()
    );
    Ok(())
}

/// Split `user@host` / `host`. Without a user, use the current local username.
async fn parse_target(target: &str) -> Result<(String, String)> {
    if let Some((u, h)) = target.split_once('@') {
        Ok((u.to_string(), h.to_string()))
    } else {
        let user = util::run("whoami", &[])
            .await
            .unwrap_or_else(|_| "root".into());
        Ok((user, target.to_string()))
    }
}
