//! `runtime-orbit setup` — run this on the machine that is short on RAM.
//!
//! Discovers (or takes) the donor's address, sets up the SSH key and authorizes
//! it, links, brings the borrow up, and proves it works with an end-to-end
//! self-test. Target: borrowing a runtime in about two minutes.

use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use std::process::Stdio;

use crate::commands::{link, up};
use crate::config::Config;
use crate::metrics;
use crate::net_scan;
use crate::ssh;
use crate::util;

#[allow(clippy::too_many_arguments)]
pub async fn run(
    address: Option<String>,
    user_arg: Option<String>,
    port: u16,
    max_ram: Option<f64>,
    local_ram_threshold: Option<f64>,
    yes: bool,
    no_test: bool,
) -> Result<()> {
    util::header("runtime-orbit setup");
    println!(
        "  This machine will borrow another machine's container runtime.\n  \
         Takes about two minutes.\n"
    );

    // Show what we're working with — it frames why you're doing this.
    let local = metrics::local_vitals().await;
    if local.mem_total > 0 {
        println!(
            "  {} {} · {} · {} cores · {} RAM ({} free)\n",
            "this machine:".dimmed(),
            local.hostname,
            local.os,
            local.cores,
            metrics::fmt_gib_u(local.mem_total),
            metrics::fmt_gib_u(local.mem_avail),
        );
    }

    // 1. Donor address ------------------------------------------------------
    let donor_addr = match address {
        Some(h) => h,
        None => pick_donor(yes)?,
    };

    // 2. SSH user ------------------------------------------------------------
    let default_user = util::run("whoami", &[])
        .await
        .unwrap_or_else(|_| "root".into());
    let user = match user_arg {
        Some(u) => u,
        None if yes => default_user.clone(),
        None => inquire::Text::new("SSH username on the donor:")
            .with_default(&default_user)
            .with_help_message("Your login on the beefy machine")
            .prompt()
            .context("cancelled")?,
    };

    let target = format!("{user}@{donor_addr}");
    let cfg = Config::for_donor(&user, &donor_addr, port);

    // 3. SSH key + authorization --------------------------------------------
    util::step("Preparing the runtime-orbit SSH key…");
    let pubkey = ssh::ensure_key().await?;
    util::ok("key ready");

    util::step(&format!("Checking SSH access to {target}…"));
    if ssh::test_connection(&cfg).await.is_err() {
        authorize_key(&cfg, &pubkey, yes).await?;
    } else {
        util::ok("the runtime-orbit key is already authorized");
    }

    // 4. Link (detect socket + create context) ------------------------------
    println!();
    link::run(&target, port, None).await?;

    // 5. Budgets, if the user asked for them --------------------------------
    if max_ram.is_some() || local_ram_threshold.is_some() {
        let mut cfg = Config::load()?;
        if let Some(gb) = max_ram {
            cfg.limits.max_borrow_ram_gb = Some(gb);
        }
        if let Some(gb) = local_ram_threshold {
            cfg.limits.local_ram_threshold_gb = Some(gb);
        }
        cfg.save()?;
        util::ok("saved your RAM budgets — see `runtime-orbit limits show`");
    }

    // 6. Up ------------------------------------------------------------------
    println!();
    up::run(false).await?;

    // 7. Self-test -----------------------------------------------------------
    if !no_test {
        println!();
        if let Err(e) = self_test().await {
            util::warn(&format!(
                "self-test could not complete ({e:#}). The borrow is up regardless — try:\n\
                 docker run -d -p 8080:80 nginx && curl localhost:8080"
            ));
        }
    }

    util::header("You're set");
    println!("  Docker now runs on {}. Use it normally:\n", target.bold());
    println!(
        "      {}  docker build / run / compose — all on the donor",
        "›".dimmed()
    );
    println!(
        "      {}  published ports (-p) appear on this machine's localhost",
        "›".dimmed()
    );
    println!(
        "      {}  {}   live view of both machines",
        "›".dimmed(),
        "runtime-orbit dashboard".cyan()
    );
    println!(
        "      {}  {}   keep it running across logins",
        "›".dimmed(),
        "runtime-orbit service install".cyan()
    );
    println!(
        "      {}  {}   stop and go back to local docker",
        "›".dimmed(),
        "runtime-orbit down".cyan()
    );
    util::funding_note();
    Ok(())
}

/// Discover donors on the LAN and let the user pick, or type one in.
fn pick_donor(yes: bool) -> Result<String> {
    if yes {
        anyhow::bail!("--yes needs an address: `runtime-orbit setup --ip <donor-ip> --yes`");
    }
    util::step("Scanning your LAN for machines with SSH open…");
    let candidates = futures_lite_block(net_scan::scan(22));

    let manual = "✎ Enter an address manually".to_string();
    let rescan = "↻ Rescan".to_string();

    let mut options: Vec<String> = candidates.iter().map(|c| c.ip.to_string()).collect();
    if options.is_empty() {
        util::warn("no SSH hosts found automatically — enter the address manually.");
        return prompt_manual_donor();
    }
    options.push(manual.clone());
    options.push(rescan.clone());

    let choice = inquire::Select::new("Which machine should lend its runtime?", options)
        .with_help_message("Pick the beefy machine on your network")
        .prompt()
        .context("cancelled")?;

    if choice == manual {
        prompt_manual_donor()
    } else if choice == rescan {
        pick_donor(false)
    } else {
        Ok(choice)
    }
}

