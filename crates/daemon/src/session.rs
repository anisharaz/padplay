// SPDX-License-Identifier: Apache-2.0

//! One streaming session: virtual output -> capture -> encode -> USB -> tablet.

use anyhow::{bail, Context, Result};
use capture::evdi_backend::EvdiOutput;
use capture::session::{BufferMode, Capture, CaptureConfig};
use encoder::{Encoder, EncoderConfig};
use std::collections::VecDeque;
use std::io::Read;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use transport::{adb, stream_header, Sender};

use crate::output::VirtualOutput;

const APP_PACKAGE: &str = "com.moreland.display";
const APP_ACTIVITY: &str = "com.moreland.display/.DisplayActivity";

/// Where captured frames come from — a Wayland capture protocol handing back
/// DMA-BUF fds, or evdi handing back CPU-mapped bytes. Deliberately not a
/// trait: the two push different buffer kinds into the encoder, so the
/// interesting logic already lives in `capture_and_push` rather than in
/// per-backend impls of some shared method.
enum FrameSource<'a> {
    // Boxed: Capture is over 600 bytes (its buffer pool and Wayland proxy
    // objects), against Evdi's 8-byte reference -- unboxed, the enum (held
    // as a plain value for the session's whole life) would pay Wayland's
    // size on the Evdi path too.
    Wayland(Box<Capture>),
    Evdi(&'a mut EvdiOutput),
}

impl FrameSource<'_> {
    fn width(&self) -> u32 {
        match self {
            Self::Wayland(c) => c.width,
            Self::Evdi(e) => e.width(),
        }
    }

    fn height(&self) -> u32 {
        match self {
            Self::Wayland(c) => c.height,
            Self::Evdi(e) => e.height(),
        }
    }

    /// Capture and push one frame at a *constant* rate: on the evdi path,
    /// this pushes whatever is currently in the buffer whether or not
    /// anything actually changed since the last call, so the output stream
    /// keeps a steady cadence instead of stalling every time the desktop is
    /// idle. `evdi_timeout` bounds how long to wait for evdi to report a
    /// fresher frame before giving up and reusing what's already there —
    /// see the pacing loop in `run()`, which budgets this against the
    /// target frame interval. The Wayland path is unaffected: its capture
    /// protocol has no equivalent "get the current buffer regardless" mode,
    /// so it keeps blocking for a genuinely new frame each call.
    fn capture_and_push(
        &mut self,
        encoder: &Encoder,
        pts_ns: u64,
        evdi_timeout: Duration,
    ) -> Result<()> {
        match self {
            Self::Wayland(c) => {
                let timing = c.capture_frame()?;
                let dmabuf = c
                    .dmabuf(timing.buffer_index)
                    .context("capture produced no DMA-BUF")?;
                encoder.push_frame(
                    dmabuf.planes[0].fd.as_fd(),
                    dmabuf.planes[0].offset,
                    dmabuf.planes[0].stride,
                    pts_ns,
                )
            }
            Self::Evdi(e) => {
                let _ = e.capture_frame(evdi_timeout)?;
                let stride = e.stride()?;
                encoder.push_frame_bytes(e.bytes()?, stride, pts_ns)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Explicit resolution. `None` matches the device's own panel aspect ratio.
    pub resolution: Option<(u32, u32)>,
    /// Cap on the auto-detected width, to keep encoder load sane.
    pub max_width: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub position_x: i32,
    pub position_y: i32,
    pub output_name: String,
    /// Composite the mouse cursor into the streamed frames.
    pub paint_cursor: bool,
    /// Emit round-trip latency statistics on exit.
    pub stats: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            resolution: None,
            max_width: 1920,
            // Measured on Renoir + Pad 6 at 1920x1200, once capture and encode
            // were on the same GPU: median round trip 33.5 ms at 60, 25.5 ms at
            // 90, 18.8 ms at 120. Higher rates *lower* latency (a shorter frame
            // interval means less waiting for the next slot) and cost fewer
            // bytes, since smaller inter-frame deltas compress better. 144
            // saturates: 163 of 1764 frames unacked and a 208 ms stall.
            fps: 90,
            bitrate_kbps: 20_000,
            // To the left of a 1920x1080 primary at 0x0 — this fork's own
            // reference layout (see `~/.config/niri/config.d/output.kdl`,
            // which places the evdi connector at the same x=-1920, y=0 as a
            // static pre-connect default). The daemon's own `wlr-randr`
            // reposition on every niri connect always wins over that static
            // config, so the two need to agree, and this is the one that
            // actually matches the hardware this fork runs on. Override with
            // `--position`/`--position-y` if your layout differs.
            position_x: -1920,
            position_y: 0,
            output_name: capture::VIRTUAL_OUTPUT_NAME.to_string(),
            paint_cursor: false,
            stats: false,
        }
    }
}

