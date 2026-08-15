mod cli;
mod commands;
mod config;
mod docker_ctx;
mod engines;
mod forwarder;
mod host;
mod mcp;
mod metrics;
mod net_scan;
mod pairing;
mod routing;
mod ssh;
mod util;

use clap::Parser;
use cli::{Cli, Command, DonorCmd, LimitsCmd, PortsCmd, RouteCmd, ServiceCmd};

#[tokio::main]
async fn main() {
    // Pick up a pre-0.2 `~/.orbit` before anything reads config.
    config::migrate_legacy_dir();

    let args = Cli::parse();
    init_tracing(args.verbose, args.log_file.as_deref());

    let result = match args.command {
        // ── borrower ────────────────────────────────────────────────────────
        Command::Setup {
            address,
            ip,
            user,
            port,
            max_ram,
            local_ram_threshold,
            yes,
            no_test,
        } => {
            commands::setup::run(
                address.or(ip),
                user,
                port,
                max_ram,
                local_ram_threshold,
                yes,
                no_test,
            )
            .await
        }
        Command::Doctor => commands::doctor::run().await,
        Command::Up { foreground } => commands::up::run(foreground).await,
        Command::Down => commands::down::run().await,
        Command::Status => commands::status::run().await,
        Command::Dashboard {
            interval,
            once,
            json,
        } => commands::dashboard::run(interval, once, json).await,
        Command::Ports { cmd } => match cmd {
            Some(PortsCmd::Add { port }) => commands::ports::add(port).await,
            Some(PortsCmd::Rm { port }) => commands::ports::rm(port).await,
            None => commands::ports::list().await,
        },
        Command::Engines => commands::engines_cmd::run().await,
        Command::Logs { follow, lines } => commands::logs::run(follow, lines).await,
        Command::Service { cmd } => match cmd {
            ServiceCmd::Install => commands::service::install().await,
            ServiceCmd::Uninstall => commands::service::uninstall().await,
            ServiceCmd::Status => commands::service::status().await,
        },
        Command::Pair { port, minutes } => commands::pair::run(port, minutes).await,
        Command::Link {
            target,
            port,
            socket,
        } => commands::link::run(&target, port, socket).await,

        // ── donor ───────────────────────────────────────────────────────────
        Command::Donor { cmd } => match cmd {
            DonorCmd::Doctor => commands::donor::doctor().await,
            DonorCmd::Setup { iphost, allow, yes } => {
                commands::donor::setup(iphost, allow, yes).await
            }
            DonorCmd::Pair {
                address,
                code,
                port,
            } => commands::donor::pair(&address, port, code).await,
            DonorCmd::Pending { yes } => commands::donor::pending(yes).await,
            DonorCmd::Status => commands::donor::status().await,
            DonorCmd::Allow { pubkey, iphost } => commands::donor::allow(&pubkey, iphost).await,
        },

        // ── routing policy ──────────────────────────────────────────────────
        Command::Limits { cmd } => match cmd {
            Some(LimitsCmd::Set {
                max_ram,
                local_ram_threshold,
                local_load_threshold,
                prefer,
            }) => {
                commands::limits::set(max_ram, local_ram_threshold, local_load_threshold, prefer)
                    .await
            }
            Some(LimitsCmd::Show) | None => commands::limits::show().await,
        },
        Command::Route { cmd } => match cmd {
            RouteCmd::List => commands::route::list().await,
            RouteCmd::Add {
                pattern,
                target,
                note,
            } => commands::route::add(pattern, target, note).await,
            RouteCmd::Rm { index } => commands::route::rm(index).await,
            RouteCmd::Explain { image } => commands::route::explain(image).await,
        },
        Command::Docker { args } => commands::run_docker::run(args).await,

        // ── misc ────────────────────────────────────────────────────────────
        Command::Mcp => commands::mcp::run().await,
        Command::Funding => commands::funding::run().await,
        Command::Forward => commands::run_worker::run().await,
    };

    if let Err(e) = result {
        eprintln!("\n{} {:#}", owo_colors::OwoColorize::red(&"error:"), e);
        std::process::exit(1);
    }
}

fn init_tracing(verbose: u8, log_file: Option<&str>) {
    use tracing_subscriber::prelude::*;

    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let make_filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            // The crate name is the filter target, and it has a hyphen in the
            // package name but an underscore as a Rust identifier.
            tracing_subscriber::EnvFilter::new(format!("runtime_orbit={level}"))
        })
    };

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .with_filter(make_filter());

    let file_layer = log_file.and_then(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(|f| {
                tracing_subscriber::fmt::layer()
                    .with_target(false)
                    .with_ansi(false)
                    .with_writer(f)
                    .with_filter(make_filter())
            })
    });

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .init();
}
