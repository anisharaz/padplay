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

use anyhow::{Context, Result};
use evdi::buffer::BufferId;
use evdi::device_config::DeviceConfig;
use evdi::device_node::DeviceNode;
use evdi::events::Mode;
use evdi::handle::Handle;
use std::time::Duration;
use tokio::runtime::Runtime;

const AWAIT_MODE_TIMEOUT: Duration = Duration::from_millis(2000);
const UPDATE_TIMEOUT: Duration = Duration::from_millis(500);

/// One CVT reduced-blanking timing, computed for a given resolution/refresh.
///
/// A from-scratch Rust implementation of the published VESA CVT-RB algorithm
/// (see `docs/`), cross-checked against the Linux kernel's `drm_cvt_mode`
/// (`drivers/gpu/drm/drm_modes.c`) for the constants and integer-arithmetic
/// order — this is the same timing generator the kernel itself uses to
/// synthesize modes, so a compositor's EDID parser has seen timings shaped
/// exactly like this before.
struct CvtTiming {
    hactive: u32,
    vactive: u32,
    htotal: u32,
    vtotal: u32,
    hsync_start: u32,
    hsync_end: u32,
    vsync_start: u32,
    vsync_end: u32,
    pixel_clock_khz: u32,
}

impl CvtTiming {
    fn reduced_blanking(hdisplay: u32, vdisplay: u32, vrefresh: u32) -> Self {
        const MIN_VBLANK_US: u64 = 460;
        const H_SYNC: u32 = 32;
        const H_BLANK: u32 = 160;
        const V_FRONT_PORCH: u32 = 3;
        const MIN_V_BACK_PORCH: u32 = 6;
        const HV_FACTOR: u64 = 1000;

        let hactive = hdisplay - (hdisplay % 8);
        let vactive = vdisplay;
        let vfieldrate = u64::from(vrefresh);

        // Vertical sync width, chosen from how closely the aspect ratio
        // matches a small set of standard ratios — this is what the CVT spec
        // itself specifies, not an approximation.
        let vsync = if vdisplay % 3 == 0 && vdisplay * 4 / 3 == hdisplay {
            4
        } else if vdisplay % 9 == 0 && vdisplay * 16 / 9 == hdisplay {
            5
        } else if vdisplay % 10 == 0 && vdisplay * 16 / 10 == hdisplay {
            6
        } else if vdisplay % 4 == 0 && vdisplay * 5 / 4 == hdisplay {
            7
        } else if vdisplay % 9 == 0 && vdisplay * 15 / 9 == hdisplay {
            7
        } else {
            10
        };

        let tmp1 = HV_FACTOR * 1_000_000 - MIN_VBLANK_US * HV_FACTOR * vfieldrate;
        let tmp2 = u64::from(vactive);
        let hperiod = tmp1 / (tmp2 * vfieldrate);

        let mut vbilines = u32::try_from(MIN_VBLANK_US * HV_FACTOR / hperiod).unwrap_or(0) + 1;
        let min_vbilines = V_FRONT_PORCH + vsync + MIN_V_BACK_PORCH;
        if vbilines < min_vbilines {
            vbilines = min_vbilines;
        }

        let vtotal = vactive + vbilines;
        let htotal = hactive + H_BLANK;
        let hsync_end = hactive + H_BLANK / 2;
        let hsync_start = hsync_end - H_SYNC;
        let vsync_start = vactive + V_FRONT_PORCH;
        let vsync_end = vsync_start + vsync;

        let clock_khz = (u64::from(htotal) * HV_FACTOR * 1000) / hperiod;

        Self {
            hactive,
            vactive,
            htotal,
            vtotal,
            hsync_start,
            hsync_end,
            vsync_start,
            vsync_end,
            pixel_clock_khz: u32::try_from(clock_khz).unwrap_or(u32::MAX),
        }
    }

