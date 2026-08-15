//! `runtime-orbit donor …` — the beefy machine's half of the CLI.
//!
//! A donor lends its container runtime. It needs three things and nothing else:
//! a runtime that's running, an SSH server that accepts the borrower's key, and
//! the discipline not to fall asleep mid-build.

use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use std::io::Write;

use crate::engines;
use crate::metrics;
use crate::util;

// ── donor doctor ────────────────────────────────────────────────────────────

pub async fn doctor() -> Result<()> {
    util::header("runtime-orbit donor doctor");
    println!("  Checking whether this machine can lend its container runtime.\n");
    let mut problems = 0u32;
    let mut warnings = 0u32;

    let vitals = metrics::local_vitals().await;

    // [✓] A runtime with a live socket.
    let found = engines::detect_local().await;
    match engines::preferred(&found) {
        Some(engine) => {
            pass(&format!("Runtime: {} ({})", engine.name, engine.socket));
            if found.len() > 1 {
                let others: Vec<String> = found
                    .iter()
                    .skip(1)
                    .map(|e| format!("{} at {}", e.name, e.socket))
                    .collect();
                note_line(&format!("also available: {}", others.join(", ")));
            }
        }
        None => {
            problems += 1;
            let tools = engines::tools_local().await;
            if tools.is_empty() {
                fail(
                    "No container runtime found",
                    "install one: Docker Desktop, OrbStack, Rancher Desktop, colima or Podman",
                );
            } else {
                fail(
                    &format!(
                        "A runtime is installed ({}) but not running",
                        tools.join(", ")
                    ),
                    "start it, then re-run `runtime-orbit donor doctor`",
                );
            }
        }
    }

    // [✓] The daemon actually answers.
    if util::succeeds("docker", &["info", "--format", "{{.ServerVersion}}"]).await {
        let v = util::run("docker", &["info", "--format", "{{.ServerVersion}}"])
            .await
            .unwrap_or_default();
        pass(&format!("Runtime API answers (engine {})", v.trim()));
    } else {
        problems += 1;
        fail(
            "The runtime is not answering `docker info`",
            "start Docker/OrbStack/Rancher (or `sudo systemctl start docker`)",
        );
    }

    // [✓] SSH server listening.
    if ssh_listening().await {
        pass("SSH server is listening on port 22");
    } else {
        problems += 1;
        fail(
            "SSH server is not listening",
            match std::env::consts::OS {
                "macos" => "System Settings → General → Sharing → Remote Login (turn it on)",
                _ => "sudo systemctl enable --now ssh",
            },
        );
    }

    // [✓] At least one borrower authorized.
    match authorized_orbit_keys() {
        n if n > 0 => pass(&format!(
            "{n} runtime-orbit key(s) authorized in ~/.ssh/authorized_keys"
        )),
        _ => {
            warnings += 1;
            note(
                "No borrower authorized yet",
                "run `runtime-orbit setup --ip <this-machine-ip>` on the other machine: it \
                 authorizes itself, or shows a code for `runtime-orbit donor pair <its-ip>` here",
            );
        }
    }

    // [!] Requests sitting in the inbox unapproved.
    if inbox_has_unauthorized() {
        warnings += 1;
        note(
            "A pairing request is waiting for approval",
            "review it with `runtime-orbit donor pending`",
        );
    }

    // [✓] Reachable address to hand out.
    match &vitals.ip {
        Some(ip) => pass(&format!("LAN address: {ip}")),
        None => {
            warnings += 1;
            note(
                "Could not determine this machine's LAN address",
                "find it with `ipconfig getifaddr en0` (macOS) or `hostname -I` (Linux)",
            );
        }
    }

    // [✓] Worth lending — and honest if it isn't.
    if vitals.mem_total > 0 {
        let total = metrics::fmt_gib_u(vitals.mem_total);
        let avail = metrics::fmt_gib_u(vitals.mem_avail);
        if vitals.mem_total >= 32 * 1024 * 1024 * 1024 {
            pass(&format!(
                "Resources: {} cores · {total} RAM ({avail} free) — plenty to lend",
                vitals.cores
            ));
        } else {
            warnings += 1;
            note(
                &format!(
                    "Resources: {} cores · {total} RAM ({avail} free)",
                    vitals.cores
                ),
                "this will work, but lending from a machine under ~32 GB may not feel like a win",
            );
        }
    }

    // [✓] Sleep — the single most common cause of a borrow dying overnight.
    if let Some(msg) = sleep_warning().await {
        warnings += 1;
        note(&msg, "keep it awake while lending: `sudo pmset -a sleep 0 disablesleep 1` (macOS), or plug in and disable suspend");
    } else {
        pass("Sleep settings won't interrupt a borrow");
    }

    // Verdict.
    println!();
    if problems == 0 && warnings == 0 {
        println!(
            "{} {}",
            "•".green().bold(),
            "No issues found! This machine is ready to lend."
                .green()
                .bold()
        );
    } else if problems == 0 {
        println!(
            "{} {}",
            "•".yellow().bold(),
            format!("Ready to lend, with {warnings} thing(s) worth a look above").yellow()
        );
    } else {
        println!(
            "{} {}",
            "•".red().bold(),
            format!("{problems} blocking issue(s) — fix those and re-run").red()
        );
    }

    if problems == 0 {
        println!();
        print_join(&vitals).await;
    }
    Ok(())
}

