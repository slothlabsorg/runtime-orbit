//! Command-line surface.
//!
//! Two roles, two halves of the CLI:
//!
//! * **borrower** — the machine that is short on RAM. Everything at the top
//!   level (`setup`, `doctor`, `up`, `dashboard`, …) runs here.
//! * **donor** — the beefy machine that lends its container runtime. Its
//!   commands live under `runtime-orbit donor …`.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "runtime-orbit",
    bin_name = "runtime-orbit",
    version,
    about = "Borrow a beefier machine's container runtime over your LAN, transparently.",
    long_about = "runtime-orbit points this machine's `docker` at another machine's container \
runtime over SSH and forwards published container ports back to localhost — so heavy builds and \
containers run on the beefy machine while you keep working on this one.\n\n\
Two roles:\n  \
• borrower — this machine, the one low on RAM. Run `runtime-orbit setup --ip <donor-ip>`.\n  \
• donor    — the machine that lends its runtime. Run `runtime-orbit donor setup` there.\n\n\
Short on time? On the low-RAM machine run:  runtime-orbit setup --ip 192.168.1.20",
    after_help = "Aliases: `r-orbit` and `orbit` are installed as shortcuts for `runtime-orbit`.\n\
Docs: https://slothlabs.org/runtime-orbit/docs"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Increase log verbosity (-v info, -vv debug, -vvv trace — logs every ssh/forward action).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Also write logs to this file (in addition to the terminal).
    #[arg(long, global = true, value_name = "PATH")]
    pub log_file: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    // ── Borrower: this machine, the one low on RAM ───────────────────────────
    /// [borrower] Set this machine up to borrow a donor's runtime. Start here.
    Setup {
        /// Donor address (IP or hostname). Positional form: `setup 192.168.1.20`.
        #[arg(value_name = "ADDRESS")]
        address: Option<String>,

        /// Donor address (IP or hostname) — same as the positional form.
        #[arg(long, value_name = "ADDRESS", alias = "host", alias = "ipdonor")]
        ip: Option<String>,

        /// SSH user on the donor (defaults to your current username).
        #[arg(long)]
        user: Option<String>,

        /// SSH port on the donor.
        #[arg(long, default_value_t = 22)]
        port: u16,

        /// Cap how much of the donor's RAM this machine may lean on, in GB.
        #[arg(long, value_name = "GB", alias = "max-borrow-ram")]
        max_ram: Option<f64>,

        /// Keep containers local until this much local RAM is in use, then route to the donor.
        #[arg(long, value_name = "GB", alias = "local-ram-budget")]
        local_ram_threshold: Option<f64>,

        /// Don't prompt — accept detected defaults (for scripts/CI).
        #[arg(long, short = 'y')]
        yes: bool,

        /// Skip the end-to-end self-test container after linking.
        #[arg(long)]
        no_test: bool,
    },

    /// [borrower] Check everything end to end and tell you exactly what to fix.
    Doctor,

    /// [borrower] Route docker to the donor and start forwarding ports (detached).
    Up {
        /// Run the forwarder in the foreground instead of detaching.
        #[arg(long)]
        foreground: bool,
    },

    /// [borrower] Stop forwarding and put docker back on this machine's engine.
    Down,

    /// [borrower] One-shot summary: link, connection, forwarded ports, donor usage.
    Status,

    /// [borrower] Live dashboard — both machines, IPs, RAM, traffic, containers.
    Dashboard {
        /// Seconds between refreshes.
        #[arg(long, short = 'n', value_name = "SECS", default_value_t = 2)]
        interval: u64,

        /// Render once and exit (for scripts, CI, or piping to a file).
        #[arg(long)]
        once: bool,

        /// Print the same data as JSON instead of drawing the dashboard.
        #[arg(long)]
        json: bool,
    },

    /// [borrower] List or manage forwarded ports.
    Ports {
        #[command(subcommand)]
        cmd: Option<PortsCmd>,
    },

    /// [both] Show which container runtimes are installed here (and on the donor).
    Engines,

    /// [borrower] Show the forwarder log.
    Logs {
        /// Follow the log (like `tail -f`).
        #[arg(short, long)]
        follow: bool,
        /// Number of trailing lines to print first.
        #[arg(short = 'n', long, default_value_t = 200)]
        lines: usize,
    },

    /// [borrower] Keep the borrow alive across logins (launchd/systemd).
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },

    /// [borrower] Offer this machine's key to a donor over the LAN, no password.
    Pair {
        /// Port to listen on while pairing.
        #[arg(long, default_value_t = crate::pairing::DEFAULT_PORT)]
        port: u16,

        /// How long to wait for the donor, in minutes.
        #[arg(long, value_name = "MINUTES", default_value_t = 10)]
        minutes: u64,
    },

    /// [borrower] Low-level link to a donor (`user@host`). `setup` does this for you.
    Link {
        /// Target as `user@host` or just `host` (uses your current username).
        target: String,
        /// SSH port on the donor.
        #[arg(long, default_value_t = 22)]
        port: u16,
        /// Override the donor's runtime socket path (auto-detected otherwise).
        #[arg(long)]
        socket: Option<String>,
    },

    // ── Donor: the beefy machine that lends its runtime ──────────────────────
    /// [donor] Commands for the machine that lends its runtime.
    #[command(visible_alias = "donator", alias = "lender", alias = "host")]
    Donor {
        #[command(subcommand)]
        cmd: DonorCmd,
    },

    // ── Routing policy ──────────────────────────────────────────────────────
    /// RAM/CPU budgets: how much to borrow, and when to stop running locally.
    Limits {
        #[command(subcommand)]
        cmd: Option<LimitsCmd>,
    },

    /// Routing table — rules that decide local vs donor per workload.
    #[command(visible_alias = "routes")]
    Route {
        #[command(subcommand)]
        cmd: RouteCmd,
    },

    /// Run one docker command through the routing table (local or donor).
    #[command(disable_help_flag = true)]
    Docker {
        /// Arguments passed straight through to `docker`.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },

    // ── Misc ────────────────────────────────────────────────────────────────
    /// Start the MCP server (stdio) so AI assistants can drive runtime-orbit.
    Mcp,

    /// Support the project — it's built with love, and free.
    Funding,

    /// Internal: the detached forwarder worker. Not for direct use.
    #[command(hide = true)]
    Forward,
}

