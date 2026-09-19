# Audio: design proposal (not yet implemented)

**Nothing in this document is built.** This is a research-and-design pass —
three independent investigations (Linux capture/encode, wire protocol +
sync, Android decode/playback), synthesized into one plan — done so
implementation can start directly from it later, the way
[06-plasma-backend.md](06-plasma-backend.md) documents KDE groundwork before
any of it existed either.

## Goal

Host system audio, captured and played out the tablet's speaker in sync
with the video, the way an HDMI-connected monitor's speaker works. Must be
switchable off entirely — it adds bandwidth and the video path shouldn't pay
for it when unwanted.

## Why this needs real compression at all

Unlike HDMI (which carries raw, uncompressed pixels and raw PCM because it
has 18+ Gbps of dedicated bandwidth to be lazy with), this project's link is
USB via `adb forward`, measured at ~275 Mbps in practice. Video already
spends ~16-20 Mbps of that on H.264. Audio needs its own real codec (Opus)
for the same reason video does — there's no spare bandwidth to send it raw.

## Architecture

```
PipeWire (default sink's monitor, followed live — not redirected)
  -> pipewiresrc -> audioconvert -> audioresample -> capsfilter(48kHz/stereo)
  -> opusenc(bitrate=128000)     [own gst::Pipeline, own thread, crates/audio]
  -> appsink
        |
        v   (opus_packet, pts_ns)                    video path unchanged:
   session.rs pacing loop  <---- (h264_packet, pts_ns, keyframe) ---- crates/encoder
        |
        v   single writer thread, audio drained before video each tick
   crates/transport::Sender   (StreamType-tagged frames, one socket)
        |
        v   adb forward -> localabstract:padplay
   VideoStream.kt's read loop (single demuxer)
        |
        +--> StreamType::Video --> existing MediaCodec/SurfaceView path (unchanged)
        +--> StreamType::Audio --> new AudioSink -> MediaCodec(Opus) -> AudioTrack
```

## The off switch (what was explicitly asked for)

`--no-audio` on the daemon, mirroring `--show-cursor`'s mechanism but with
**opposite default polarity**: audio defaults **on**, since the entire
premise is "acts like a real monitor" and a real monitor's speaker isn't
opt-in. `--show-cursor` defaults off because it changes what's *captured* (a
visibly surprising compositing side effect); audio capture has no
equivalent surprise, so it gets `--bitrate`/`--fps`'s treatment (always
on, disableable) rather than `--show-cursor`'s (opt-in).

When disabled (flag, or host-side PipeWire init failure — same code path,
not a hard error either way): `StreamHeader.audio_codec = AudioCodec::None`,
the audio `gst::Pipeline` never starts, zero bytes of audio ever hit the
wire. This is a real "off," not a "sends silence" — nothing about the video
path's bandwidth or timing changes when audio is off, by construction (the
audio thread simply doesn't exist).

## Wire protocol

### `StreamHeader` — grows from 16 to 24 bytes, version bumps `1 -> 2`

```
 0..4   magic                 "MRLD"
 4..6   version               u16   = 2
 6..8   width                 u16
 8..10  height                u16
10..12  framerate             u16
   12   video codec           u8    0 = H.264, 1 = H.265
   13   audio codec           u8    0 = none, 1 = Opus
14..16  audio sample rate     u16   Hz (e.g. 48000); meaningless if audio codec = 0
   16   audio channels        u8    1 or 2; meaningless if audio codec = 0
17..19  audio pre-skip        u16   Opus CSD field, see "Opus CSD" below
19..24  reserved                    5 bytes
```

Version check stays exact-match, checked immediately after magic, before
any other field is read (both sides already do this) — an old app talking
to a new daemon or vice versa fails cleanly with a clear error instead of
misparsing a header whose length changed.

### `FrameHeader` — stays 16 bytes, one reserved byte becomes `stream_type`

```
 0..4   payload length        u32
 4..12  pts, nanoseconds      u64
   12   flags                 u8    bit 0 = keyframe (video only; always 0 for audio)
   13   stream type           u8    0 = video, 1 = audio
14..16  reserved                    2 bytes
```

Payload: unchanged Annex-B access unit for video; one raw Opus packet for
audio (self-delimiting, independently decodable — no keyframe concept, so
that bit is always 0 on audio frames).

**One socket, multiplexed — not a second `adb forward`.** A second socket
means a second forward rule, a second accept loop, a second thing to keep in
lockstep with the existing `Phase` state machine, for no ordering benefit:
two independent USB/adb-multiplexed sockets can introduce *more*
differential jitter between the streams than one byte-ordered stream the
sender fully controls the interleaving of. The demux cost is one byte and
one branch on the read side.

### Opus CSD — resolved (was a genuine disagreement between two of the three research passes)

