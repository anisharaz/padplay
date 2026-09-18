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
    let mut output = EvdiOutput::connect(width, height, refresh)?.finish()?;
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

    // Tight loop, no sleep — matches how the daemon's main loop actually
    // calls this (immediate retry on a timeout), so the achieved rate here
    // is a real measurement of capture cadence, not an artifact of pacing.
    let run_for = Duration::from_secs(5);
    let started = std::time::Instant::now();
    let mut hits = 0u32;
    let mut misses = 0u32;
    let mut latencies = Vec::new();
    while started.elapsed() < run_for {
        match output.capture_frame()? {
            Some(timing) => {
                hits += 1;
                latencies.push(timing.latency);
            }
            None => misses += 1,
        }
    }
    latencies.sort();
    let median = latencies.get(latencies.len() / 2).copied().unwrap_or_default();
    let elapsed = started.elapsed().as_secs_f64();
    println!(
        "over {elapsed:.1}s: {hits} frames captured ({:.1} fps), {misses} timeouts, median latency {median:?}",
        hits as f64 / elapsed,
    );
    if let Some(last) = latencies.last() {
        println!("min latency {:?}, max latency {last:?}", latencies.first().unwrap());
    }

    let bytes = output.bytes()?;
    let nonzero = bytes.iter().take(300_000).filter(|&&b| b != 0).count();
    println!("last frame nonzero_sample={nonzero}/300000 stride={}", output.stride()?);

    Ok(())
}
