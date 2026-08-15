//! `runtime-orbit funding` — how to support the project. orbit is free and open source;
//! this just tells you where to chip in if it saved your laptop.

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::util;

pub async fn run() -> Result<()> {
    util::header("Support runtime-orbit");
    println!(
        "  runtime-orbit is built with {} by SlothLabs — free and open source, forever.",
        "love".magenta()
    );
    println!("  No company, no VC behind it — just devs fixing their own friction on");
    println!("  nights and weekends, so your work laptop stops choking on Docker.\n");
    println!("  If runtime-orbit saves you time (and RAM), consider supporting the work:\n");
    util::info("Funding page", util::FUNDING_URL);
    util::info("Ko-fi", "https://ko-fi.com/slothlabs");
    util::info(
        "GitHub Sponsors",
        "https://github.com/sponsors/slothlabsorg",
    );
    println!(
        "\n  Thank you — it genuinely keeps the tools coming. {}",
        "♥".magenta()
    );
    Ok(())
}
