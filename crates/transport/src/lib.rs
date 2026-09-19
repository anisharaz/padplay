// SPDX-License-Identifier: Apache-2.0

//! Framed video transport to the device over `adb forward`.

pub mod adb;

use anyhow::{Context, Result};
use protocol::{AudioCodec, Codec, FrameHeader, StreamHeader, StreamType};
use std::io::{IoSlice, Write};
use std::net::TcpStream;
use std::time::Duration;

pub struct Sender {
    stream: TcpStream,
    frames_sent: u64,
    bytes_sent: u64,
}

impl Sender {
    /// Connect to the adb-forwarded port and send the stream header.
    pub fn connect(port: u16, header: &StreamHeader) -> Result<Self> {
        let stream = TcpStream::connect(("127.0.0.1", port))
            .with_context(|| format!("connecting to forwarded port {port}"))?;

        // Nagle batches small writes waiting for an ack. On a per-frame video
        // stream that is pure added latency, and our frames are already
        // sized deliberately.
        stream
            .set_nodelay(true)
            .context("disabling Nagle on the transport socket")?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;

        let mut sender = Self {
            stream,
            frames_sent: 0,
            bytes_sent: 0,
        };
        let encoded = header.encode();
        sender
            .stream
            .write_all(&encoded)
            .context("sending stream header")?;
        sender.bytes_sent += encoded.len() as u64;
        Ok(sender)
    }

    /// Send one video access unit. Header and payload go out in a single
    /// vectored write so they cannot be split into separate segments.
    ///
    /// Only video frames are ever acked by the device (see the doc comment
    /// on `protocol::ACK_LEN`) — this is the sole path that produces frames
    /// the caller should push onto an ack-matching queue.
    pub fn send_video_frame(&mut self, payload: &[u8], pts_ns: u64, keyframe: bool) -> Result<()> {
        self.send(StreamType::Video, payload, pts_ns, keyframe)
    }

    /// Send one Opus packet. `keyframe` is always encoded as `false` on the
    /// wire for audio — Opus packets are independently decodable, there is
    /// no keyframe concept.
    ///
    /// Audio frames must never be acked by the device and must never be
    /// pushed onto the same ack-matching queue as video sends; see the doc
    /// comment on `protocol::ACK_LEN` for why.
    pub fn send_audio_frame(&mut self, payload: &[u8], pts_ns: u64) -> Result<()> {
        self.send(StreamType::Audio, payload, pts_ns, false)
    }

    /// Shared implementation behind `send_video_frame`/`send_audio_frame`.
    /// Kept private so a call site can never pass `keyframe = true` for an
    /// audio frame -- that possibility doesn't exist in either public
    /// method's signature.
    fn send(
        &mut self,
        stream_type: StreamType,
        payload: &[u8],
        pts_ns: u64,
        keyframe: bool,
    ) -> Result<()> {
        let header = FrameHeader {
            length: payload.len() as u32,
            pts_ns,
            keyframe,
            stream_type,
        }
        .encode();

        let mut slices = [IoSlice::new(&header), IoSlice::new(payload)];
        write_all_vectored(&mut self.stream, &mut slices).context("sending frame")?;

        self.frames_sent += 1;
        self.bytes_sent += (header.len() + payload.len()) as u64;
        Ok(())
    }

    /// A second handle on the same socket for reading device acknowledgements.
    ///
    /// Acks flow device→host while frames flow host→device, so the two
    /// directions can be driven from separate threads without locking.
    pub fn ack_reader(&self) -> Result<TcpStream> {
        let stream = self
            .stream
            .try_clone()
            .context("cloning transport socket for acks")?;
        stream.set_read_timeout(Some(Duration::from_millis(500)))?;
        Ok(stream)
    }

    pub fn frames_sent(&self) -> u64 {
        self.frames_sent
    }

    pub fn bytes_sent(&self) -> u64 {
        self.bytes_sent
    }
}

/// `Write::write_all_vectored` is still unstable, so advance the slices by hand.
fn write_all_vectored(writer: &mut impl Write, slices: &mut [IoSlice<'_>]) -> std::io::Result<()> {
    let mut slices = slices;
    while !slices.is_empty() {
        let written = writer.write_vectored(slices)?;
        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "transport socket accepted no bytes",
            ));
        }
        IoSlice::advance_slices(&mut slices, written);
    }
    Ok(())
}

/// Audio parameters for a `StreamHeader`. `None` at the `stream_header` call
/// site means `AudioCodec::None` — no audio, zero bytes of audio ever hit
/// the wire.
pub struct AudioParams {
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub pre_skip: u16,
}

/// Convenience for the common case. `audio: None` produces a video-only
/// header (`AudioCodec::None`); `Some(params)` fills in the Opus fields.
pub fn stream_header(
    width: u32,
    height: u32,
    framerate: u32,
    audio: Option<AudioParams>,
) -> Result<StreamHeader> {
    let (audio_codec, audio_sample_rate_hz, audio_channels, audio_pre_skip) = match audio {
        None => (AudioCodec::None, 0u16, 0u8, 0u16),
        Some(params) => {
            let sample_rate_hz: u16 = params.sample_rate_hz.try_into().with_context(|| {
                format!(
                    "audio sample rate {} does not fit in u16",
                    params.sample_rate_hz
                )
            })?;
            (
                AudioCodec::Opus,
                sample_rate_hz,
                params.channels,
                params.pre_skip,
            )
        }
    };

    Ok(StreamHeader {
        width: width as u16,
        height: height as u16,
        framerate: framerate as u16,
        codec: Codec::H264,
        audio_codec,
        audio_sample_rate_hz,
        audio_channels,
        audio_pre_skip,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_header_without_audio_is_none_codec() {
        let header = stream_header(1920, 1200, 60, None).unwrap();
        assert_eq!(header.audio_codec, AudioCodec::None);
    }

    #[test]
    fn stream_header_with_audio_fills_opus_fields() {
        let header = stream_header(
            1920,
            1200,
            60,
            Some(AudioParams {
                sample_rate_hz: 48_000,
                channels: 2,
                pre_skip: 312,
            }),
        )
        .unwrap();
        assert_eq!(header.audio_codec, AudioCodec::Opus);
        assert_eq!(header.audio_sample_rate_hz, 48_000);
        assert_eq!(header.audio_channels, 2);
        assert_eq!(header.audio_pre_skip, 312);
    }

    #[test]
    fn stream_header_rejects_sample_rate_that_does_not_fit_u16() {
        let result = stream_header(
            1920,
            1200,
            60,
            Some(AudioParams {
                sample_rate_hz: 100_000, // > u16::MAX, must error, not truncate
                channels: 2,
                pre_skip: 0,
            }),
        );
        assert!(result.is_err());
    }
}
