// SPDX-License-Identifier: Apache-2.0

//! Virtual output + capture via the `evdi` kernel driver.
//!
//! Used on compositors that implement neither a native "create headless
//! output" IPC (Hyprland, Sway) nor `ext-image-copy-capture-v1` (niri, as of
//! 26.04). evdi creates a real DRM device that the compositor mode-sets like
//! any pluggable monitor, so it needs nothing compositor-specific beyond
//! `wlr-randr` to position and enable the resulting output — niri implements
//! that already.
//!
//! Unlike `session::Capture`, this hands back CPU-mapped bytes, not a
//! DMA-BUF: the `evdi` crate's `Buffer::bytes()` is the only pixel access it
//! exposes. That costs the zero-copy path the Wayland backend gets, but it's
//! the same trade-off the pipeline already accepts elsewhere (see
//! `dmabuf.rs`'s fallback-to-CPU-copy note) and is fast enough in practice.

mod edid;

use anyhow::{Context, Result};
use edid::build_edid;
use evdi::buffer::BufferId;
use evdi::device_config::DeviceConfig;
use evdi::device_node::DeviceNode;
use evdi::events::Mode;
use evdi::handle::Handle;
use std::time::Duration;
use tokio::runtime::Runtime;

const AWAIT_MODE_TIMEOUT: Duration = Duration::from_millis(2000);
/// How long `request_update` waits for a frame before `capture_frame`
/// reports "nothing new" (see its doc comment — that's a normal outcome, not
/// an error, and the caller already retries immediately on it).
///
/// Root cause of the choppiness this once caused: `evdi::Handle::request_update`
/// first tries a synchronous, immediate grab (`evdi_request_update` — instant
/// if a frame is already sitting there), and only falls back to an async wait
/// when that says "not yet". That async wait subscribes to a broadcast
/// channel *after* the immediate check already came back empty — a tokio
/// `broadcast` channel doesn't replay anything sent before a subscriber
/// joins, so an update event landing in that gap is simply missed, and the
/// call blocks for the full timeout waiting for the *next* one. A short
/// timeout turns that miss into a quick retry (the fast synchronous path
/// catches it almost every time) instead of a multi-second stall — measured
/// at 150ms: 380 frames in 5s (75.3fps) against a moving video, 0 timeouts,
/// median capture latency 9.7ms. 5000ms measured 5 total captures in the
/// same window. The fix is entirely about retry granularity, not about the
/// underlying request/notify path getting faster.
///
/// That measurement was against an unpaced tight loop (`evdi_probe`), which
/// is still what this constant is for. `daemon::session`'s real capture loop
/// is paced to a target frame rate instead and picks its own, usually
/// tighter, per-tick timeout — see [`EvdiOutput::capture_frame`].
pub const PROBE_UPDATE_TIMEOUT: Duration = Duration::from_millis(150);

/// A live evdi virtual output: a real DRM device the compositor mode-sets,
/// paired with a single reusable buffer for polling frames out of it.
///
/// Bundles its own single-threaded Tokio runtime because `evdi`'s update loop
/// is `async`, while the rest of this pipeline is plain OS threads — this
/// keeps that async requirement from spreading past this one module.
pub struct EvdiOutput {
    handle: Handle,
    rt: Runtime,
    buffer_id: BufferId,
    mode: Mode,
}

/// Timing for one captured frame, mirroring `session::FrameTiming`'s shape so
/// callers in `crates/daemon` don't need to distinguish backends for it.
#[derive(Debug, Clone, Copy)]
pub struct EvdiFrameTiming {
    pub latency: Duration,
}

/// An evdi handle that's connected and mode-set, but has no buffer
/// registered yet — see [`EvdiOutput::connect`] for why that split matters.
pub struct PendingEvdiOutput {
    handle: Handle,
    rt: Runtime,
    mode: Mode,
}

impl PendingEvdiOutput {
    /// Re-check the current mode and register a buffer for it.
    ///
    /// Call this only once the output's position/scale is done being
    /// reconfigured — re-fetching the mode here (rather than trusting the one
    /// from [`EvdiOutput::connect`]) means that even if a reconfiguration
    /// *did* cause a real mode change underneath us, the buffer still ends up
    /// sized for whatever the final, settled mode actually is.
    pub fn finish(self) -> Result<EvdiOutput> {
        let Self {
            mut handle,
            rt,
            mode,
        } = self;
        let mode = rt
            .block_on(handle.events.await_mode(AWAIT_MODE_TIMEOUT))
            .unwrap_or(mode);
        let buffer_id = handle.new_buffer(&mode);
        Ok(EvdiOutput {
            handle,
            rt,
            buffer_id,
            mode,
        })
    }
}