The Android-decode research wanted explicit `sampleRate`/`channels`/
`preSkipSamples` in a header so it can synthesize the OpusHead `MediaCodec`
needs at `configure()` time, plus derived codec-delay/seek-preroll. The
wire-protocol research proposed relying on the Android side synthesizing a
*default* OpusHead from just sample rate + channel count, keeping the
header smaller.

**Decision: include `audio pre-skip` explicitly** (the `17..19` field
above). It's 2 bytes, and getting it wrong doesn't crash anything but does
measurably degrade the first ~10s of audio quality while the decoder's
convergence catches up — not worth risking for 2 bytes. Seek pre-roll
stays a derived constant (80ms, the standard libopus value) rather than
also wired — lower stakes if wrong, and one less thing to keep both sides
agreeing on by convention.

### PTS / clock origin

Video's PTS today is **synthetic**, not a real clock sample:
`index * frame_duration_ns` in `session.rs`'s pacing loop, where `index` is
a per-tick counter. Audio has no equivalent "papering over a gap" need
(PipeWire delivers continuously), so: **audio PTS = nanoseconds elapsed
since one shared `session_start: Instant`**, the same instant video's
`index=0` is implicitly anchored to. Concretely, factor `session_start` out
of `session.rs`'s `next_tick` setup and hand it to whatever spawns the
audio thread.

This resolves cleanly without touching the video encoder or its PTS scheme
at all — a simpler fix than an earlier-considered option (sharing a real
GStreamer pipeline clock/base-time between two `gst::Pipeline`s, which
would have required changing `encoder::Encoder` to read real buffer PTS
instead of its current synthetic counter). No host↔device clock sync is
needed either way — the device only ever compares the two streams' PTS to
each other, never to its own wall clock.

### Single writer thread — required, not optional

`Sender::send_video_frame`/`send_audio_frame` (split from today's single
`send_frame`, specifically so a call site can't accidentally mark an audio
packet as a keyframe) must be the *only* thing writing to the transport
socket, called from *one* thread. The video and audio capture/encode
threads hand finished packets to that thread over channels (mirroring the
existing `packet_rx` pattern) rather than writing directly — concurrent
writers could interleave partial frames from two different packets and
corrupt the wire framing, since a frame's header+payload need to land on
the wire as one unit.

Each pacing tick, drain **audio before video** — audio packets are tiny
(~200-400 bytes at 20ms Opus frames) so this is free, and it bounds how
long an audio packet can sit queued behind an in-flight video keyframe
write.

**Audio frames are never acked.** The existing ack-matching in `session.rs`
pairs acks to sends by pure FIFO order (`sent_at.pop_front()`), not by PTS.
If audio frames were pushed into that same queue, acks would silently
mismatch. This needs a doc comment on the ack path once implemented, since
it's a real constraint that didn't exist before audio existed.

## Sync strategy

Video keeps doing exactly what it does today: decode and render
immediately (`releaseOutputBuffer(index, render = true)`, no
presentation-time scheduling) — this already works and re-architecting it
to timed release would add device-side latency for no benefit this
"floats to whatever rate frames arrive" use case needs.

Audio becomes the de facto presentation master, not by design choice but by
physical necessity: `AudioTrack` fed in blocking-write mode paces itself to
real hardware playback rate whether you build for it or not.

- **Device-side jitter buffer**: 2-3 arrived-but-undecoded Opus packets
  (~40-60ms at 20ms frames) before steady playback starts, absorbing
  transport jitter (a video keyframe briefly hogging the shared socket
  write) without underrunning `AudioTrack`.
- **No continuous sync correction.** Broadcast guidance (ITU-R BT.1359 /
  ATSC) treats audio lagging video by up to ~45ms, or leading by up to
  ~15ms, as generally imperceptible; beyond ~125ms it becomes distracting.
  Keep steady-state lag under ~45ms via the buffer above; only correct on a
  **coarse threshold** (~150-200ms drift — clearly a real stall, not normal
  jitter): drop buffered audio to jump forward, or insert silence to catch
  up. No sample-rate slewing, no NTP-style clock sync, no continuous
  closed-loop corrector — deliberately the cheap end of the tradeoff space
  for a "second monitor," not a movie player with a hard lip-sync bar.
- **Underrun**: feed silence for the gap rather than blocking the audio
  pull thread. Log it; don't build concealment/interpolation — should be
  rare if the buffer depth holds.

Both the 40-60ms buffer depth and the 150-200ms resync threshold are
starting points, not measurements — same posture this project already took
with `fps`/`bitrate_kbps` defaults (`session::Config`'s doc comments cite
actual measured round trips, not guesses). Plan to tune both against real
hardware once built.

## Expected latency

