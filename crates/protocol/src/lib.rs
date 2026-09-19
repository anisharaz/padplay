// SPDX-License-Identifier: Apache-2.0

//! Wire format between the host daemon and the Android app.
//!
//! One stream header at connect, then a frame header + payload per access
//! unit, on a single multiplexed socket carrying both video and audio:
//!
//! ```text
//! StreamHeader (24 bytes)
//!   0..4   magic                 "MRLD"
//!   4..6   version               u16   = 2
//!   6..8   width                 u16
//!   8..10  height                u16
//!  10..12  framerate             u16
//!     12   video codec           u8    0 = H.264, 1 = H.265
//!     13   audio codec           u8    0 = none, 1 = Opus
//!  14..16  audio sample rate     u16   Hz; meaningless if audio codec = 0
//!     16   audio channels        u8    1 or 2; meaningless if audio codec = 0
//!  17..19  audio pre-skip        u16   Opus CSD field, see docs/07-audio-proposal.md
//!  19..24  reserved                    5 bytes
//!
//! FrameHeader (16 bytes), repeated
//!   0..4   payload length        u32
//!   4..12  pts, nanoseconds      u64
//!     12   flags                 u8    bit 0 = keyframe (video only; always 0 for audio)
//!     13   stream type           u8    0 = video, 1 = audio
//!  14..16  reserved                    2 bytes
//! ```
//!
//! Payload: unchanged Annex-B access unit for video; one raw Opus packet for
//! audio (self-delimiting, independently decodable — no keyframe concept).
//!
//! Every multi-byte field is **big-endian**. That is deliberate: Kotlin's
//! `DataInputStream` reads big-endian natively, so the device side needs no
//! byte-swapping code at all.
//!
//! Version check is exact-match, checked immediately after magic and before
//! any other field is decoded, on both sides — an old app talking to a new
//! daemon (or vice versa) fails cleanly instead of misparsing a header whose
//! length changed underneath it.

use anyhow::{bail, Result};

pub const MAGIC: [u8; 4] = *b"MRLD";
pub const VERSION: u16 = 2;
pub const STREAM_HEADER_LEN: usize = 24;
pub const FRAME_HEADER_LEN: usize = 16;

/// Default host-side port for `adb forward`.
pub const DEFAULT_PORT: u16 = 27183;
/// Abstract Unix socket the device app listens on.
pub const SOCKET_NAME: &str = "padplay";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    H264 = 0,
    H265 = 1,
}

