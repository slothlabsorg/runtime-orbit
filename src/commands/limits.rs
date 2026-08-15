//! `runtime-orbit limits` — the RAM/CPU budgets that drive routing.

use anyhow::{Context, Result};
use owo_colors::OwoColorize;

use crate::config::{self, Config};
use crate::metrics;
use crate::routing;
use crate::util;

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

pub async fn show() -> Result<()> {
    let cfg = config::require_linked()?;
    util::header("runtime-orbit limits");

    let l = &cfg.limits;
    let snap = routing::Snapshot::gather().await;
    let local_used = snap.local.mem_used() as f64 / GB;

    // Borrow ceiling.
    match l.max_borrow_ram_gb {
        Some(cap) => {
            let used = snap
                .remote
                .as_ref()
                .map(|r| r.offloaded_mem as f64 / GB)
                .unwrap_or(0.0);
            util::info(
                "max borrowed RAM",
                &format!("{}  {used:.1} / {cap:.1} GB", metrics::bar(used / cap, 18)),
            );
        }
        None => util::info("max borrowed RAM", &"unlimited".dimmed().to_string()),
    }

    // Local RAM budget.
    match l.local_ram_threshold_gb {
        Some(t) => util::info(
            "local RAM budget",
            &format!(
                "{}  {local_used:.1} / {t:.1} GB {}",
                metrics::bar(local_used / t, 18),
                if local_used >= t {
                    "· tripped, new work goes to the donor".green().to_string()
                } else {
                    "· under budget, new work stays here".dimmed().to_string()
                }
            ),
        ),
        None => util::info(
            "local RAM budget",
            &"not set — everything is delegated".dimmed().to_string(),
        ),
    }

    // Local load budget.
    match (l.local_load_threshold, snap.local.load1) {
        (Some(t), Some(load)) => util::info(
            "local load budget",
            &format!("{}  {load:.2} / {t:.2}", metrics::bar(load / t, 18)),
        ),
        (Some(t), None) => util::info("local load budget", &format!("{t:.2} (load unreadable)")),
        (None, _) => util::info("local load budget", &"not set".dimmed().to_string()),
    }

    util::info("prefer", &l.prefer);
    util::info(
        "routing rules",
        &format!("{} — `runtime-orbit route list`", cfg.routes.len()),
    );

    // The whole point of the budgets: where does the next container land?
    util::header("Right now");
    let d = routing::decide(&cfg, "a-new-container", &snap);
    println!(
        "  next container → {} {}",
        d.target.as_str().bold().cyan(),
        format!("({})", d.reason).dimmed()
    );
    println!(
        "\n  {}",
        "Try a specific one with: runtime-orbit route explain postgres:16".dimmed()
    );
    Ok(())
}

pub async fn set(
    max_ram: Option<String>,
    local_ram_threshold: Option<String>,
    local_load_threshold: Option<String>,
    prefer: Option<String>,
) -> Result<()> {
    let mut cfg = config::require_linked()?;

    if max_ram.is_none()
        && local_ram_threshold.is_none()
        && local_load_threshold.is_none()
        && prefer.is_none()
    {
        anyhow::bail!(
            "nothing to set. For example:\n  \
             runtime-orbit limits set --max-ram 32 --local-ram-threshold 5\n  \
             runtime-orbit limits set --max-ram off"
        );
    }

    if let Some(v) = max_ram {
        cfg.limits.max_borrow_ram_gb = parse_amount(&v, "--max-ram")?;
    }
    if let Some(v) = local_ram_threshold {
        cfg.limits.local_ram_threshold_gb = parse_amount(&v, "--local-ram-threshold")?;
    }
    if let Some(v) = local_load_threshold {
        cfg.limits.local_load_threshold = parse_amount(&v, "--local-load-threshold")?;
    }
    if let Some(p) = prefer {
        cfg.limits.prefer = p;
    }

    validate(&cfg)?;
    cfg.save()?;
    util::ok("budgets saved");
    println!();
    show().await
}

/// `off`/`none`/`unlimited` clear a budget; anything else must be a number.
fn parse_amount(raw: &str, flag: &str) -> Result<Option<f64>> {
    let v = raw.trim().to_ascii_lowercase();
    if matches!(v.as_str(), "off" | "none" | "unlimited" | "-") {
        return Ok(None);
    }
    // Tolerate "32gb" / "32 GB" — people type units.
    let cleaned: String = v
        .trim_end_matches("gb")
        .trim_end_matches('g')
        .trim()
        .to_string();
    let n: f64 = cleaned
        .parse()
        .with_context(|| format!("{flag} expects a number of GB, or `off` — got `{raw}`"))?;
    if n <= 0.0 {
        anyhow::bail!("{flag} must be greater than zero (use `off` to remove the limit)");
    }
    Ok(Some(n))
}

/// Catch budgets that can never fire, rather than letting them look active.
fn validate(cfg: &Config) -> Result<()> {
    if let Some(t) = cfg.limits.local_ram_threshold_gb {
        let total = crate::metrics::local_vitals_blocking_total();
        if total > 0.0 && t > total {
            util::warn(&format!(
                "the local RAM budget ({t:.1} GB) is above this machine's total RAM ({total:.1} GB) — \
                 it will never trip, so work will always stay local"
            ));
        }
    }
    if !matches!(cfg.limits.prefer.as_str(), "auto" | "local" | "donor") {
        anyhow::bail!("prefer must be one of: auto, local, donor");
    }
    Ok(())
}
