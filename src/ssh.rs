//! SSH transport: key management, a multiplexed master connection, and dynamic
//! port forwards driven through the master's control socket.
//!
//! v1 shells out to the system `ssh` (OpenSSH) behind these helpers — battle
//! tested and present on macOS/Linux/Windows. The surface is deliberately thin
//! so it can be swapped for a native `russh` backend later (see ROADMAP).

use anyhow::{bail, Context, Result};
use std::path::Path;

use crate::config::{self, Config};
use crate::util;

/// Common ssh options applied to every invocation.
fn base_opts(cfg: &Config) -> Vec<String> {
    let mut v = vec![
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        "ConnectTimeout=8".into(),
        "-p".into(),
        cfg.ssh_port.to_string(),
    ];
    if let Ok(key) = config::ssh_key_path() {
        if key.exists() {
            v.push("-i".into());
            v.push(key.to_string_lossy().into_owned());
        }
    }
    v
}

fn control_opts() -> Result<Vec<String>> {
    Ok(vec![
        "-S".into(),
        config::control_socket()?.to_string_lossy().into_owned(),
    ])
}

/// Install our public key on the donor over an interactive SSH session.
///
/// This is the in-app replacement for asking the user to run `ssh-copy-id`: we
/// inherit the terminal so OpenSSH's own password prompt appears inside the
/// running command, and we do the `authorized_keys` edit ourselves rather than
/// depending on `ssh-copy-id` being installed. The key also lands in
/// `~/.runtime-orbit/inbox/` on the donor so `runtime-orbit donor pending` can
/// show it later as a record of who asked.
///
/// A failed password makes `ssh` exit non-zero, which we surface as an error; the
/// caller then re-probes with key auth to confirm the install really took.
pub async fn install_key_interactive(cfg: &Config, pubkey: &str, label: &str) -> Result<()> {
    let key = pubkey.trim();
    if key.contains('\'') || key.contains('\n') {
        bail!("refusing to install a public key containing quotes or newlines");
    }
    let safe_label: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();

    // Idempotent, permission-fixing, and it records the request in the inbox.
    let script = format!(
        "set -e; \
         mkdir -p ~/.ssh; chmod 700 ~/.ssh; \
         touch ~/.ssh/authorized_keys; chmod 600 ~/.ssh/authorized_keys; \
         if ! grep -qF '{key}' ~/.ssh/authorized_keys; then printf '%s\\n' '{key}' >> ~/.ssh/authorized_keys; fi; \
         mkdir -p ~/.runtime-orbit/inbox; chmod 700 ~/.runtime-orbit/inbox 2>/dev/null || true; \
         printf '%s\\n' '{key}' > ~/.runtime-orbit/inbox/{safe_label}.pub; \
         echo RUNTIME_ORBIT_KEY_INSTALLED"
    );

    // Password auth needs a TTY, so inherit stdio and capture nothing. We verify
    // the outcome with a follow-up key-auth probe instead of parsing output.
    let args = vec![
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        "ConnectTimeout=15".into(),
        "-o".into(),
        // Don't let a half-configured agent key silently win or fail us.
        "IdentitiesOnly=no".into(),
        "-p".into(),
        cfg.ssh_port.to_string(),
        cfg.ssh_target(),
        script,
    ];
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    tracing::debug!(target = %cfg.ssh_target(), "installing public key over interactive ssh");

    let status = tokio::process::Command::new("ssh")
        .args(&refs)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await
        .context("could not run ssh")?;

    if !status.success() {
        bail!("the donor rejected the connection (wrong password, or SSH is not enabled there)");
    }
    Ok(())
}

/// Generate this machine's ed25519 key pair if it does not exist. Returns the
/// public key. The comment is what donors match on to count their borrowers.
pub async fn ensure_key() -> Result<String> {
    let key = config::ssh_key_path()?;
    let pubkey = key.with_extension("pub");
    if !key.exists() {
        util::run(
            "ssh-keygen",
            &[
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                "runtime-orbit",
                "-f",
                &key.to_string_lossy(),
            ],
        )
        .await
        .context("ssh-keygen failed")?;
    }
    let pk = std::fs::read_to_string(&pubkey)
        .with_context(|| format!("reading {}", pubkey.display()))?;
    Ok(pk.trim().to_string())
}

