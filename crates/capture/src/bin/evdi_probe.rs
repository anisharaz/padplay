// SPDX-License-Identifier: Apache-2.0

//! Standalone smoke test for the evdi backend: create a virtual output,
//! enable it with wlr-randr, capture a handful of frames, report basic
//! stats. Independent of the daemon's full pipeline and of any tablet.

use anyhow::{Context, Result};
use capture::evdi_backend::EvdiOutput;
use std::process::Command;
use std::time::Duration;

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let width: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1920);
    let height: u32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1200);
    let refresh: u32 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);

    println!("creating evdi output {width}x{height}@{refresh}...");
    let mut output = EvdiOutput::create(width, height, refresh)?;
    println!(
        "compositor mode-set: {}x{}",
        output.width(),
        output.height()
    );

    // niri does not create a headless output for us — evdi already gave it a
    // real DRM connector. It just needs enabling and positioning, exactly
    // like a physical monitor.
    let status = Command::new("niri")
        .args(["msg", "action", "load-config-file"])
        .status()
        .context("reloading niri config")?;
    println!("niri config reload: {status}");

    let out = Command::new("wlr-randr").output().context("wlr-randr")?;
    println!("--- wlr-randr ---\n{}", String::from_utf8_lossy(&out.stdout));

    let out = Command::new("niri")
        .args(["msg", "outputs"])
        .output()
        .context("niri msg outputs")?;
    println!(
        "--- niri msg outputs ---\n{}",
        String::from_utf8_lossy(&out.stdout)
    );

    print!("capturing 5 frames... ");
    for i in 0..5 {
        let timing = output.capture_frame()?;
        let bytes = output.bytes()?;
        let nonzero = bytes.iter().take(300_000).filter(|&&b| b != 0).count();
        println!(
            "frame {i}: latency={:?} stride={} nonzero_sample={nonzero}/300000",
            timing.latency,
            output.stride()?
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    Ok(())
}