impl Codec {
    pub fn from_u8(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Codec::H264,
            1 => Codec::H265,
            other => bail!("unknown codec id {other}"),
        })
    }

    /// MIME type MediaCodec expects.
    pub fn mime(self) -> &'static str {
        match self {
            Codec::H264 => "video/avc",
            Codec::H265 => "video/hevc",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AudioCodec {
    None = 0,
    Opus = 1,
}

impl AudioCodec {
    pub fn from_u8(value: u8) -> Result<Self> {
        Ok(match value {
            0 => AudioCodec::None,
            1 => AudioCodec::Opus,
            other => bail!("unknown audio codec id {other}"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StreamType {
    Video = 0,
    Audio = 1,
}

impl StreamType {
    pub fn from_u8(value: u8) -> Result<Self> {
        Ok(match value {
            0 => StreamType::Video,
            1 => StreamType::Audio,
            other => bail!("unknown stream type id {other}"),
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StreamHeader {
    pub width: u16,
    pub height: u16,
    pub framerate: u16,
    pub codec: Codec,
    pub audio_codec: AudioCodec,
    /// Meaningless if `audio_codec` is `AudioCodec::None`.
    pub audio_sample_rate_hz: u16,
    /// Meaningless if `audio_codec` is `AudioCodec::None`.
    pub audio_channels: u8,
    /// Opus CSD field; meaningless if `audio_codec` is `AudioCodec::None`.
    /// See docs/07-audio-proposal.md's "Opus CSD" section for why this is
    /// carried explicitly rather than derived from sample rate/channels.
    pub audio_pre_skip: u16,
}

impl StreamHeader {
    pub fn encode(&self) -> [u8; STREAM_HEADER_LEN] {
        let mut buf = [0u8; STREAM_HEADER_LEN];
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4..6].copy_from_slice(&VERSION.to_be_bytes());
        buf[6..8].copy_from_slice(&self.width.to_be_bytes());
        buf[8..10].copy_from_slice(&self.height.to_be_bytes());
        buf[10..12].copy_from_slice(&self.framerate.to_be_bytes());
        buf[12] = self.codec as u8;
        buf[13] = self.audio_codec as u8;
        buf[14..16].copy_from_slice(&self.audio_sample_rate_hz.to_be_bytes());
        buf[16] = self.audio_channels;
        buf[17..19].copy_from_slice(&self.audio_pre_skip.to_be_bytes());
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < STREAM_HEADER_LEN {
            bail!("stream header too short: {} bytes", buf.len());
        }
        if buf[0..4] != MAGIC {
            bail!("bad magic: {:02x?}", &buf[0..4]);
        }
        let version = u16::from_be_bytes([buf[4], buf[5]]);
        if version != VERSION {
            bail!("unsupported protocol version {version} (expected {VERSION})");
        }
        Ok(Self {
            width: u16::from_be_bytes([buf[6], buf[7]]),
            height: u16::from_be_bytes([buf[8], buf[9]]),
            framerate: u16::from_be_bytes([buf[10], buf[11]]),
            codec: Codec::from_u8(buf[12])?,
            audio_codec: AudioCodec::from_u8(buf[13])?,
            audio_sample_rate_hz: u16::from_be_bytes([buf[14], buf[15]]),
            audio_channels: buf[16],
            audio_pre_skip: u16::from_be_bytes([buf[17], buf[18]]),
        })
    }
}

pub const FLAG_KEYFRAME: u8 = 1 << 0;

#[derive(Debug, Clone, Copy)]
pub struct FrameHeader {
    pub length: u32,
    pub pts_ns: u64,
    pub keyframe: bool,
    pub stream_type: StreamType,
}

impl FrameHeader {
    pub fn encode(&self) -> [u8; FRAME_HEADER_LEN] {
        let mut buf = [0u8; FRAME_HEADER_LEN];
        buf[0..4].copy_from_slice(&self.length.to_be_bytes());
        buf[4..12].copy_from_slice(&self.pts_ns.to_be_bytes());
        buf[12] = if self.keyframe { FLAG_KEYFRAME } else { 0 };
        buf[13] = self.stream_type as u8;
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < FRAME_HEADER_LEN {
            bail!("frame header too short: {} bytes", buf.len());
        }
        Ok(Self {
            length: u32::from_be_bytes(buf[0..4].try_into().unwrap()),
            pts_ns: u64::from_be_bytes(buf[4..12].try_into().unwrap()),
            keyframe: buf[12] & FLAG_KEYFRAME != 0,
            stream_type: StreamType::from_u8(buf[13])?,
        })
    }
}

/// Device → host acknowledgement, sent when a frame is handed to the display.
///
/// This exists purely to make one-way latency measurable. Host and tablet have
/// unrelated monotonic clocks, so timestamps cannot be compared directly; a
/// round trip can be. The host measures send→ack and subtracts a baseline ping
/// RTT to isolate transport plus decode.
///
/// **Audio frames must never be acked.** `session.rs` matches acks to sends by
/// pure FIFO order (a queue of send timestamps, popped front-to-back as acks
/// arrive) — not by PTS, and not by stream type. If an audio send were ever
/// pushed into that same queue, or if the device ever acked an audio frame,
/// the FIFO pairing would silently desync and every subsequent latency
/// measurement would be wrong without any visible error. Only video frames
/// participate in the ack protocol at all.
pub const ACK_LEN: usize = 8;

pub fn encode_ack(pts_ns: u64) -> [u8; ACK_LEN] {
    pts_ns.to_be_bytes()
}

pub fn decode_ack(buf: &[u8]) -> Result<u64> {
    if buf.len() < ACK_LEN {
        bail!("ack too short: {} bytes", buf.len());
    }
    Ok(u64::from_be_bytes(buf[..ACK_LEN].try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video_only_header() -> StreamHeader {
        StreamHeader {
            width: 1920,
            height: 1200,
            framerate: 60,
            codec: Codec::H264,
            audio_codec: AudioCodec::None,
            audio_sample_rate_hz: 0,
            audio_channels: 0,
            audio_pre_skip: 0,
        }
    }

    #[test]
    fn stream_header_round_trips_without_audio() {
        let header = video_only_header();
        let decoded = StreamHeader::decode(&header.encode()).unwrap();
        assert_eq!(decoded.width, 1920);
        assert_eq!(decoded.height, 1200);
        assert_eq!(decoded.framerate, 60);
        assert_eq!(decoded.codec, Codec::H264);
        assert_eq!(decoded.audio_codec, AudioCodec::None);
    }

    #[test]
    fn stream_header_round_trips_with_audio() {
        let header = StreamHeader {
            width: 1920,
            height: 1200,
            framerate: 60,
            codec: Codec::H264,
            audio_codec: AudioCodec::Opus,
            audio_sample_rate_hz: 48_000,
            audio_channels: 2,
            audio_pre_skip: 312,
        };
        let decoded = StreamHeader::decode(&header.encode()).unwrap();
        assert_eq!(decoded.width, 1920);
        assert_eq!(decoded.height, 1200);
        assert_eq!(decoded.framerate, 60);
        assert_eq!(decoded.codec, Codec::H264);
        assert_eq!(decoded.audio_codec, AudioCodec::Opus);
        assert_eq!(decoded.audio_sample_rate_hz, 48_000);
        assert_eq!(decoded.audio_channels, 2);
        assert_eq!(decoded.audio_pre_skip, 312);
    }

    #[test]
    fn stream_header_is_24_bytes() {
        assert_eq!(STREAM_HEADER_LEN, 24);
        assert_eq!(video_only_header().encode().len(), 24);
    }

    #[test]
    fn frame_header_round_trips_video() {
        let header = FrameHeader {
            length: 32357,
            pts_ns: 1_666_666_600,
            keyframe: true,
            stream_type: StreamType::Video,
        };
        let decoded = FrameHeader::decode(&header.encode()).unwrap();
        assert_eq!(decoded.length, 32357);
        assert_eq!(decoded.pts_ns, 1_666_666_600);
        assert!(decoded.keyframe);
        assert_eq!(decoded.stream_type, StreamType::Video);
    }

    #[test]
    fn frame_header_round_trips_audio() {
        let header = FrameHeader {
            length: 320,
            pts_ns: 20_000_000,
            keyframe: false,
            stream_type: StreamType::Audio,
        };
        let decoded = FrameHeader::decode(&header.encode()).unwrap();
        assert_eq!(decoded.length, 320);
        assert_eq!(decoded.pts_ns, 20_000_000);
        assert!(!decoded.keyframe);
        assert_eq!(decoded.stream_type, StreamType::Audio);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = video_only_header().encode();
        buf[0] = b'X';
        assert!(StreamHeader::decode(&buf).is_err());
    }

    #[test]
    fn rejects_wrong_version() {
        let mut buf = video_only_header().encode();
        // Claim v1 (the old 16-byte format) in an otherwise-valid v2 buffer.
        buf[4..6].copy_from_slice(&1u16.to_be_bytes());
        let err = StreamHeader::decode(&buf).unwrap_err();
        assert!(err.to_string().contains("version"));
    }

    #[test]
    fn rejects_v1_shaped_bare_header() {
        // A bare 16-byte v1 header is too short to even reach the version
        // check meaningfully for v2 parsing -- decode must fail cleanly
        // rather than panic or misparse.
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4..6].copy_from_slice(&1u16.to_be_bytes());
        assert!(StreamHeader::decode(&buf).is_err());
    }
}