    /// Encode as an 18-byte EDID Detailed Timing Descriptor (EDID 1.4 §3.10.2).
    fn detailed_timing_descriptor(&self) -> [u8; 18] {
        let mut d = [0u8; 18];
        let clock_10khz = self.pixel_clock_khz / 10;
        d[0] = (clock_10khz & 0xff) as u8;
        d[1] = ((clock_10khz >> 8) & 0xff) as u8;

        let hblank = self.htotal - self.hactive;
        let vblank = self.vtotal - self.vactive;

        d[2] = (self.hactive & 0xff) as u8;
        d[3] = (hblank & 0xff) as u8;
        d[4] = (((self.hactive >> 8) & 0xf) << 4) as u8 | ((hblank >> 8) & 0xf) as u8;

        d[5] = (self.vactive & 0xff) as u8;
        d[6] = (vblank & 0xff) as u8;
        d[7] = (((self.vactive >> 8) & 0xf) << 4) as u8 | ((vblank >> 8) & 0xf) as u8;

        let hfront = self.hsync_start - self.hactive;
        let hsync_width = self.hsync_end - self.hsync_start;
        let vfront = self.vsync_start - self.vactive;
        let vsync_width = self.vsync_end - self.vsync_start;

        d[8] = (hfront & 0xff) as u8;
        d[9] = (hsync_width & 0xff) as u8;
        d[10] = (((vfront & 0xf) << 4) | (vsync_width & 0xf)) as u8;
        d[11] = ((((hfront >> 8) & 0x3) << 6)
            | (((hsync_width >> 8) & 0x3) << 4)
            | (((vfront >> 4) & 0x3) << 2)
            | ((vsync_width >> 4) & 0x3)) as u8;

        // Image size left unspecified (0mm x 0mm) — legal per spec, and a
        // virtual display has no physical dimensions to report.
        d[12] = 0;
        d[13] = 0;
        d[14] = 0;
        d[15] = 0; // horizontal border pixels
        d[16] = 0; // vertical border pixels
        // Digital separate sync, +hsync/-vsync, non-interlaced, no stereo —
        // the polarity CVT-RB conventionally uses.
        d[17] = 0b0001_1010;
        d
    }
}

/// Pack a 3-letter manufacturer code into EDID's 5-bit-per-letter, big-endian
/// `u16` (bit 15 reserved zero). `MRE` is an unregistered placeholder — this
/// display is never manufactured or sold, so there is no real PNP ID to use.
fn manufacturer_id(letters: [u8; 3]) -> u16 {
    let code = |c: u8| u16::from(c - b'A' + 1);
    (code(letters[0]) << 10) | (code(letters[1]) << 5) | code(letters[2])
}