#[derive(Subcommand, Debug)]
pub enum DonorCmd {
    /// Check this machine can lend its runtime, and say what to fix.
    Doctor,

    /// Prepare this machine to lend its runtime, and print the borrower's command.
    Setup {
        /// IP of the borrower (the low-RAM machine that will connect here).
        #[arg(
            long = "iphost",
            value_name = "IP",
            alias = "borrower-ip",
            alias = "ip"
        )]
        iphost: Option<String>,

        /// Authorize this SSH public key while we're here (same as `donor allow`).
        #[arg(long, value_name = "PUBKEY")]
        allow: Option<String>,

        /// Don't prompt — accept detected defaults.
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// Pull a borrower's key over the LAN and authorize it (no password).
    Pair {
        /// The borrower's IP — the machine running `runtime-orbit pair`.
        #[arg(value_name = "BORROWER_IP")]
        address: String,

        /// The 6-digit code shown on the borrower. Prompted for if omitted.
        #[arg(long)]
        code: Option<String>,

        /// Port the borrower is listening on.
        #[arg(long, default_value_t = crate::pairing::DEFAULT_PORT)]
        port: u16,
    },

    /// Review pairing requests that are waiting for approval.
    Pending {
        /// Approve everything without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// What this machine is currently lending, and to whom.
    Status,

    /// Authorize a borrower's SSH public key (appends to authorized_keys).
    #[command(visible_alias = "add-key")]
    Allow {
        /// The full public key line, e.g. "ssh-ed25519 AAAA... runtime-orbit".
        pubkey: String,

        /// Only accept that key from this IP (adds a `from=` restriction).
        #[arg(long = "iphost", value_name = "IP", alias = "borrower-ip")]
        iphost: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum PortsCmd {
    /// Manually forward an extra TCP port (e.g. a non-docker service on the donor).
    Add { port: u16 },
    /// Stop forwarding a manually-added port.
    Rm { port: u16 },
}

#[derive(Subcommand, Debug)]
pub enum ServiceCmd {
    /// Install a login service that keeps `runtime-orbit up` running.
    Install,
    /// Remove the runtime-orbit login service.
    Uninstall,
    /// Show whether the runtime-orbit login service is installed and running.
    Status,
}

#[derive(Subcommand, Debug)]
pub enum LimitsCmd {
    /// Print the current budgets and how much of each is in use.
    Show,
    /// Change the budgets.
    Set {
        /// Cap on how much donor RAM to lean on, in GB (`off` clears it).
        #[arg(long, value_name = "GB", alias = "max-borrow-ram")]
        max_ram: Option<String>,

        /// Run locally until this much local RAM is in use, then route to the donor.
        #[arg(long, value_name = "GB", alias = "local-ram-budget")]
        local_ram_threshold: Option<String>,

        /// Route to the donor once local CPU load exceeds this many cores' worth.
        #[arg(long, value_name = "LOAD")]
        local_load_threshold: Option<String>,

        /// Default target when no rule and no threshold applies.
        #[arg(long, value_name = "TARGET", value_parser = ["auto", "local", "donor"])]
        prefer: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum RouteCmd {
    /// Show the routing table in evaluation order.
    #[command(visible_alias = "ls")]
    List,

    /// Append a rule. First match wins, so add specific rules first.
    Add {
        /// Glob matched against the image and container name, e.g. `postgres:*`.
        #[arg(value_name = "PATTERN")]
        pattern: String,

        /// Where matching workloads go.
        #[arg(long, value_name = "TARGET", value_parser = ["local", "donor"])]
        target: String,

        /// Why this rule exists (shown in `route list`).
        #[arg(long)]
        note: Option<String>,
    },

    /// Remove a rule by its number in `route list`.
    Rm {
        #[arg(value_name = "N")]
        index: usize,
    },

    /// Explain where a given workload would run, and why.
    Explain {
        /// Image or container name to test against the table, e.g. `postgres:16`.
        #[arg(value_name = "IMAGE")]
        image: String,
    },
}
