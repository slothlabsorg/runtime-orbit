//! The port reconciler — the piece that makes remote Docker feel local.
//!
//! It talks to the remote daemon through the forwarded unix socket (set up by
//! `ssh::start_master`), watches container events, and keeps the set of SSH TCP
//! forwards in sync with the set of published container ports. Net effect:
//! `docker run -p 8080:80` on the donor ⇒ `curl localhost:8080` works on the borrower.

use anyhow::{Context, Result};
use bollard::container::ListContainersOptions;
use bollard::system::EventsOptions;
use bollard::Docker;
use futures_util::StreamExt;
use std::collections::HashSet;
use std::path::Path;

use crate::config::Config;
use crate::ssh;

/// Connect bollard to the locally-forwarded docker socket.
pub fn connect(socket: &Path) -> Result<Docker> {
    Docker::connect_with_unix(&socket.to_string_lossy(), 120, bollard::API_DEFAULT_VERSION)
        .with_context(|| {
            format!(
                "connecting to forwarded docker socket at {}",
                socket.display()
            )
        })
}

/// The set of published host ports across all running containers.
async fn desired_ports(docker: &Docker) -> Result<HashSet<u16>> {
    let opts = ListContainersOptions::<String> {
        all: false,
        ..Default::default()
    };
    let containers = docker.list_containers(Some(opts)).await?;
    let mut ports = HashSet::new();
    for c in containers {
        for p in c.ports.unwrap_or_default() {
            if let Some(public) = p.public_port {
                ports.insert(public);
            }
        }
    }
    Ok(ports)
}

/// Bring active SSH forwards in line with `desired`, mutating `current`.
async fn reconcile(cfg: &Config, current: &mut HashSet<u16>, desired: HashSet<u16>) {
    for &port in desired
        .difference(current)
        .cloned()
        .collect::<Vec<_>>()
        .iter()
    {
        match ssh::add_forward(cfg, port).await {
            Ok(()) => {
                current.insert(port);
                tracing::info!(port, "forwarding localhost:{port} -> donor:{port}");
                println!("  + forwarding localhost:{port} → donor:{port}");
            }
            Err(e) => {
                tracing::warn!(port, error = %e, "could not forward");
                eprintln!("  ! could not forward {port}: {e}");
            }
        }
    }
    for &port in current
        .difference(&desired)
        .cloned()
        .collect::<Vec<_>>()
        .iter()
    {
        let _ = ssh::cancel_forward(cfg, port).await;
        current.remove(&port);
        tracing::info!(port, "stopped forwarding {port}");
        println!("  - stopped forwarding {port}");
    }
}

/// Run the reconciler until the process is asked to stop. Performs an initial
/// reconcile, then reacts to every container event. The docker event stream can
/// drop (idle timeouts, the forwarded socket blipping); rather than tearing down
/// every forward we log it and re-subscribe with a short backoff, keeping the
/// existing forwards in place so the connection stays transparent.
pub async fn run(cfg: &Config, socket: &Path) -> Result<()> {
    let docker = connect(socket)?;
    docker
        .ping()
        .await
        .context("forwarded docker socket is not responding")?;
    tracing::info!(socket = %socket.display(), "forwarder started");

    let mut current: HashSet<u16> = HashSet::new();
    let mut backoff_ms = 500u64;

    loop {
        // (Re)sync from the source of truth on every (re)connect.
        let desired = desired_ports(&docker)
            .await
            .unwrap_or_else(|_| current.clone());
        reconcile(cfg, &mut current, desired).await;

        let mut events = docker.events(Some(EventsOptions::<String>::default()));
        tracing::debug!("subscribed to docker event stream");

        let mut clean = true;
        while let Some(event) = events.next().await {
            match event {
                Ok(ev) => {
                    backoff_ms = 500; // healthy stream resets backoff
                    tracing::trace!(action = ?ev.action, "docker event");
                    let desired = desired_ports(&docker)
                        .await
                        .unwrap_or_else(|_| current.clone());
                    reconcile(cfg, &mut current, desired).await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "docker event stream dropped — reconnecting");
                    clean = false;
                    break;
                }
            }
        }

        // If the stream ended, wait and try again (keeping forwards intact). The
        // process is stopped externally by `runtime-orbit down` (SIGTERM), not from here.
        if clean {
            tracing::debug!("event stream ended; reconnecting");
        }
        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
        backoff_ms = (backoff_ms * 2).min(10_000);

        // If the socket is gone entirely, keep looping; ping just logs.
        if let Err(e) = docker.ping().await {
            tracing::warn!(error = %e, "forwarded socket not responding; will retry");
        }
    }
}

/// Snapshot of the currently-published ports — used by `runtime-orbit status`/`ports`.
pub async fn list_published(socket: &Path) -> Result<Vec<u16>> {
    let docker = connect(socket)?;
    let mut v: Vec<u16> = desired_ports(&docker).await?.into_iter().collect();
    v.sort_unstable();
    Ok(v)
}
