// SPDX-License-Identifier: Apache-2.0

//! Standalone smoke test / benchmark for `AudioEncoder`, mirroring
//! `crates/capture/src/bin/evdi_probe.rs`'s pattern: a tight, un-slept loop
//! against a real backend, timed rather than frame-counted (audio has no
//! fixed frame count to wait for the way a capture probe can wait for N
//! frames), reporting throughput and inter-packet latency.
//!
//!   audio-probe [--seconds N] [--frame-size 10|20] [--sweep] [--target NODE]
//!
//! `--sweep` runs both 10ms and 20ms frame sizes back to back and reports
//! both, per `docs/07-audio-proposal.md`'s flag that `frame_size_ms` is
//! worth measuring rather than assuming (the same posture that found
//! video's `target-usage` cliff in `docs/02-encode.md`).

use anyhow::{Context, Result};
use audio::{audio_available, AudioEncoder, AudioEncoderConfig};
use std::time::{Duration, Instant};

struct RunStats {
    frame_size_ms: u32,
    elapsed: Duration,
    packets: usize,
    bytes: usize,
    gaps: Vec<Duration>,
    sizes: Vec<usize>,
}

fn run(frame_size_ms: u32, seconds: u64, target_node: Option<String>) -> Result<RunStats> {
    let config = AudioEncoderConfig {
        target_node,
        frame_size_ms,
        ..Default::default()
    };
    let session_start = Instant::now();
    let encoder = AudioEncoder::new(&config, session_start)?;

    let run_for = Duration::from_secs(seconds);
    let started = Instant::now();
    let mut packets = 0usize;
    let mut bytes = 0usize;
    let mut sizes = Vec::new();
    let mut gaps = Vec::new();
    let mut last_arrival: Option<Instant> = None;

    // Tight loop, no sleep between pulls: `pull_packet`'s own timeout is
    // what paces this, so the measured inter-packet gap is a real read of
    // PipeWire/opusenc's cadence, not an artifact of a probe-side sleep.
    while started.elapsed() < run_for {
        match encoder.pull_packet(Duration::from_millis(200))? {
            Some(packet) => {
                let now = Instant::now();
                if let Some(last) = last_arrival {
                    gaps.push(now.duration_since(last));
                }
                last_arrival = Some(now);
                packets += 1;
                bytes += packet.data.len();
                sizes.push(packet.data.len());
            }
            None => continue,
        }
    }
    let elapsed = started.elapsed();
    encoder.stop();

    Ok(RunStats {
        frame_size_ms,
        elapsed,
        packets,
        bytes,
        gaps,
        sizes,
    })
}

fn report(stats: &RunStats) {
    let RunStats {
        frame_size_ms,
        elapsed,
        packets,
        bytes,
        gaps,
        sizes,
    } = stats;

    println!("\n=== frame-size {frame_size_ms}ms ===");
    if *packets == 0 {
        println!("  no packets captured (pipeline produced nothing in {elapsed:?})");
        return;
    }

    println!(
        "  packets   {packets} over {:.2}s ({:.1} packets/s, expected ~{:.1})",
        elapsed.as_secs_f64(),
        *packets as f64 / elapsed.as_secs_f64(),
        1000.0 / *frame_size_ms as f64,
    );
    println!(
        "  bytes     {bytes} total, {:.1} bytes/packet avg, {:.2} kbps",
        *bytes as f64 / *packets as f64,
        (*bytes as f64 * 8.0) / elapsed.as_secs_f64() / 1000.0,
    );

    let mut sorted_sizes = sizes.clone();
    sorted_sizes.sort_unstable();
    println!(
        "  size      min {} B, median {} B, max {} B",
        sorted_sizes[0],
        sorted_sizes[sorted_sizes.len() / 2],
        sorted_sizes[sorted_sizes.len() - 1],
    );

    if !gaps.is_empty() {
        let mut sorted_gaps = gaps.clone();
        sorted_gaps.sort_unstable();
        println!(
            "  inter-packet gap   min {:.2}ms  median {:.2}ms  p95 {:.2}ms  max {:.2}ms",
            ms(sorted_gaps[0]),
            ms(sorted_gaps[sorted_gaps.len() / 2]),
            ms(sorted_gaps[sorted_gaps.len() * 95 / 100]),
            ms(sorted_gaps[sorted_gaps.len() - 1]),
        );
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut seconds = 10u64;
    let mut frame_size_ms = 20u32;
    let mut sweep = false;
    let mut target_node: Option<String> = None;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--seconds" => seconds = it.next().and_then(|v| v.parse().ok()).unwrap_or(10),
            "--frame-size" => {
                frame_size_ms = it.next().and_then(|v| v.parse().ok()).unwrap_or(20)
            }
            "--sweep" => sweep = true,
            "--target" => target_node = it.next(),
            other => anyhow::bail!("unrecognized argument: {other}"),
        }
    }

    println!("=== audio_available() ===");
    let available = audio_available();
    println!("  {available}");
    if !available {
        anyhow::bail!(
            "audio_available() returned false -- check pipewiresrc is installed \
             (gst-inspect-1.0 pipewiresrc) and PipeWire is running (pactl info)"
        );
    }

    if sweep {
        let a = run(10, seconds, target_node.clone()).context("10ms run")?;
        let b = run(20, seconds, target_node).context("20ms run")?;
        report(&a);
        report(&b);
        println!("\n=== verdict ===");
        println!(
            "  10ms: {} packets, {:.1} bytes/packet avg",
            a.packets,
            a.bytes as f64 / a.packets.max(1) as f64
        );
        println!(
            "  20ms: {} packets, {:.1} bytes/packet avg",
            b.packets,
            b.bytes as f64 / b.packets.max(1) as f64
        );
    } else {
        let stats = run(frame_size_ms, seconds, target_node)?;
        report(&stats);
        if stats.packets == 0 {
            anyhow::bail!("captured zero packets -- pipeline did not produce audio");
        }
    }

    Ok(())
}
