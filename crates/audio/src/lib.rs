// SPDX-License-Identifier: Apache-2.0

//! System audio capture and Opus encoding, for streaming a host's audio to
//! the tablet the way an HDMI monitor's speaker gets fed.
//!
//! ```text
//! pipewiresrc(stream.capture.sink=true) -> audioconvert -> audioresample
//!   -> capsfilter(48kHz/stereo/S16LE) -> opusenc -> appsink
//! ```
//!
//! This is its own crate, not folded into `encoder`: audio's "capture" *is*
//! its GStreamer source element (`pipewiresrc`) — there is no zero-copy
//! DMA-BUF handoff to protect between two separately-owned stages the way
//! video needs `capture`/`encoder` split. See `docs/07-audio-proposal.md`'s
//! "Crate structure" section.

use anyhow::{bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;
use std::time::{Duration, Instant};

/// Configuration for [`AudioEncoder`].
///
/// Deliberately does not carry a `session_start: Instant` field (an earlier
/// sketch in `docs/07-audio-proposal.md` had one). `Instant` has no
/// meaningful default, which would force dropping `Default` for this whole
/// struct just to thread through one timestamp anchor. `session_start` is
/// instead a separate argument to [`AudioEncoder::new`] — it is naturally a
/// per-session construction-time value (the same instant video's PTS
/// counter is implicitly anchored to, per the design doc's "PTS / clock
/// origin" section), not a tunable encode parameter like the fields below.
#[derive(Debug, Clone)]
pub struct AudioEncoderConfig {
    /// `None` = follow PipeWire's default sink live, including across a
    /// mid-session default-sink change (headphones plugged in, etc.) — the
    /// same behavior `pw-loopback` itself uses. `Some(name)` pins to a
    /// specific PipeWire node instead.
    ///
    /// The default-follow behavior is implemented as `pipewiresrc` with
    /// `stream-properties = {stream.capture.sink: true}` and no
    /// `target-object`. See "Capture target" in `docs/07-audio-proposal.md`
    /// for why this doesn't redirect the default sink to a new virtual one.
    pub target_node: Option<String>,
    /// Hz. 48_000 matches this desktop's actual default sink rate
    /// (confirmed via `pactl info`), so `audioresample` in the pipeline is
    /// normally a no-op passthrough rather than a real resample.
    pub sample_rate: u32,
    /// 2 (stereo) — this is system/desktop audio, not a voice call.
    pub channels: u32,
    /// Opus target bitrate. 128_000, not the GStreamer default of 64_000:
    /// this is stereo music/game audio, and USB has ample headroom (video
    /// alone uses ~16-20 Mbps of a measured ~275 Mbps link).
    pub bitrate_bps: u32,
    /// Opus frame duration in milliseconds. Must be one of opusenc's
    /// supported values (2, 5, 10, 20, 40, 60 — `gst-inspect-1.0 opusenc`'s
    /// `frame-size` enum). 20ms is the safer default; `audio_probe` sweeps
    /// 10ms vs 20ms (see its own doc comment and `docs/07-audio-proposal.md`'s
    /// "Expected latency" section for why this is worth measuring rather
    /// than assuming).
    pub frame_size_ms: u32,
}

impl Default for AudioEncoderConfig {
    fn default() -> Self {
        Self {
            target_node: None,
            sample_rate: 48_000,
            channels: 2,
            bitrate_bps: 128_000,
            frame_size_ms: 20,
        }
    }
}

/// One encoded Opus packet — self-delimiting and independently decodable,
/// unlike an H.264 access unit, so there is no `keyframe` field to carry.
pub struct EncodedAudioPacket {
    pub data: Vec<u8>,
    /// Nanoseconds elapsed since the `session_start` passed to
    /// [`AudioEncoder::new`], measured when the packet is pulled out of the
    /// pipeline (not GStreamer's own buffer PTS — the video encoder found
    /// those come back rebased by an arbitrary large running-time offset,
    /// see `docs/02-encode.md`'s "GStreamer rewrites PTS" finding, and
    /// there is no reason to expect PipeWire's clock to behave differently).
    /// This is simplest-and-correct for a caller to build a proper
    /// cross-stream timestamp from, per `docs/07-audio-proposal.md`'s
    /// "PTS / clock origin" section; threading a real shared clock between
    /// two independent `gst::Pipeline`s was considered and rejected there
    /// as unnecessary complexity.
    pub pts_ns: u64,
}

/// PipeWire capture + Opus encode as one small GStreamer graph, running on
/// GStreamer's own thread (`pipewiresrc` pulls from PipeWire's real-time
/// thread on its own cadence — there is no `push_frame` here, unlike
/// `encoder::Encoder`'s appsrc-driven design, because there is nothing for
/// a caller to push).
pub struct AudioEncoder {
    pipeline: gst::Pipeline,
    appsink: AppSink,
    session_start: Instant,
}

impl AudioEncoder {
    /// Build and start the pipeline. `session_start` anchors every packet's
    /// `pts_ns` — pass the same instant the video path's frame-index
    /// counter is anchored to, so the two streams' timestamps are directly
    /// comparable at the receiver.
    pub fn new(config: &AudioEncoderConfig, session_start: Instant) -> Result<Self> {
        gst::init().context("initialising GStreamer")?;

        // `stream.capture.sink = true` is what makes this a *monitor* of the
        // sink (i.e. "what's playing"), not a capture of a microphone-style
        // source. Confirmed via a live gst-launch-1.0 run reaching PLAYING
        // (docs/07-audio-proposal.md's implementation-order step 1) before
        // this Rust construction of the same GstStructure was verified.
        let stream_properties = gst::Structure::builder("props")
            .field("stream.capture.sink", true)
            .build();

        let mut pipewiresrc = gst::ElementFactory::make("pipewiresrc")
            .property("stream-properties", &stream_properties);
        if let Some(node) = &config.target_node {
            pipewiresrc = pipewiresrc.property("target-object", node);
        }
        let pipewiresrc = pipewiresrc.build().context("creating pipewiresrc")?;

        let audioconvert = gst::ElementFactory::make("audioconvert")
            .build()
            .context("creating audioconvert")?;
        let audioresample = gst::ElementFactory::make("audioresample")
            .build()
            .context("creating audioresample")?;

        // Explicit S16LE: this is exactly what opusenc's sink pad template
        // requires. Leaving it to negotiation would probably land on the
        // same result, but stating it here documents the contract instead
        // of relying on caps-negotiation magic downstream.
        let raw_caps = gst::Caps::builder("audio/x-raw")
            .field("format", "S16LE")
            .field("layout", "interleaved")
            .field("rate", config.sample_rate as i32)
            .field("channels", config.channels as i32)
            .build();
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .property("caps", &raw_caps)
            .build()
            .context("creating capsfilter")?;

        let opusenc = gst::ElementFactory::make("opusenc")
            .property("bitrate", config.bitrate_bps as i32)
            .property_from_str("frame-size", &config.frame_size_ms.to_string())
            // USB is lossless, unlike the lossy links Opus's FEC/DTX exist
            // for. Same reasoning as the video encoder's keyframe_interval:
            // redundancy for a lossy link is waste on this one. DTX's
            // transmission gaps would also complicate the receiver-side
            // jitter buffer's assumption of a steady packet cadence.
            .property("dtx", false)
            .property("inband-fec", false)
            // This is desktop/game/music audio, not a voice call.
            .property_from_str("audio-type", "generic")
            .build()
            .context("creating opusenc")?;

        let appsink = gst::ElementFactory::make("appsink")
            .property("sync", false)
            // PipeWire pushes on its own real-time cadence regardless of
            // whether a consumer is pulling. `drop = true` bounds how far
            // behind a stalled consumer (e.g. the daemon's pacing thread
            // briefly blocked writing an in-flight video keyframe, per
            // docs/07-audio-proposal.md's "single writer thread" section)
            // can fall: once 8 packets (~160ms at 20ms frames) are queued,
            // the oldest is dropped rather than letting the backlog and its
            // latency grow without bound. This mirrors encoder::Encoder's
            // appsrc `leaky-type=downstream` choice — stale audio delivered
            // late is exactly as unhelpful here as a stale video frame is
            // there.
            .property("max-buffers", 8u32)
            .property("drop", true)
            .build()
            .context("creating appsink")?;

        let pipeline = gst::Pipeline::new();
        let elements = [
            &pipewiresrc,
            &audioconvert,
            &audioresample,
            &capsfilter,
            &opusenc,
            &appsink,
        ];
        pipeline.add_many(elements).context("adding elements")?;
        gst::Element::link_many(elements).context("linking pipeline")?;

        let appsink = appsink.dynamic_cast::<AppSink>().unwrap();

        pipeline
            .set_state(gst::State::Playing)
            .context("starting pipeline")?;

        Ok(Self {
            pipeline,
            appsink,
            session_start,
        })
    }

    /// Pull the next encoded Opus packet, waiting up to `timeout`.
    pub fn pull_packet(&self, timeout: Duration) -> Result<Option<EncodedAudioPacket>> {
        let sample = match self
            .appsink
            .try_pull_sample(gst::ClockTime::from_nseconds(timeout.as_nanos() as u64))
        {
            Some(sample) => sample,
            None => {
                self.check_bus()?;
                return Ok(None);
            }
        };
        // Measured at the moment the packet becomes available to the
        // caller, not derived from the buffer's own (rebased) PTS. See
        // `EncodedAudioPacket::pts_ns`'s doc comment for why.
        let pts_ns = self.session_start.elapsed().as_nanos() as u64;

        let buffer = sample.buffer().context("sample carried no buffer")?;
        let map = buffer.map_readable().context("mapping encoded buffer")?;

        Ok(Some(EncodedAudioPacket {
            data: map.as_slice().to_vec(),
            pts_ns,
        }))
    }

    /// Surface any pipeline error rather than letting it stall silently.
    fn check_bus(&self) -> Result<()> {
        let Some(bus) = self.pipeline.bus() else {
            return Ok(());
        };
        while let Some(msg) = bus.pop() {
            if let gst::MessageView::Error(err) = msg.view() {
                bail!(
                    "pipeline error from {}: {} ({})",
                    err.src().map(|s| s.path_string()).unwrap_or_default(),
                    err.error(),
                    err.debug().unwrap_or_default()
                );
            }
        }
        Ok(())
    }

    pub fn stop(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Drop for AudioEncoder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Whether this system can plausibly serve audio: the `pipewiresrc` plugin
/// is actually installed (a separate package from `gst-plugins-good` on
/// e.g. Arch — see `docs/07-audio-proposal.md`'s "New prerequisites"), a
/// PipeWire server is actually reachable (not just the client library being
/// present), and a default sink exists to monitor.
///
/// Every check is a live round-trip, not a marker taken on trust — the same
/// posture `Compositor::detect()` in `crates/daemon/src/output.rs` uses for
/// compositor identification (e.g. it never trusts `NIRI_SOCKET` alone,
/// because a systemd user service can inherit it from an exited session; it
/// always backs it with a live IPC call).
pub fn audio_available() -> bool {
    if gst::init().is_err() {
        return false;
    }

    // Plugin presence: actually try to instantiate the element rather than
    // checking for a package/file marker. `gst-plugins-good` and PipeWire
    // itself can both be present while `gst-plugin-pipewire` (a separate
    // package) is not — confirmed the hard way on this project's own
    // reference machine before that package was installed.
    if gst::ElementFactory::make("pipewiresrc").build().is_err() {
        return false;
    }

    match pactl_info() {
        Some(info) => info.contains("PipeWire") && has_default_sink(&info),
        None => false,
    }
}

/// Shells out to `pactl info` for a live server round-trip. Duplicated
/// rather than shared with `crates/daemon`'s `run()` helper: this crate has
/// no dependency on `daemon` (nor should it gain one for three lines of
/// process-spawning), and `daemon` is the one that will eventually depend
/// on `audio`, not the other way around.
fn pactl_info() -> Option<String> {
    let output = std::process::Command::new("pactl")
        .arg("info")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn has_default_sink(info: &str) -> bool {
    info.lines()
        .find_map(|line| line.strip_prefix("Default Sink:"))
        .map(|value| {
            let value = value.trim();
            !value.is_empty() && value != "none" && value != "@DEFAULT_SINK@"
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // audio_available()'s live-round-trip checks (real pactl process, real
    // GStreamer plugin registry) can't be meaningfully unit-tested without
    // uninstalling gst-plugin-pipewire or stopping PipeWire on the dev
    // machine. This exercises the one piece of logic that's pure: parsing
    // `pactl info`'s "Default Sink:" line, including the two "no default
    // sink" spellings PipeWire actually uses.
    #[test]
    fn default_sink_parsing() {
        let info = "Server Name: PulseAudio (on PipeWire 1.6.8)\n\
                     Default Sink: alsa_output.pci-0000_00_1f.3.analog-stereo\n";
        assert!(has_default_sink(info));

        assert!(!has_default_sink("Default Sink: none\n"));
        assert!(!has_default_sink("Default Sink: @DEFAULT_SINK@\n"));
        assert!(!has_default_sink(
            "Server Name: PulseAudio (on PipeWire 1.6.8)\n"
        ));
    }
}
