//! `runtime-orbit docker …` — run one docker command through the routing table.
//!
//! This is the opt-in half of routing. `runtime-orbit up` delegates *everything*
//! by switching the docker context; this instead picks a context per invocation,
//! so `postgres` can stay on your own SSD while a 40-layer build goes to the
//! donor. Nothing is wrapped or intercepted: we just choose `--context` for you
//! and exec the real `docker`.

use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use std::process::Stdio;

use crate::config;
use crate::routing::{self, Target};
use crate::util;

pub async fn run(args: Vec<String>) -> Result<()> {
    if args.is_empty() {
        anyhow::bail!(
            "nothing to run. Example:\n  \
             runtime-orbit docker run -d -p 8080:80 nginx\n  \
             runtime-orbit docker build -t acme/api:dev ."
        );
    }

    let cfg = config::require_linked()?;
    let workload = routing::workload_from_args(&args);
    let snap = routing::Snapshot::gather().await;
    let decision = routing::decide(&cfg, &workload, &snap);

    // `local` means this machine's own engine — whatever it was before we
    // borrowed, falling back to the stock context.
    let context = match decision.target {
        Target::Donor => cfg.context_name.clone(),
        Target::Local => cfg
            .previous_context
            .clone()
            .filter(|c| c != &cfg.context_name)
            .unwrap_or_else(|| "default".to_string()),
    };

    eprintln!(
        "{} {} → {} {}",
        "▸".cyan().bold(),
        workload.bold(),
        decision.target.as_str().bold(),
        format!("({}) · context {context}", decision.reason).dimmed()
    );

    if decision.target == Target::Donor {
        // Routing to a donor we haven't connected to would fail deep inside
        // docker with a confusing socket error. Say it plainly instead.
        let sock = config::local_docker_socket()?;
        if !sock.exists() {
            anyhow::bail!(
                "this workload routes to the donor, but the connection isn't up — \
                 run `runtime-orbit up` first"
            );
        }
    }

    let mut argv: Vec<String> = vec!["--context".into(), context];
    argv.extend(args);

    let status = tokio::process::Command::new("docker")
        .args(&argv)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("could not run docker — is it installed and on PATH?")?;

    let _ = util::FUNDING_URL;
    if !status.success() {
        // Pass docker's own exit code through; wrappers that swallow it break
        // shell `&&` chains and CI.
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