fn prompt_manual_donor() -> Result<String> {
    inquire::Text::new("Donor address (IP or hostname):")
        .with_help_message("e.g. 192.168.1.20 or beefy.local")
        .prompt()
        .context("cancelled")
        .map(|s| s.trim().to_string())
}

/// Authorize our public key on the donor, entirely from inside this command.
///
/// Two in-app routes, both self-contained — we never ask anyone to hand-edit
/// `authorized_keys` or run `ssh-copy-id`:
///
/// 1. **Password once** — we open an SSH session with the terminal inherited, so
///    OpenSSH's password prompt appears right here, and we do the
///    `authorized_keys` edit ourselves on the other side.
/// 2. **Pair from the donor** — for machines with password login disabled. This
///    command opens a one-shot listener with a 6-digit code; the donor runs
///    `runtime-orbit donor pair <ip>` and pulls the key over the LAN.
async fn authorize_key(cfg: &Config, pubkey: &str, yes: bool) -> Result<()> {
    util::warn("this machine isn't authorized on the donor yet — setting that up now.");

    if yes {
        // Non-interactive: the password route needs a human, so go straight to
        // the one route that can't prompt us.
        return pair_route(cfg, pubkey).await;
    }

    const PASSWORD: &str = "Use the donor's login password (once, right here)";
    const PAIR: &str = "Pair from the donor instead (no password needed)";

    let mut choice =
        inquire::Select::new("How should I authorize this machine?", vec![PASSWORD, PAIR])
            .with_help_message("Both happen inside runtime-orbit — nothing to copy or paste")
            .prompt()
            .context("cancelled")?;

    loop {
        if choice == PASSWORD {
            util::step(&format!(
                "Connecting to {} — type that machine's login password when asked…",
                cfg.ssh_target()
            ));
            let label = metrics::local_vitals().await.hostname;
            match ssh::install_key_interactive(cfg, pubkey, &label).await {
                Ok(()) => {
                    if ssh::test_connection(cfg).await.is_ok() {
                        util::ok("authorized — SSH works without a password from now on");
                        return Ok(());
                    }
                    util::warn(
                        "the key was written but key-based login still fails — trying pairing.",
                    );
                }
                Err(e) => {
                    util::warn(&format!("that didn't work: {e:#}"));
                }
            }
            println!();
            choice = PAIR;
            continue;
        }

        // Pairing route.
        pair_route(cfg, pubkey).await?;
        return Ok(());
    }
}

/// Open the pairing listener and wait for the donor to pull the key.
async fn pair_route(cfg: &Config, pubkey: &str) -> Result<()> {
    let vitals = metrics::local_vitals().await;
    let my_ip = vitals
        .ip
        .clone()
        .unwrap_or_else(|| "<this-machine-ip>".into());

    crate::commands::pair::serve(
        pubkey,
        &vitals.hostname,
        &my_ip,
        crate::pairing::DEFAULT_PORT,
        10,
    )
    .await?;

    // The donor has the key now; confirm it actually took.
    for _ in 0..10 {
        if ssh::test_connection(cfg).await.is_ok() {
            util::ok("authorized — SSH works without a password from now on");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    }
    anyhow::bail!(
        "the donor picked up the key, but SSH still refuses it.\n\
         Run `runtime-orbit donor doctor` on {} to see why (SSH off? wrong user?).",
        cfg.donor_addr
    )
}

/// Prove transparency: run a tiny container on the donor and curl it via localhost.
async fn self_test() -> Result<()> {
    util::step("Running an end-to-end self-test (nginx on the donor → curl localhost)…");
    let port = free_local_port().unwrap_or(8080);
    let name = "runtime-orbit-selftest";
    docker_quiet(&["rm", "-f", name]).await;

    println!("  pulling a small test image (nginx:alpine)…");
    docker(&["pull", "nginx:alpine"]).await?;

    docker(&[
        "run",
        "-d",
        "--rm",
        "-p",
        &format!("{port}:80"),
        "--name",
        name,
        "nginx:alpine",
    ])
    .await
    .context("could not start the test container")?;

    let mut ok = false;
    for _ in 0..15 {
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        if curl_ok(port).await {
            ok = true;
            break;
        }
    }
    docker_quiet(&["rm", "-f", name]).await;

    if ok {
        util::ok(&format!(
            "self-test passed — a container on the donor answered on localhost:{port}"
        ));
        Ok(())
    } else {
        anyhow::bail!("the test container didn't answer on localhost:{port}")
    }
}

async fn docker(args: &[&str]) -> Result<()> {
    let status = tokio::process::Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("failed to run docker — is it installed?")?;
    if !status.success() {
        anyhow::bail!("`docker {}` failed", args.join(" "));
    }
    Ok(())
}

/// Fire-and-forget docker call with all output suppressed (for cleanup).
async fn docker_quiet(args: &[&str]) {
    let _ = tokio::process::Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

async fn curl_ok(port: u16) -> bool {
    tokio::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "--max-time",
            "5",
            &format!("http://localhost:{port}"),
        ])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

fn free_local_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
}

/// Run a future to completion on the current runtime from a sync context.
fn futures_lite_block<F: std::future::Future>(fut: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}
