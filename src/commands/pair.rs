//! `runtime-orbit pair` — the borrower half of passwordless pairing.
//!
//! Opens a one-shot listener holding this machine's public key behind a 6-digit
//! code, then waits for the donor to pull it. Used automatically by
//! `runtime-orbit setup` when password login is disabled on the donor, and
//! available on its own for re-pairing after a key rotation.

use anyhow::Result;
use owo_colors::OwoColorize;
use std::time::Duration;

use crate::metrics;
use crate::pairing;
use crate::ssh;
use crate::util;

/// Stand-alone `runtime-orbit pair`.
pub async fn run(port: u16, minutes: u64) -> Result<()> {
    let pubkey = ssh::ensure_key().await?;
    let vitals = metrics::local_vitals().await;
    let ip = vitals
        .ip
        .clone()
        .unwrap_or_else(|| "<this-machine-ip>".into());
    serve(&pubkey, &vitals.hostname, &ip, port, minutes).await
}

/// Serve the key until a donor pairs, or the window closes.
pub async fn serve(
    pubkey: &str,
    hostname: &str,
    my_ip: &str,
    port: u16,
    minutes: u64,
) -> Result<()> {
    let code = pairing::code_for(pubkey, pairing::session_salt());

    util::header("Pairing");
    println!(
        "  This machine is waiting to be authorized. On the {}, run:\n",
        "donor".bold()
    );
    println!(
        "      {}\n",
        format!("runtime-orbit donor pair {my_ip}").bold().green()
    );
    println!("  Pairing code   {}", code.bold().cyan());
    println!(
        "  {}\n",
        format!("listening on port {port} for {minutes} minute(s) — the code works once").dimmed()
    );
    util::step("Waiting for the donor…");

    let peer = pairing::serve_once(
        port,
        &code,
        hostname,
        pubkey,
        Duration::from_secs(minutes * 60),
        |peer, accepted| {
            if accepted {
                util::ok(&format!("paired with {peer}"));
            } else {
                util::warn(&format!("refused an attempt from {peer} (wrong code)"));
            }
        },
    )
    .await?;

    util::info("authorized by", &peer);
    Ok(())
}