pub fn app_installed(serial: &str) -> Result<bool> {
    let out = adb::shell(serial, &format!("pm list packages {APP_PACKAGE}"))?;
    Ok(out.contains(APP_PACKAGE))
}

/// Stream until `shutdown` is set, the device vanishes, or the app disconnects.
///
/// Every resource here is RAII-scoped: the virtual output and the adb forward
/// are removed when this function returns, however it returns.
pub fn run(serial: &str, config: &Config, shutdown: &AtomicBool) -> Result<()> {
    if !app_installed(serial)? {
        bail!(
            "app not installed on the tablet.\n\
             Build it with `cd android && ANDROID_HOME=/opt/android-sdk ./gradlew assembleRelease`,\n\
             then `adb install -r android/app/build/outputs/apk/release/app-release.apk`."
        );
    }

    // Match the tablet's own aspect ratio unless told otherwise, so the image
    // fills its screen instead of letterboxing.
    let (width, height) = match config.resolution {
        Some(explicit) => explicit,
        None => {
            let panel = adb::display_size(serial)?;
            let scaled = adb::stream_resolution(panel, config.max_width);
            tracing::info!(
                "device panel {}x{} (landscape) -> streaming {}x{}",
                panel.0,
                panel.1,
                scaled.0,
                scaled.1
            );
            scaled
        }
    };

    let mut output = VirtualOutput::create(
        &config.output_name,
        width,
        height,
        config.fps,
        config.position_x,
        config.position_y,
    )?;
    if output.applied_mode() {
        tracing::info!(
            "virtual output {} at {}x{}@{}",
            output.name(),
            width,
            height,
            config.fps
        );
    } else {
        tracing::info!(
            "using existing output {} with its own mode; encoding at whatever \
             size it reports",
            output.name()
        );
    }

    let forward = adb::Forward::new(
        serial,
        protocol::DEFAULT_PORT,
        &format!("localabstract:{}", protocol::SOCKET_NAME),
    )?;
    let port = forward.local_port()?;

    // Deliberately no `am force-stop` here. It used to run before every
    // session specifically to clear a stale instance's grip on the
    // process-wide abstract socket name -- but the app is designed to keep
    // that socket open across sessions now (VideoStream's accept loop
    // already handles a sequence of connections without restarting), and
    // `DisplayActivity` is `singleTask`, so `am start` below reuses the
    // running instance instead of spawning a duplicate. Killing it here
    // would just be visible, pointless churn: the app quitting and
    // relaunching on every single connection.
    //
    // Wake the tablet first. A sleeping or locked device starts the activity
    // without ever making it visible, so no surface is created and the session
    // dies with "no valid surface available" — which looks like a crash but is
    // just a dark screen. The manifest's showWhenLocked/turnScreenOn only help
    // once the activity is actually being brought up.
    let _ = adb::shell(serial, "input keyevent KEYCODE_WAKEUP");
    let _ = adb::shell(serial, "wm dismiss-keyguard");
    std::thread::sleep(Duration::from_millis(400));

    adb::shell(serial, &format!("am start -n {APP_ACTIVITY}"))
        .context("launching the display app")?;
    std::thread::sleep(Duration::from_millis(2500));

    let output_name = output.name().to_string();

    // evdi's own handle already captures frames — a separate Wayland capture
    // session neither exists for it nor is needed. Everything else
    // (Hyprland, labwc) still goes through `capture::session::Capture`.
    let mut frame_source = if let Some(evdi) = output.evdi_mut() {
        FrameSource::Evdi(evdi)
    } else {
        // Probe what this machine's encoder can actually import rather than
        // assuming; the accepted modifier set is GPU-vendor specific.
        let allowed_modifiers = encoder::supported_modifiers(capture::XR24);
        tracing::debug!("encoder accepts modifiers {allowed_modifiers:02x?}");
        FrameSource::Wayland(Box::new(Capture::new(
            &output_name,
            &CaptureConfig {
                mode: BufferMode::Dmabuf,
                pool_size: 3,
                allowed_modifiers,
                paint_cursor: config.paint_cursor,
            },
        )?))
    };

    let width = frame_source.width();
    let height = frame_source.height();

    let encoder = Arc::new(match &frame_source {
        FrameSource::Wayland(c) => Encoder::new(&EncoderConfig {
            width,
            height,
            framerate: config.fps,
            bitrate_kbps: config.bitrate_kbps,
            fourcc: c.format,
            modifier: c.modifier.unwrap_or(0),
            ..Default::default()
        })?,
        // No DMA-BUF, so no format/modifier to negotiate — `new_cpu` fixes
        // the source format at `BGRx` (evdi's own pixel format) instead.
        FrameSource::Evdi(_) => Encoder::new_cpu(&EncoderConfig {
            width,
            height,
            framerate: config.fps,
            bitrate_kbps: config.bitrate_kbps,
            ..Default::default()
        })?,
    });

    let mut sender = Sender::connect(port, &stream_header(width, height, config.fps))
        .context("connecting to the app — is it in the foreground on the tablet?")?;
    tracing::info!("streaming to {serial}");

    let running = Arc::new(AtomicBool::new(true));
    let sent_at: Arc<Mutex<VecDeque<Instant>>> = Arc::new(Mutex::new(VecDeque::new()));
    let round_trips: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));

    let ack_thread = {
        let mut reader = sender.ack_reader()?;
        let sent_at = Arc::clone(&sent_at);
        let round_trips = Arc::clone(&round_trips);
        let running = Arc::clone(&running);
        std::thread::spawn(move || {
            let mut buf = [0u8; protocol::ACK_LEN];
            let mut filled = 0usize;
            while running.load(Ordering::Relaxed) {
                match reader.read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => {
                        filled += n;
                        if filled == protocol::ACK_LEN {
                            filled = 0;
                            if let Some(sent) = sent_at.lock().unwrap().pop_front() {
                                round_trips.lock().unwrap().push(sent.elapsed());
                            }
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
        })
    };

    let (packet_tx, packet_rx) = std::sync::mpsc::channel::<(Vec<u8>, u64, bool)>();
    let drain_thread = {
        let encoder = Arc::clone(&encoder);
        let running = Arc::clone(&running);
        std::thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                match encoder.pull_packet(Duration::from_millis(100)) {
                    Ok(Some(p)) => {
                        if packet_tx.send((p.data, p.pts_ns, p.keyframe)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::error!("encoder: {e}");
                        break;
                    }
                }
            }
        })
    };

    let frame_duration_ns = 1_000_000_000u64 / u64::from(config.fps);
    let frame_duration = Duration::from_nanos(frame_duration_ns);
    // Budgeted against the tick, not the old fixed 150ms: on evdi, waiting
    // for a fresher frame competes with the tick's own deadline. Half the
    // interval leaves room for encode+push, and reusing a ~1-tick-stale
    // frame on a miss is imperceptible at a real frame rate -- constant
    // cadence matters more here than always having the very latest pixels.
    let evdi_timeout =
        (frame_duration / 2).clamp(Duration::from_millis(2), Duration::from_millis(50));
    let mut index = 0u64;
    let mut next_tick = Instant::now();
    let result = (|| -> Result<()> {
        while !shutdown.load(Ordering::Relaxed) {
            frame_source.capture_and_push(&encoder, index * frame_duration_ns, evdi_timeout)?;
            index += 1;

            while let Ok((data, pts, keyframe)) = packet_rx.try_recv() {
                sent_at.lock().unwrap().push_back(Instant::now());
                sender.send_frame(&data, pts, keyframe)?;
            }

            // Constant-rate pacing. On evdi this is what actually produces a
            // steady output instead of one frame per real repaint; on
            // Wayland it just caps an otherwise-uncapped capture loop at the
            // configured rate, which it never was before. If something
            // (a stall, a slow encode) put us more than a full interval
            // behind, resync to now instead of bursting frames to catch up.
            next_tick += frame_duration;
            let now = Instant::now();
            if now < next_tick {
                std::thread::sleep(next_tick - now);
            } else if now > next_tick + frame_duration {
                next_tick = now;
            }
        }
        Ok(())
    })();

    running.store(false, Ordering::Relaxed);
    let _ = drain_thread.join();
    let _ = ack_thread.join();

    if config.stats {
        report(&round_trips.lock().unwrap(), index, sender.bytes_sent());
    }

    // No `am force-stop` here either (see the comment above the launch
    // sequence): the app notices the socket closing on its own and falls
    // back to its Normal/Home view, ready for the next connection. Killing
    // the process would just be a visible flash back to the launcher for no
    // reason.

    drop(forward);
    drop(output);
    result
}

fn report(trips: &[Duration], frames: u64, bytes: u64) {
    println!("\n  frames captured   {frames}");
    println!("  bytes sent        {:.1} MB", bytes as f64 / 1e6);
    println!("  acks received     {}", trips.len());
    if trips.is_empty() {
        println!("\n  no acknowledgements — check `adb logcat -s Moreland`");
        return;
    }
    let mut trips = trips.to_vec();
    trips.sort_unstable();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!("\n  round trip: host send -> device render -> host ack");
    println!("    min       {:>8.2} ms", ms(trips[0]));
    println!("    median    {:>8.2} ms", ms(trips[trips.len() / 2]));
    println!("    p95       {:>8.2} ms", ms(trips[trips.len() * 95 / 100]));
    println!("    max       {:>8.2} ms", ms(trips[trips.len() - 1]));
}
