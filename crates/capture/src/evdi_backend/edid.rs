// SPDX-License-Identifier: Apache-2.0

//! CVT reduced-blanking timing generation and EDID synthesis for the virtual
//! evdi output. Split out of `mod.rs` since this is a self-contained
//! algorithm with no dependency on the `evdi` crate's runtime types.

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
        let vsync = if vdisplay.is_multiple_of(3) && vdisplay * 4 / 3 == hdisplay {
            4
        } else if vdisplay.is_multiple_of(9) && vdisplay * 16 / 9 == hdisplay {
            5
        } else if vdisplay.is_multiple_of(10) && vdisplay * 16 / 10 == hdisplay {
            6
        } else if (vdisplay.is_multiple_of(4) && vdisplay * 5 / 4 == hdisplay)
            || (vdisplay.is_multiple_of(9) && vdisplay * 15 / 9 == hdisplay)
        {
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
pub(super) fn build_edid(width: u32, height: u32, refresh_hz: u32) -> Vec<u8> {
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
    edid[20] = 0b1010_0101;
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
    let name = b"PadPlay";
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
