//! LAN pairing — authorize a borrower on a donor without anyone typing an SSH
//! password or touching `authorized_keys` by hand.
//!
//! The borrower opens a one-shot listener and shows a 6-digit code. On the donor,
//! `runtime-orbit donor pair <borrower-ip>` connects, presents the code, and gets
//! the public key back — then appends it locally. The code is what stops a
//! neighbour on the same Wi-Fi from helping themselves: without it the listener
//! hands out nothing, and it is accepted exactly once.
//!
//! Wire protocol, newline-delimited ASCII on TCP:
//!
//! ```text
//! → RUNTIME-ORBIT PAIR 1 <code>
//! ← OK <hostname> <ssh-ed25519 AAAA... runtime-orbit>
//! ← ERR <reason>
//! ```

use anyhow::{bail, Context, Result};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Default pairing port. Only open while a pairing is in progress.
pub const DEFAULT_PORT: u16 = 47601;

const PROTO: &str = "RUNTIME-ORBIT PAIR 1";

/// A short numeric code, derived from the key itself so it needs no RNG crate
/// and is stable for the lifetime of the key pair.
///
/// This is a pairing nonce, not a secret: it only has to be unguessable enough
/// that a bystander can't complete a handshake in the minutes the port is open.
pub fn code_for(pubkey: &str, salt: u64) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
    for b in pubkey.as_bytes().iter().chain(&salt.to_le_bytes()) {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{:06}", h % 1_000_000)
}

/// A salt that changes per pairing session, so the code isn't reusable forever.
pub fn session_salt() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 60) // rotates every minute
        .unwrap_or(0)
}

/// Serve the public key to whoever presents `code`, once. Returns the peer that
/// completed the pairing.
///
/// Wrong codes are answered and dropped without ending the session, so a typo on
/// the donor doesn't mean starting over — but each attempt is logged to the
/// caller's screen so a stream of them is visible.
pub async fn serve_once(
    port: u16,
    code: &str,
    hostname: &str,
    pubkey: &str,
    timeout: Duration,
    mut on_attempt: impl FnMut(&str, bool),
) -> Result<String> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("could not listen on port {port} for pairing"))?;

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let accept = tokio::time::timeout_at(deadline, listener.accept()).await;
        let Ok(accepted) = accept else {
            bail!("pairing timed out — nobody connected");
        };
        let (stream, peer) = accepted.context("accept failed")?;
        let peer_ip = peer.ip().to_string();

        match handshake(stream, code, hostname, pubkey).await {
            Ok(true) => {
                on_attempt(&peer_ip, true);
                return Ok(peer_ip);
            }
            Ok(false) | Err(_) => on_attempt(&peer_ip, false),
        }
    }
}

/// One server-side exchange. `Ok(true)` means the code matched and we sent the key.
async fn handshake(stream: TcpStream, code: &str, hostname: &str, pubkey: &str) -> Result<bool> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    // Don't let a connection that says nothing hold the session open.
    tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
        .await
        .context("pairing client timed out")??;

    let inner = reader.get_mut();

    // Strip the prefix *before* trimming the remainder: trimming the whole line
    // first eats the separating space, so a well-formed request with an empty
    // code would be reported as a protocol error rather than a wrong code.
    let Some(offered) = line.trim().strip_prefix(PROTO) else {
        inner.write_all(b"ERR unrecognised protocol\n").await?;
        return Ok(false);
    };

    if offered.trim() != code {
        inner.write_all(b"ERR wrong code\n").await?;
        return Ok(false);
    }

    inner
        .write_all(format!("OK {hostname} {pubkey}\n").as_bytes())
        .await?;
    inner.flush().await?;
    Ok(true)
}

/// The borrower's `(hostname, public key)` as fetched from `addr`.
pub async fn fetch(addr: &str, port: u16, code: &str) -> Result<(String, String)> {
    let stream = tokio::time::timeout(Duration::from_secs(8), TcpStream::connect((addr, port)))
        .await
        .with_context(|| format!("could not reach {addr}:{port}"))?
        .with_context(|| {
            format!(
                "could not reach {addr}:{port} — is `runtime-orbit pair` running on that machine?"
            )
        })?;

    let mut reader = BufReader::new(stream);
    reader
        .get_mut()
        .write_all(format!("{PROTO} {code}\n").as_bytes())
        .await?;
    reader.get_mut().flush().await?;

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
        .await
        .context("the other machine didn't answer")??;

    let line = line.trim();
    if let Some(reason) = line.strip_prefix("ERR ") {
        bail!("pairing refused: {reason}");
    }
    let Some(rest) = line.strip_prefix("OK ") else {
        bail!("unexpected reply from {addr}: {line}");
    };
    let Some((hostname, pubkey)) = rest.split_once(' ') else {
        bail!("malformed reply from {addr}");
    };
    Ok((hostname.to_string(), pubkey.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_six_digits_and_key_dependent() {
        let a = code_for("ssh-ed25519 AAAA", 1);
        let b = code_for("ssh-ed25519 BBBB", 1);
        assert_eq!(a.len(), 6);
        assert!(a.chars().all(|c| c.is_ascii_digit()));
        assert_ne!(a, b);
    }

    #[test]
    fn code_rotates_with_salt() {
        assert_ne!(code_for("k", 1), code_for("k", 2));
    }

    #[tokio::test]
    async fn round_trip() {
        let pubkey = "ssh-ed25519 AAAATESTKEY runtime-orbit";
        let code = code_for(pubkey, 7);
        let port = 47699;
        let server = tokio::spawn({
            let code = code.clone();
            async move {
                serve_once(
                    port,
                    &code,
                    "laptop",
                    pubkey,
                    Duration::from_secs(5),
                    |_, _| {},
                )
                .await
            }
        });
        // Give the listener a moment to bind.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (host, key) = fetch("127.0.0.1", port, &code).await.unwrap();
        assert_eq!(host, "laptop");
        assert_eq!(key, pubkey);
        assert!(server.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn empty_code_is_a_wrong_code_not_a_protocol_error() {
        let pubkey = "ssh-ed25519 AAAATESTKEY runtime-orbit";
        let port = 47697;
        tokio::spawn(async move {
            let _ = serve_once(
                port,
                "111111",
                "laptop",
                pubkey,
                Duration::from_secs(3),
                |_, _| {},
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(150)).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream
            .write_all(format!("{PROTO} \n").as_bytes())
            .await
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.contains("wrong code"), "got: {line}");
    }

    #[tokio::test]
    async fn wrong_code_is_refused() {
        let pubkey = "ssh-ed25519 AAAATESTKEY runtime-orbit";
        let port = 47698;
        tokio::spawn(async move {
            let _ = serve_once(
                port,
                "111111",
                "laptop",
                pubkey,
                Duration::from_secs(3),
                |_, _| {},
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(fetch("127.0.0.1", port, "222222").await.is_err());
    }
}