| | Video (measured, existing) | Audio (estimated) |
|---|---|---|
| Capture | 2.13ms median | ~5-21ms (one PipeWire quantum) |
| Encode | 6.26ms median | ~1ms compute + **22.5ms Opus algorithmic delay at 20ms frames** (codec-intrinsic lookahead, not tunable away) |
| **Total** | **~8.4ms** | **~20-40ms** |

The gap is structural (Opus must accumulate samples before it can encode a
frame; video's zero-copy DMA-BUF path has no equivalent accumulation), not
a defect, and it's exactly why sync is done by comparable timestamps at the
receiver rather than by trying to match host-side latency between the two
encoders — see "PTS / clock origin" above. `frame_size` (Opus frame
duration) is the one number worth measuring rather than assuming, the same
way `target-usage` for the video encoder turned out to have a non-obvious
sweet spot (`02-encode.md`) — 10ms frames roughly halve the buffering floor
at a small compression-efficiency cost, worth a swept `audio_probe`
(mirroring `capture/src/bin/evdi_probe.rs`'s pattern) before committing to
20ms.

## Crate structure

**New `crates/audio`**, not folded into `crates/encoder`. Argument: audio's
"capture" *is* its GStreamer source element (`pipewiresrc`) — unlike video,
which needs `capture` and `encoder` as separate crates specifically to
protect a zero-copy DMA-BUF handoff between acquiring the frame and
encoding it. Audio has no equivalent handoff to protect; PipeWire capture
and Opus encode are two elements in one small graph, structurally more like
`vapostproc`→`vah264enc` living inside one crate than like the
capture/encoder split. Keeps `encoder`'s existing DMA-BUF fd-ownership
contract (`02-encode.md`'s "Buffer ownership" section) untouched by an
unrelated change.

```rust
// crates/audio/src/lib.rs — sketch, not final

pub struct AudioEncoderConfig {
    /// None = follow PipeWire's default sink live (the default —
    /// see "capture target" below). Some(name) pins to a specific node.
    pub target_node: Option<String>,
    pub sample_rate: u32,      // 48_000
    pub channels: u32,         // 2
    pub bitrate_bps: u32,      // 128_000
    pub frame_size_ms: u32,    // 20 (needs measurement, see above)
}

pub struct EncodedAudioPacket {
    pub data: Vec<u8>,
    pub pts_ns: u64,
}

pub struct AudioEncoder { /* own gst::Pipeline + AppSink */ }

impl AudioEncoder {
    /// No push_* method, deliberately: pipewiresrc pulls from PipeWire's
    /// own real-time thread on its own cadence. There's nothing for a
    /// caller to push, unlike encoder::Encoder's appsrc-driven design.
    pub fn new(config: &AudioEncoderConfig) -> Result<Self>;
    pub fn pull_packet(&self, timeout: Duration) -> Result<Option<EncodedAudioPacket>>;
}

/// pipewiresrc plugin present, PipeWire server actually live (not just
/// pactl-via-compat-shim), a default sink exists. session.rs degrades to
/// video-only + a warning when this is false, same posture as a missing
/// app install or unauthorized device.
pub fn audio_available() -> bool;
```

### Capture target: follow the default sink live, don't redirect

`pipewiresrc` with `stream.capture.sink = true` and no explicit target
follows whatever the *current* default output sink is, live, including if
the user changes it mid-session (headphones plugged in, etc.) — this is
what `pw-loopback` itself uses. **Not** creating a new virtual sink and
making it the system default (which would be the closer literal analogy to
"selecting an HDMI display as your audio output," and is worth keeping as a
future opt-in flag) — because doing that by default would silently redirect
*all* the user's system audio away from their speakers the instant the
daemon starts, which cuts against this project's own established posture
elsewhere (`COMPATIBILITY.md` on labwc: "reshaping a real part of your
session behind your back is worse than honouring how you configured it").
Consequence worth documenting once built: host speakers and tablet both
play, ~20-40ms apart — a real, visible tradeoff of not redirecting, not an
oversight.

### `opusenc` configuration (GStreamer 1.28.7, confirmed present locally)

Mostly defaults — GStreamer's own Opus defaults are already sane here.
Deliberate deviations: `bitrate=128000` (up from the 64000 default — stereo
system audio, not voice; trivial against USB headroom regardless).
`inband-fec`/`dtx` confirmed **off** (matches this project's own existing
reasoning for video's `keyframe_interval`: redundancy for a lossy link is
waste on USB's lossless one; DTX's transmission gaps would also complicate
the jitter-buffer/sync design above). `audio-type=generic`, not `voip`
(this is system/music/game audio, not a voice call).

## New prerequisites (blocks even testing this, confirmed on the actual dev machine)

- **`gst-plugin-pipewire` is not installed** on this project's own verified
  reference machine, despite `gstreamer`/`gst-plugins-good`/PipeWire itself
  all being present. It's a separate Arch package from `gst-plugins-good`;
  `pipewiresrc` genuinely does not exist without it. First real step of
  implementation: `sudo pacman -S gst-plugin-pipewire`, then
  `gst-inspect-1.0 pipewiresrc` to confirm actual property names before
  writing code against them (none of this proposal's `pipewiresrc`
  property names — `target-object`, `stream-properties` for the
  default-sink-follow behavior — were verified against the real element;
  they're from general PipeWire/GStreamer knowledge and need confirming
  once the plugin is actually installed).
- `opusenc` **is** confirmed present (ships in `gst-plugins-base`).
- `padplay-doctor.sh` needs a new check block: `gst-inspect-1.0
  pipewiresrc`/`opusenc` alongside the existing `vapostproc`/`vah264enc`
  checks, plus a live PipeWire check (`pactl info | grep -i "server
  name.*pipewire"`, not just checking `pactl`/`pw-cli` exist — same
  "confirm with a live round-trip, don't trust markers alone" posture the
  compositor-detection code already uses).
- `COMPATIBILITY.md` needs the new package added to each distro's install
  block, same UNVERIFIED-except-Arch caveat the rest of that table already
  carries.

## Android integration (contract, not internals — those are designed but not written up here in full)

- New `AudioStream` class, structurally a near-clone of `VideoStream`: same
  `Phase` state machine (reused as-is — its wording is already
  codec-agnostic), same "created once, outlives sessions, started/stopped
  by the same accept switch as video" lifecycle, same discipline of never
  blocking the `MediaCodec` async callback (copy bytes + release buffer
  immediately; hand PCM to a dedicated writer thread that owns the
  blocking `AudioTrack.write()` call, mirroring the existing `ackThread`
  pattern).
- `MediaCodec.configure()` for audio: `configure(format, null, null, 0)` —
  no Surface equivalent exists for audio; decoded PCM always comes out as
  `ByteBuffer`s the app must explicitly feed to `AudioTrack`.
- `AudioTrack`: `USAGE_MEDIA` (this app owns the whole screen, behaves like
  the one thing the tablet is doing), `PERFORMANCE_MODE_LOW_LATENCY`
  requested but not depended on (silently downgrades on unsupported
  hardware, safe to always request), buffer sized 2-4x
  `getMinBufferSize()` (favor underrun safety over shaving already-modest
  achievable latency).
- **Zero manifest changes.** `AudioTrack` playback is output-only — no
  `RECORD_AUDIO`, no `MODIFY_AUDIO_SETTINGS`, nothing. Confirmed this
  holds the project's existing "requests no Android permissions at all"
  claim in the top-level README.
- `VideoStream.kt`'s read loop stays the single demuxer: after reading
  `FrameHeader`, branch on `stream_type`; video path untouched; audio
  payload handed through a narrow `AudioSink` interface
  (`onAudioFrame(payload, length, ptsNs)`) to keep the demux boundary
  explicit.
- Status surfacing: minimal state mirroring `VideoStream`'s granularity —
  `phase` (reused), `samplesPlayed` (audio's `framesDecoded` equivalent),
  `lastAudioAtMs`, `lastError`, and `underrunCount`
  (`AudioTrack.getUnderrunCount()`, API 24+) as the single most meaningful
  audio liveness/quality signal, the way frame staleness is for video.
  `StatsWidget` UI wiring is a separate, later step.

## Explicitly deferred (not designed here, flagged as legitimate future work)

- Redirect-all-audio-to-the-tablet mode (`--redirect-audio` or similar),
  as an opt-in alternative to the default follow-and-don't-redirect
  behavior.
- Per-app audio selection/filtering (only capture specific apps' audio).
- Device-initiated audio mute/opt-out (the protocol is host-push-only
  today; this would need a negotiation message before `StreamHeader`).
- Continuous/closed-loop A/V drift correction beyond the coarse
  threshold-based resync above.
- `--stats` reporting separate audio byte/frame counts.

## Suggested implementation order

1. `sudo pacman -S gst-plugin-pipewire`; `gst-inspect-1.0 pipewiresrc` to
   confirm real property names against this proposal's assumptions.
2. `crates/audio` with a standalone `audio_probe` bin (mirroring
   `evdi_probe.rs`) to measure real capture+encode latency and sweep
   `frame_size_ms` before committing to a default.
3. `crates/protocol` + `crates/transport` changes (version bump, header
   changes, `send_video_frame`/`send_audio_frame` split) — independently
   testable against the existing video-only path with `audio_codec = None`.
4. `session.rs` wiring: `session_start` factored out, audio thread +
   channel, audio-drained-before-video in the pacing loop, `--no-audio`
   flag.
5. Android `AudioStream` + `AudioSink` demux wiring, built and tested
   against a daemon already emitting real audio frames from step 4.
6. `padplay-doctor.sh` / `COMPATIBILITY.md` updates.