// ── donor setup ─────────────────────────────────────────────────────────────

pub async fn setup(iphost: Option<String>, allow_key: Option<String>, yes: bool) -> Result<()> {
    util::header("runtime-orbit donor setup");
    println!("  Preparing this machine to lend its container runtime.\n");

    // Authorize up front if a key was passed, so the checks below can see it.
    if let Some(key) = &allow_key {
        allow(key, iphost.clone()).await?;
        println!();
    }

    let vitals = metrics::local_vitals().await;

    // Runtime present?
    let found = engines::detect_local().await;
    match engines::preferred(&found) {
        Some(e) => util::ok(&format!("runtime: {} ({})", e.name, e.socket)),
        None => anyhow::bail!(
            "no running container runtime found on this machine.\n\
             Start Docker Desktop / OrbStack / Rancher / colima / Podman and re-run.\n\
             Run `runtime-orbit donor doctor` for a full check."
        ),
    }

    // SSH server? We can turn it on from here — it just needs admin rights.
    if ssh_listening().await {
        util::ok("SSH server is listening");
    } else {
        util::warn("the SSH server is off, so nothing can borrow this machine yet.");
        let turn_on = yes
            || inquire::Confirm::new("Turn the SSH server on now? (asks for your admin password)")
                .with_default(true)
                .prompt()
                .unwrap_or(false);
        if turn_on {
            if let Err(e) = enable_ssh_server().await {
                util::warn(&format!("{e:#}"));
                if cfg!(target_os = "macos") {
                    println!(
                        "      {} you can also flip it in System Settings → General → Sharing → {}",
                        "→".dimmed(),
                        "Remote Login".bold()
                    );
                }
            }
        }
    }

    // Sleep is the usual reason an overnight build dies.
    if let Some(msg) = sleep_warning().await {
        util::warn(&msg);
        let fix = yes
            || inquire::Confirm::new("Keep this machine awake while it lends?")
                .with_default(true)
                .prompt()
                .unwrap_or(false);
        if fix {
            if let Err(e) = keep_awake().await {
                util::warn(&format!("{e:#}"));
            }
        }
    }

    // Anything waiting to be approved from a previous attempt.
    if inbox_has_unauthorized() {
        println!();
        util::warn("there are pairing requests waiting for approval.");
        if yes
            || inquire::Confirm::new("Review them now?")
                .with_default(true)
                .prompt()
                .unwrap_or(false)
        {
            pending(yes).await?;
        }
    }

    println!();
    print_join(&vitals).await;

    println!();
    util::header("If it can't log in");
    println!(
        "  On the other machine, {} offers a passwordless option and shows a 6-digit\n  \
         code. Then, here:\n",
        "runtime-orbit setup".cyan()
    );
    let borrower = iphost.as_deref().unwrap_or("<borrower-ip>");
    println!(
        "      {}\n",
        format!("runtime-orbit donor pair {borrower}")
            .bold()
            .green()
    );

    util::header("Check on it later");
    println!(
        "  {}   what this machine is lending right now",
        "runtime-orbit donor status".cyan()
    );
    println!(
        "  {}   full health check",
        "runtime-orbit donor doctor".cyan()
    );
    util::funding_note();
    Ok(())
}

/// The command the borrower needs, spelled out with this machine's address.
async fn print_join(vitals: &metrics::MachineSpecs) {
    let user = util::run("whoami", &[])
        .await
        .unwrap_or_else(|_| "<user>".into());
    let addr = vitals.ip.clone().unwrap_or_else(|| vitals.hostname.clone());

    util::header("Run this on the machine that needs the RAM");
    println!(
        "      {}\n",
        format!("runtime-orbit setup --ip {addr}").bold().green()
    );
    util::info("this machine", &format!("{user}@{addr}"));
    util::info(
        "lending",
        &format!(
            "{} cores · {} RAM ({} free)",
            vitals.cores,
            metrics::fmt_gib_u(vitals.mem_total),
            metrics::fmt_gib_u(vitals.mem_avail)
        ),
    );
}

