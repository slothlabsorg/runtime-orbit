//! Best-effort LAN discovery: find hosts on the local /24 subnets that accept
//! TCP connections on the SSH port. Used by `runtime-orbit setup` to suggest a host so
//! the user doesn't have to hunt for an IP address.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// A reachable host on the LAN.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Candidate {
    pub ip: Ipv4Addr,
}

/// Local, private IPv4 addresses of this machine (one per active interface).
pub fn local_ipv4s() -> Vec<Ipv4Addr> {
    let mut out = BTreeSet::new();
    // `ifconfig` (macOS + most Linux) then `ip -4 addr` as a fallback.
    let text = run_sync("ifconfig", &[])
        .or_else(|| run_sync("ip", &["-4", "addr"]))
        .unwrap_or_default();
    for tok in text.split_whitespace() {
        if let Ok(ip) = tok.trim_end_matches(&['/'][..]).parse::<Ipv4Addr>() {
            if is_private(&ip) && !ip.is_loopback() {
                out.insert(ip);
            }
        }
        // `ip addr` prints CIDR like 192.168.1.8/24 — split on '/'.
        if let Some((head, _)) = tok.split_once('/') {
            if let Ok(ip) = head.parse::<Ipv4Addr>() {
                if is_private(&ip) && !ip.is_loopback() {
                    out.insert(ip);
                }
            }
        }
    }
    out.into_iter().collect()
}

fn is_private(ip: &Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 10 || (o[0] == 172 && (16..=31).contains(&o[1])) || (o[0] == 192 && o[1] == 168)
}

/// Scan the /24 of every local private interface for open `port`, returning the
/// reachable hosts (excluding this machine). Bounded concurrency + short timeout
/// so a full sweep takes a couple of seconds.
pub async fn scan(port: u16) -> Vec<Candidate> {
    let mine = local_ipv4s();
    let mut targets: BTreeSet<Ipv4Addr> = BTreeSet::new();
    for ip in &mine {
        let o = ip.octets();
        for host in 1u16..=254 {
            let cand = Ipv4Addr::new(o[0], o[1], o[2], host as u8);
            if &cand != ip {
                targets.insert(cand);
            }
        }
    }

    let found: Vec<Ipv4Addr> = stream::iter(targets)
        .map(|ip| async move {
            let addr = SocketAddr::new(IpAddr::V4(ip), port);
            match timeout(Duration::from_millis(350), TcpStream::connect(addr)).await {
                Ok(Ok(_)) => Some(ip),
                _ => None,
            }
        })
        .buffer_unordered(128)
        .filter_map(|r| async move { r })
        .collect()
        .await;

    let mut cands: Vec<Candidate> = found.into_iter().map(|ip| Candidate { ip }).collect();
    cands.sort();
    cands.dedup();
    cands
}

fn run_sync(program: &str, args: &[&str]) -> Option<String> {
    std::process::Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}
