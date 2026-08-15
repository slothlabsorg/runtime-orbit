//! `runtime-orbit logs` — show (and optionally follow) the forwarder's log.

use anyhow::Result;
use std::io::{Read, Seek, SeekFrom};

use crate::config;
use crate::util;

pub async fn run(follow: bool, lines: usize) -> Result<()> {
    let path = config::log_path()?;
    if !path.exists() {
        util::warn("no log yet — start orbit with `runtime-orbit up` first.");
        return Ok(());
    }

    // Print the trailing `lines` lines.
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let tail: Vec<&str> = content.lines().rev().take(lines).collect();
    for line in tail.iter().rev() {
        println!("{line}");
    }

    if !follow {
        return Ok(());
    }

    // Follow: poll for appended bytes until Ctrl-C.
    let mut file = std::fs::File::open(&path)?;
    let mut pos = file.seek(SeekFrom::End(0))?;
    let mut buf = String::new();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(pos);
                if len < pos {
                    // File was truncated/rotated — start over.
                    pos = 0;
                }
                if len > pos {
                    file.seek(SeekFrom::Start(pos))?;
                    buf.clear();
                    file.read_to_string(&mut buf)?;
                    print!("{buf}");
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                    pos = len;
                }
            }
        }
    }
    Ok(())
}