// ── donor status ────────────────────────────────────────────────────────────

pub async fn status() -> Result<()> {
    util::header("runtime-orbit donor status");
    let vitals = metrics::local_vitals().await;

    util::info("machine", &format!("{} · {}", vitals.hostname, vitals.os));
    util::info("address", vitals.ip.as_deref().unwrap_or("(unknown)"));
    util::info(
        "resources",
        &format!(
            "{} cores · {} RAM ({} free)",
            vitals.cores,
            metrics::fmt_gib_u(vitals.mem_total),
            metrics::fmt_gib_u(vitals.mem_avail)
        ),
    );
    if let Some(l) = vitals.load1 {
        util::info("load (1m)", &format!("{l:.2}"));
    }

    let found = engines::detect_local().await;
    match engines::preferred(&found) {
        Some(e) => util::info("runtime", &format!("{} · {}", e.name, e.socket)),
        None => util::warn("no running runtime — nothing to lend right now"),
    }

    util::info(
        "authorized",
        &format!("{} runtime-orbit key(s)", authorized_orbit_keys()),
    );

    // Who's actually connected over SSH right now.
    util::header("Borrowers connected");
    let peers = ssh_peers().await;
    if peers.is_empty() {
        println!("  {}", "none right now".dimmed());
    } else {
        for p in peers {
            println!("  {} {p}", "•".green());
        }
    }

    // What's running here — that's the RAM someone else isn't spending.
    util::header("Containers running here");
    match util::run(
        "docker",
        &[
            "ps",
            "--format",
            "{{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}",
        ],
    )
    .await
    {
        Ok(out) if !out.trim().is_empty() => {
            for line in out.lines() {
                let f: Vec<&str> = line.split('\t').collect();
                println!(
                    "  {:<24} {:<28} {}",
                    f.first().unwrap_or(&"?").bold(),
                    f.get(1).unwrap_or(&""),
                    f.get(2).unwrap_or(&"").dimmed()
                );
            }
        }
        Ok(_) => println!("  {}", "none".dimmed()),
        Err(_) => util::warn("could not read containers — is the runtime running?"),
    }
    Ok(())
}

// ── donor pair ──────────────────────────────────────────────────────────────

/// Pull a borrower's public key over the LAN and authorize it. The borrower side
/// is `runtime-orbit pair` (or `runtime-orbit setup`, which offers it).
pub async fn pair(address: &str, port: u16, code: Option<String>) -> Result<()> {
    util::header("runtime-orbit donor pair");

    let code = match code {
        Some(c) => c,
        None => inquire::Text::new("Pairing code shown on the other machine:")
            .with_help_message("6 digits, from `runtime-orbit pair` / `runtime-orbit setup`")
            .prompt()
            .context("cancelled")?,
    };

    util::step(&format!("Fetching the key from {address}…"));
    let (hostname, pubkey) = crate::pairing::fetch(address, port, code.trim()).await?;
    util::ok(&format!("got the key from {hostname}"));

    // Pin it to the address we just talked to: we know it's right, and it costs
    // nothing to make the key useless from anywhere else.
    allow(&pubkey, Some(address.to_string())).await?;

    println!();
    util::ok(&format!("{hostname} can now borrow this machine's runtime"));
    Ok(())
}

// ── donor pending ───────────────────────────────────────────────────────────