impl EvdiOutput {
    /// Open (or wait briefly for) an evdi device node, connect it with a
    /// synthesized EDID for `width`x`height`@`refresh_hz`, and block until
    /// the compositor mode-sets it — but stop short of registering a buffer.
    ///
    /// Split from buffer registration deliberately: the caller still has to
    /// discover the connector's compositor-assigned name and reconfigure its
    /// position (evdi doesn't get to choose either), and that reconfiguration
    /// can itself trigger a modeset — even one that changes nothing evdi
    /// considers part of the "mode" (width/height/refresh), a
    /// wlr-output-management client re-asserting the *whole* output
    /// configuration (mode included) is enough to make some compositors redo
    /// it internally. A buffer registered before that point gets left
    /// pointing at a mapping the kernel already invalidated — every
    /// subsequent `request_update` fails with `EFAULT` ("Bad address"). See
    /// `VirtualOutput::create`'s `Compositor::Niri` arm for the full
    /// sequence this is meant to be used in.
    ///
    /// Requires an evdi device node to already exist and be accessible to
    /// this user (see `docs/` for the one-time udev setup) — creating one
    /// (`DeviceNode::add`) needs superuser permissions, which a `--user`
    /// systemd service should not have.
    pub fn connect(width: u32, height: u32, refresh_hz: u32) -> Result<PendingEvdiOutput> {
        let device = DeviceNode::get().context(
            "no evdi device node available. \
             Run the evdi one-time setup (see docs/) to create one — it \
             needs superuser permissions once, not on every run.",
        )?;

        let edid = build_edid(width, height, refresh_hz);
        let config = DeviceConfig::new(&edid, width, height);

        // Must be multi-threaded, not current-thread: `connect` spawns a
        // background task that reads evdi's kernel events and is what
        // actually powers `events`/`request_update`. A current-thread
        // runtime only drives spawned tasks while something is inside
        // `block_on` on that same thread — between our polls (encoding,
        // pushing to the USB transport, the outer session loop) it would sit
        // completely idle. The result isn't a hang, which would be obvious;
        // it's a buffer whose `version` keeps climbing (evdi itself is fine)
        // while `request_update` times out anyway, because the task that
        // would tell us a fresh version arrived never got to run. A
        // dedicated worker thread keeps that task running continuously
        // regardless of what our calling thread is doing.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_time()
            .build()
            .context("starting evdi's Tokio runtime")?;

        // `UnconnectedHandle::connect` is not itself async, but it panics
        // without an entered Tokio reactor — it spawns the background task
        // that later powers `events`/`request_update`. So open, connect, and
        // the first `await_mode` all have to happen inside one `block_on`,
        // not as separate calls each wrapped individually.
        let (handle, mode) = rt.block_on(async {
            // Safety: see the `evdi` crate's top-level "Alpha quality" note —
            // it has not audited evdi's own soundness invariants. This
            // mirrors the crate's own documented basic-usage pattern exactly.
            let unconnected = unsafe { device.open() }.context("opening evdi device node")?;
            let handle = unconnected.connect(&config);
            let mode = handle
                .events
                .await_mode(AWAIT_MODE_TIMEOUT)
                .await
                .context(
                    "compositor never mode-set the evdi output — is wlr-randr \
                     enabling it after this call returns?",
                )?;
            Ok::<_, anyhow::Error>((handle, mode))
        })?;

        Ok(PendingEvdiOutput { handle, rt, mode })
    }

    pub fn width(&self) -> u32 {
        self.mode.width
    }

    pub fn height(&self) -> u32 {
        self.mode.height
    }

    /// Ask evdi for a fresh frame, waiting up to a few seconds for one.
    ///
    /// Returns `Ok(None)` on a plain timeout rather than erroring: a virtual
    /// monitor showing unchanging content (an idle desktop, a static image)
    /// is expected to go quiet between repaints — evdi is damage-driven, so
    /// "nothing new yet" is the normal case here, not a failure. The
    /// equivalent Wayland capture path (`session::Capture::capture_frame`)
    /// has the same property but surfaces it by simply blocking rather than
    /// erroring; evdi's `request_update` needs an explicit timeout, so the
    /// caller has to interpret it instead.
    ///
    /// `timeout` is the caller's to choose rather than fixed internally:
    /// a constant-frame-rate loop budgets this against its own tick
    /// interval (see `daemon::session`), which is a different, usually
    /// much tighter, constraint than the "give it a real chance to
    /// arrive" 150ms `evdi_probe` wants for its own unpaced stress test.
    pub fn capture_frame(&mut self, timeout: Duration) -> Result<Option<EvdiFrameTiming>> {
        use evdi::events::AwaitEventError;
        use evdi::handle::RequestUpdateError;

        let started = std::time::Instant::now();
        match self
            .rt
            .block_on(self.handle.request_update(self.buffer_id, timeout))
        {
            Ok(()) => Ok(Some(EvdiFrameTiming {
                latency: started.elapsed(),
            })),
            Err(RequestUpdateError::AwaitUpdate(AwaitEventError::Timeout)) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("evdi frame update failed: {e:?}")),
        }
    }

    /// Raw pixel bytes of the most recently captured frame. Format is
    /// whatever `pixel_format()` reports (evdi/DisplayLink devices use
    /// `XR24`/`AR24` in practice); interpret with `stride()`.
    pub fn bytes(&self) -> Result<&[u8]> {
        let buffer = self
            .handle
            .get_buffer(self.buffer_id)
            .context("evdi buffer missing — was it unregistered?")?;
        Ok(buffer.bytes())
    }

    pub fn stride(&self) -> Result<u32> {
        let buffer = self
            .handle
            .get_buffer(self.buffer_id)
            .context("evdi buffer missing — was it unregistered?")?;
        u32::try_from(buffer.stride).context("buffer stride out of range")
    }
}

impl Drop for EvdiOutput {
    fn drop(&mut self) {
        self.handle.unregister_buffer(self.buffer_id);
    }
}