/// Build a minimal, spec-valid 128-byte EDID advertising exactly one mode.
///
/// `evdi::device_config::DeviceConfig` requires the caller to supply a
/// well-formed EDID matching the requested pixel dimensions; the crate itself
/// only stores what it's given (`DeviceConfig::new` does no synthesis).
fn build_edid(width: u32, height: u32, refresh_hz: u32) -> Vec<u8> {
    let timing = CvtTiming::reduced_blanking(width, height, refresh_hz);
    let mut edid = vec![0u8; 128];

    edid[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);

    let mfg = manufacturer_id(*b"MRE");
    edid[8] = (mfg >> 8) as u8;
    edid[9] = (mfg & 0xff) as u8;

    edid[10] = 0x01; // product code (low byte)
    edid[11] = 0x00; // product code (high byte)
    edid[12..16].copy_from_slice(&0u32.to_le_bytes()); // serial number
    edid[16] = 1; // week of manufacture (unverifiable for a virtual display)
    edid[17] = 34; // year of manufacture - 1990
    edid[18] = 1; // EDID version
    edid[19] = 4; // EDID revision -> EDID 1.4

    // Digital input, 8 bits per color component, DisplayPort-ish interface —
    // matches how the tablet's decode side actually receives frames (an
    // encoded bitstream over USB, not an analog/DVI signal), so this is
    // descriptive intent more than a real electrical claim.
    edid[20] = 0b1_010_0101;
    edid[21] = 0; // horizontal screen size, cm (unspecified)
    edid[22] = 0; // vertical screen size, cm (unspecified)
    edid[23] = 120; // gamma: (120 + 100) / 100 = 2.20
                     // No DPMS support, RGB color, preferred-timing bit set (bit 1) so
                     // parsers that check it treat our one detailed descriptor as native.
    edid[24] = 0b0000_0010;

    // Chromaticity, established timings, and standard timings are left as
    // zero / "unused" (0x01, 0x01 pairs for standard timings) — nothing here
    // reads color-management metadata from a virtual display, and no
    // compositor mode-set path validates chromaticity plausibility.
    for i in 0..8 {
        edid[38 + i * 2] = 0x01;
        edid[38 + i * 2 + 1] = 0x01;
    }

    edid[54..72].copy_from_slice(&timing.detailed_timing_descriptor());

    // Descriptor #2: monitor name, for anything that displays it (e.g.
    // `niri msg outputs`, `wlr-randr`).
    edid[72] = 0x00;
    edid[73] = 0x00;
    edid[74] = 0x00;
    edid[75] = 0xFC; // monitor name tag
    edid[76] = 0x00;
    let name = b"Moreland";
    let mut name_field = [0x20u8; 13]; // space-padded per spec
    name_field[..name.len()].copy_from_slice(name);
    name_field[name.len()] = 0x0A; // line-feed terminator
    edid[77..90].copy_from_slice(&name_field);

    // Descriptors #3 and #4: dummy (explicitly marks them unused).
    for base in [90usize, 108] {
        edid[base] = 0x00;
        edid[base + 1] = 0x00;
        edid[base + 2] = 0x00;
        edid[base + 3] = 0x10; // dummy descriptor tag
                                // remaining 13 bytes of each stay zero, as the tag requires
    }

    edid[126] = 0; // no extension blocks

    let sum: u32 = edid[0..127].iter().map(|&b| u32::from(b)).sum();
    edid[127] = (256 - (sum % 256)) as u8;

    edid
}

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

impl EvdiOutput {
    /// Open (or wait briefly for) an evdi device node, connect it with a
    /// synthesized EDID for `width`x`height`@`refresh_hz`, and block until
    /// the compositor mode-sets it.
    ///
    /// Requires an evdi device node to already exist and be accessible to
    /// this user (see `docs/` for the one-time udev setup) — creating one
    /// (`DeviceNode::add`) needs superuser permissions, which a `--user`
    /// systemd service should not have.
    pub fn create(width: u32, height: u32, refresh_hz: u32) -> Result<Self> {
        let device = DeviceNode::get().context(
            "no evdi device node available. \
             Run the evdi one-time setup (see docs/) to create one — it \
             needs superuser permissions once, not on every run.",
        )?;

        let edid = build_edid(width, height, refresh_hz);
        let config = DeviceConfig::new(&edid, width, height);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .context("starting evdi's Tokio runtime")?;

        // `UnconnectedHandle::connect` is not itself async, but it panics
        // without an entered Tokio reactor — it spawns the background task
        // that later powers `events`/`request_update`. So open, connect, and
        // the first `await_mode` all have to happen inside one `block_on`,
        // not as separate calls each wrapped individually.
        let (mut handle, mode) = rt.block_on(async {
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

        let buffer_id = handle.new_buffer(&mode);

        Ok(Self {
            handle,
            rt,
            buffer_id,
            mode,
        })
    }

    pub fn width(&self) -> u32 {
        self.mode.width
    }

    pub fn height(&self) -> u32 {
        self.mode.height
    }

    /// Ask evdi for a fresh frame, blocking until it arrives or `timeout`
    /// elapses.
    pub fn capture_frame(&mut self) -> Result<EvdiFrameTiming> {
        let started = std::time::Instant::now();
        self.rt
            .block_on(self.handle.request_update(self.buffer_id, UPDATE_TIMEOUT))
            .map_err(|e| anyhow::anyhow!("evdi frame update failed: {e:?}"))?;
        Ok(EvdiFrameTiming {
            latency: started.elapsed(),
        })
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