/// Quick non-interactive connectivity probe (`ssh ... true`).
pub async fn test_connection(cfg: &Config) -> Result<()> {
    let mut args = base_opts(cfg);
    args.push("-o".into());
    args.push("BatchMode=yes".into());
    args.push(cfg.ssh_target());
    args.push("true".into());

    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    tracing::debug!(target = %cfg.ssh_target(), "probing SSH connectivity");
    tracing::trace!(?refs, "ssh (test_connection)");
    if !util::succeeds("ssh", &refs).await {
        bail!(
            "cannot reach {} over SSH with this machine's key.\n\
             Fix it from here with `runtime-orbit setup --ip {}` — it authorizes this\n\
             machine on the donor, with or without a password.",
            cfg.ssh_target(),
            cfg.donor_addr
        );
    }
    Ok(())
}

/// Run a command on the remote host, returning its stdout.
pub async fn remote_exec(cfg: &Config, command: &str) -> Result<String> {
    let mut args = base_opts(cfg);
    args.push("-o".into());
    args.push("BatchMode=yes".into());
    args.push(cfg.ssh_target());
    args.push(command.into());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    tracing::trace!(%command, "remote_exec");
    util::run("ssh", &refs).await
}

/// Is the multiplexed master connection up?
pub async fn master_alive(cfg: &Config) -> bool {
    let mut args = match control_opts() {
        Ok(a) => a,
        Err(_) => return false,
    };
    args.push("-O".into());
    args.push("check".into());
    args.push(cfg.ssh_target());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    util::succeeds("ssh", &refs).await
}

/// Open the master connection and forward the remote docker socket to
/// `local_sock`. Backgrounds itself after authenticating.
pub async fn start_master(cfg: &Config, local_sock: &Path, remote_sock: &str) -> Result<()> {
    // A stale local socket would make ssh refuse the bind.
    let _ = std::fs::remove_file(local_sock);

    let mut args = base_opts(cfg);
    args.extend(control_opts()?);
    args.extend([
        "-M".into(),
        "-f".into(),
        "-N".into(),
        "-o".into(),
        "ControlPersist=yes".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-L".into(),
        format!("{}:{}", local_sock.to_string_lossy(), remote_sock),
        cfg.ssh_target(),
    ]);

    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    tracing::debug!(local = %local_sock.display(), remote = %remote_sock, "opening SSH master + docker socket forward");
    tracing::trace!(?refs, "ssh (start_master)");
    util::run("ssh", &refs)
        .await
        .context("failed to open SSH master connection / forward docker socket")?;
    Ok(())
}

/// Tear down the master connection (and with it every forward).
pub async fn stop_master(cfg: &Config) -> Result<()> {
    if !master_alive(cfg).await {
        return Ok(());
    }
    let mut args = control_opts()?;
    args.extend(["-O".into(), "exit".into(), cfg.ssh_target()]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let _ = util::capture("ssh", &refs).await; // best effort
    Ok(())
}

/// Add a local TCP forward `localhost:port -> 127.0.0.1:port (on host)`.
pub async fn add_forward(cfg: &Config, port: u16) -> Result<()> {
    let mut args = control_opts()?;
    args.extend([
        "-O".into(),
        "forward".into(),
        "-L".into(),
        format!("{port}:127.0.0.1:{port}"),
        cfg.ssh_target(),
    ]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    tracing::debug!(port, "ssh -O forward localhost:{port} -> host:{port}");
    util::run("ssh", &refs)
        .await
        .with_context(|| format!("could not forward port {port} (already in use locally?)"))?;
    Ok(())
}

/// Remove a previously added forward.
pub async fn cancel_forward(cfg: &Config, port: u16) -> Result<()> {
    tracing::debug!(port, "ssh -O cancel (stop forwarding {port})");
    let mut args = control_opts()?;
    args.extend([
        "-O".into(),
        "cancel".into(),
        "-L".into(),
        format!("{port}:127.0.0.1:{port}"),
        cfg.ssh_target(),
    ]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let _ = util::capture("ssh", &refs).await; // best effort
    Ok(())
}