/// Show (and optionally authorize) keys that landed in `~/.runtime-orbit/inbox`.
///
/// A borrower that could log in with a password drops its key here as a record
/// of the request; this is where you review them after the fact, or authorize
/// one that didn't make it into `authorized_keys`.
pub async fn pending(yes: bool) -> Result<()> {
    util::header("runtime-orbit donor pending");

    let home = dirs::home_dir().context("cannot determine home directory")?;
    let inbox = home.join(".runtime-orbit/inbox");
    if !inbox.exists() {
        println!("  {}", "no pairing requests — the inbox is empty".dimmed());
        return Ok(());
    }

    let authorized = std::fs::read_to_string(home.join(".ssh/authorized_keys")).unwrap_or_default();
    let mut entries: Vec<(String, String)> = Vec::new(); // (label, key)
    for entry in std::fs::read_dir(&inbox)?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pub") {
            continue;
        }
        let Ok(key) = std::fs::read_to_string(&path) else {
            continue;
        };
        let key = key.trim().to_string();
        if !looks_like_key(&key) {
            continue;
        }
        let label = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".into());
        entries.push((label, key));
    }

    if entries.is_empty() {
        println!("  {}", "no pairing requests — the inbox is empty".dimmed());
        return Ok(());
    }

    let mut to_add: Vec<(String, String)> = Vec::new();
    for (label, key) in &entries {
        if authorized.contains(key) {
            println!(
                "  {} {label} {}",
                "[✓]".green().bold(),
                "already authorized".dimmed()
            );
        } else {
            println!(
                "  {} {label} {}",
                "[!]".yellow().bold(),
                "not authorized".yellow()
            );
            to_add.push((label.clone(), key.clone()));
        }
    }

    if to_add.is_empty() {
        println!(
            "\n  {}",
            "nothing to do — every request is already authorized".dimmed()
        );
        return Ok(());
    }

    println!();
    for (label, key) in to_add {
        let approve = yes
            || inquire::Confirm::new(&format!("Authorize {label}?"))
                .with_default(true)
                .prompt()
                .unwrap_or(false);
        if approve {
            allow(&key, None).await?;
        } else {
            util::info("skipped", &label);
        }
    }
    Ok(())
}

// ── system tweaks the donor needs (these are the sudo ones) ──────────────────

/// Turn on the SSH server. Needs admin rights, so `sudo` prompts inside this
/// command rather than sending the user off to a terminal.
async fn enable_ssh_server() -> Result<()> {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("sudo", vec!["systemsetup", "-setremotelogin", "on"])
    } else {
        ("sudo", vec!["systemctl", "enable", "--now", "ssh"])
    };
    util::step("Enabling the SSH server — enter your admin password if asked…");
    let status = tokio::process::Command::new(program)
        .args(&args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await
        .context("could not run sudo")?;
    if !status.success() {
        anyhow::bail!("could not enable the SSH server (was the password accepted?)");
    }
    // macOS needs a beat before the listener is up.
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    if ssh_listening().await {
        util::ok("SSH server is on");
        Ok(())
    } else {
        anyhow::bail!("the SSH server still isn't listening on port 22")
    }
}

/// Stop the machine sleeping out from under an active borrow.
async fn keep_awake() -> Result<()> {
    if !cfg!(target_os = "macos") {
        util::info(
            "linux",
            "mask sleep with: sudo systemctl mask sleep.target suspend.target",
        );
        return Ok(());
    }
    util::step("Disabling sleep — enter your admin password if asked…");
    let status = tokio::process::Command::new("sudo")
        .args(["pmset", "-a", "sleep", "0", "disablesleep", "1"])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await
        .context("could not run sudo")?;
    if !status.success() {
        anyhow::bail!("could not change the sleep settings");
    }
    util::ok("this machine will stay awake while it lends");
    util::info("to undo", "sudo pmset -a sleep 10 disablesleep 0");
    Ok(())
}

// ── donor allow ─────────────────────────────────────────────────────────────

/// Append a borrower's public key to this machine's `~/.ssh/authorized_keys`,
/// optionally pinned to one source IP.
pub async fn allow(pubkey: &str, iphost: Option<String>) -> Result<()> {
    util::header("runtime-orbit donor allow");

    let key = pubkey.trim();
    if !looks_like_key(key) {
        anyhow::bail!(
            "that doesn't look like an SSH public key.\n\
             Expected something like: ssh-ed25519 AAAA... runtime-orbit\n\
             It's the contents of ~/.runtime-orbit/keys/id_orbit_ed25519.pub on the borrower."
        );
    }

    // `from="ip"` makes the key usable only from that machine. Cheap, and it
    // means a leaked public key line is worth even less than it already was.
    let line = match &iphost {
        Some(ip) => format!("from=\"{ip}\" {key}"),
        None => key.to_string(),
    };

    let home = dirs::home_dir().context("cannot determine home directory")?;
    let ssh_dir = home.join(".ssh");
    std::fs::create_dir_all(&ssh_dir).with_context(|| format!("creating {}", ssh_dir.display()))?;
    set_mode(&ssh_dir, 0o700);

    let auth = ssh_dir.join("authorized_keys");
    let existing = std::fs::read_to_string(&auth).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == line) {
        util::ok("that key is already authorized — nothing to do");
        return Ok(());
    }
    // Same key, different restriction: replace rather than stack duplicates.
    let already_present_bare = existing.lines().any(|l| l.trim().ends_with(key));
    let filtered: String = if already_present_bare {
        existing
            .lines()
            .filter(|l| !l.trim().ends_with(key))
            .map(|l| format!("{l}\n"))
            .collect()
    } else {
        existing.clone()
    };

    if already_present_bare {
        std::fs::write(&auth, &filtered)?;
    }

    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&auth)
        .with_context(|| format!("opening {}", auth.display()))?;
    let current = std::fs::read_to_string(&auth).unwrap_or_default();
    if !current.is_empty() && !current.ends_with('\n') {
        writeln!(f)?;
    }
    writeln!(f, "{line}")?;
    set_mode(&auth, 0o600);

    util::ok(&format!("authorized the key in {}", auth.display()));
    if let Some(ip) = iphost {
        util::info("restricted to", &ip);
    }
    util::info("the borrower can now", "runtime-orbit setup --ip <this-ip>");
    Ok(())
}

