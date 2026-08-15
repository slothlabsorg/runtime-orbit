//! `runtime-orbit route` — the routing table: which workloads run where.

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::config::{self, Route};
use crate::routing::{self, Target};
use crate::util;

pub async fn list() -> Result<()> {
    let cfg = config::require_linked()?;
    util::header("routing table");

    if cfg.routes.is_empty() {
        println!(
            "  {}\n",
            "no rules — every workload follows the budgets in `runtime-orbit limits`".dimmed()
        );
        println!(
            "  {}",
            "Add one:  runtime-orbit route add 'postgres:*' --target local".dimmed()
        );
        return Ok(());
    }

    println!(
        "  {:<4} {:<28} {:<8} {}",
        "#".bold(),
        "PATTERN".bold(),
        "TARGET".bold(),
        "NOTE".bold()
    );
    for (i, r) in cfg.routes.iter().enumerate() {
        let target = match Target::parse(&r.target) {
            Some(Target::Local) => r.target.yellow().to_string(),
            Some(Target::Donor) => r.target.green().to_string(),
            None => r.target.red().to_string(),
        };
        println!(
            "  {:<4} {:<28} {:<8} {}",
            i + 1,
            r.pattern,
            target,
            r.note.clone().unwrap_or_default().dimmed()
        );
    }
    println!(
        "\n  {}",
        "First match wins. Anything unmatched falls through to `runtime-orbit limits`.".dimmed()
    );
    Ok(())
}

pub async fn add(pattern: String, target: String, note: Option<String>) -> Result<()> {
    let mut cfg = config::require_linked()?;

    if Target::parse(&target).is_none() {
        anyhow::bail!("target must be `local` or `donor`");
    }
    if pattern.trim().is_empty() {
        anyhow::bail!("pattern cannot be empty");
    }
    if cfg
        .routes
        .iter()
        .any(|r| r.pattern == pattern && r.target == target)
    {
        util::ok("that rule already exists — nothing to do");
        return Ok(());
    }

    // A rule that can never be reached is a bug in the table, not a preference.
    if let Some(shadow) = cfg
        .routes
        .iter()
        .position(|r| routing::glob_match(&r.pattern, &pattern))
    {
        util::warn(&format!(
            "rule #{} (`{}`) already matches `{}`, so this new rule will never fire — \
             remove or reorder it if that's not what you want",
            shadow + 1,
            cfg.routes[shadow].pattern,
            pattern
        ));
    }

    cfg.routes.push(Route {
        pattern: pattern.clone(),
        target: target.clone(),
        note,
    });
    cfg.save()?;
    util::ok(&format!("added: {pattern} → {target}"));
    println!();
    list().await
}

pub async fn rm(index: usize) -> Result<()> {
    let mut cfg = config::require_linked()?;
    if index == 0 || index > cfg.routes.len() {
        anyhow::bail!(
            "no rule #{index} — there are {} (see `runtime-orbit route list`)",
            cfg.routes.len()
        );
    }
    let removed = cfg.routes.remove(index - 1);
    cfg.save()?;
    util::ok(&format!(
        "removed: {} → {}",
        removed.pattern, removed.target
    ));
    println!();
    list().await
}

pub async fn explain(image: String) -> Result<()> {
    let cfg = config::require_linked()?;
    util::header("runtime-orbit route explain");

    let snap = routing::Snapshot::gather().await;
    let d = routing::decide(&cfg, &image, &snap);

    println!("  workload   {}", image.bold());
    println!(
        "  runs on    {}",
        match d.target {
            Target::Donor => format!("{} ({})", "donor".bold().green(), cfg.ssh_target()),
            Target::Local => format!("{} (this machine)", "local".bold().yellow()),
        }
    );
    println!("  because    {}", d.reason);
    if let Some(n) = d.rule {
        println!(
            "  {}",
            format!("matched rule #{n} — see `runtime-orbit route list`").dimmed()
        );
    }
    println!(
        "\n  {}",
        "Run it through the table with:  runtime-orbit docker run …".dimmed()
    );
    Ok(())
}