// ── shared helpers ──────────────────────────────────────────────────────────

fn looks_like_key(s: &str) -> bool {
    let mut parts = s.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(kind), Some(body)) => {
            (kind.starts_with("ssh-") || kind.starts_with("ecdsa-") || kind.starts_with("sk-"))
                && body.len() > 20
        }
        _ => false,
    }
}

/// How many authorized keys look like ours (we tag them with the comment
/// `runtime-orbit`, and older versions used `orbit`).
fn authorized_orbit_keys() -> usize {
    let Some(home) = dirs::home_dir() else {
        return 0;
    };
    let Ok(text) = std::fs::read_to_string(home.join(".ssh/authorized_keys")) else {
        return 0;
    };
    text.lines()
        .filter(|l| {
            let l = l.trim();
            !l.starts_with('#') && (l.ends_with("runtime-orbit") || l.ends_with("orbit"))
        })
        .count()
}

/// True when the inbox holds a key that isn't in `authorized_keys` yet.
fn inbox_has_unauthorized() -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let authorized = std::fs::read_to_string(home.join(".ssh/authorized_keys")).unwrap_or_default();
    let Ok(dir) = std::fs::read_dir(home.join(".runtime-orbit/inbox")) else {
        return false;
    };
    dir.flatten().any(|e| {
        let p = e.path();
        p.extension().and_then(|x| x.to_str()) == Some("pub")
            && std::fs::read_to_string(&p)
                .map(|k| looks_like_key(k.trim()) && !authorized.contains(k.trim()))
                .unwrap_or(false)
    })
}

/// Is something accepting TCP on localhost:22?
async fn ssh_listening() -> bool {
    tokio::time::timeout(
        std::time::Duration::from_millis(800),
        tokio::net::TcpStream::connect("127.0.0.1:22"),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

/// Remote addresses with an established connection to port 22.
async fn ssh_peers() -> Vec<String> {
    let script = r#"
if command -v ss >/dev/null 2>&1; then
  ss -tn state established '( sport = :22 )' 2>/dev/null | awk 'NR>1 {print $4}'
else
  netstat -an 2>/dev/null | awk '/ESTABLISHED/ && ($4 ~ /\.22$/) {print $5}'
fi
"#;
    util::run("sh", &["-c", script])
        .await
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// A warning string when this machine will sleep out from under a borrow.
async fn sleep_warning() -> Option<String> {
    if std::env::consts::OS != "macos" {
        return None;
    }
    let out = util::run("sh", &["-c", "pmset -g 2>/dev/null"])
        .await
        .ok()?;
    let mut sleep_min: Option<u32> = None;
    for line in out.lines() {
        let mut it = line.split_whitespace();
        if let (Some(k), Some(v)) = (it.next(), it.next()) {
            if k == "sleep" {
                sleep_min = v.parse().ok();
            }
        }
    }
    match sleep_min {
        Some(0) | None => None,
        Some(m) => Some(format!(
            "This machine sleeps after {m} minutes idle — a borrow will drop when it does"
        )),
    }
}

fn pass(msg: &str) {
    println!("  {} {msg}", "[✓]".green().bold());
}
fn fail(msg: &str, fix: &str) {
    println!("  {} {}", "[✗]".red().bold(), msg.red());
    println!("      {} {fix}", "→".dimmed());
}
fn note(msg: &str, fix: &str) {
    println!("  {} {msg}", "[!]".yellow().bold());
    println!("      {} {fix}", "→".dimmed());
}
fn note_line(msg: &str) {
    println!("      {} {}", "·".dimmed(), msg.dimmed());
}

#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_mode(_path: &std::path::Path, _mode: u32) {}
