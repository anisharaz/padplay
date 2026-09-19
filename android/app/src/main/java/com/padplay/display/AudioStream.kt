// SPDX-License-Identifier: Apache-2.0

package com.padplay.display

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaFormat
import android.util.Log
import java.io.IOException
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * Demuxed-audio consumer, called from [VideoStream]'s single read loop — the
 * one and only place frames come off the wire. Kept to exactly the calls the
 * demuxer needs to make: parse the header once per session, hand off each
 * frame's payload, and know when the session is over. [VideoStream] talks to
 * whatever implements this and nothing more, so the demux loop never needs to
 * know [AudioStream] exists as a concrete type.
 */
interface AudioSink {
    /**
     * The stream header for a newly-accepted connection, read once before
     * the frame loop starts. Implementations should (re)configure for this
     * session, or do nothing if `header.hasAudio` is false.
     */
    fun onStreamHeader(header: Protocol.StreamHeader)

    /** One demuxed audio frame's payload, straight off the wire, `streamType == STREAM_TYPE_AUDIO`. */
    fun onAudioFrame(payload: ByteArray, length: Int, ptsNs: Long)

    /** The session (socket) this header/these frames belonged to has ended. */
    fun onSessionEnded()
}

/**
 * Decodes the Opus audio track demuxed out of [VideoStream]'s frame stream
 * and plays it out the tablet's speaker via [AudioTrack].
 *
 * Structured as closely as reasonable to [VideoStream]: reuses its [Phase]
 * state machine (the wording is already codec-agnostic), is created once and
 * outlives individual host connections, and is started/stopped by
 * `DisplayActivity`'s accept switch in lockstep with the video stream.
 *
 * Drives [MediaCodec] with its **synchronous** `dequeue`/`queue` API rather
 * than the async callback API — not the more idiomatic modern choice, but a
 * deliberate one: on the reference tablet, the async path (`setCallback`)
 * left this device's vendor Codec2 stack (`c2.android.opus.decoder` via
 * MediaTek's Codec2 store) logging `Codec2-ComponentInterface: We have a
 * failed config` at `start()` and never processing a single input buffer
 * for the entire session (`Qin:0` in the codec's own end-of-session stats),
 * even with byte-verified-correct CSD. The sync API worked where async did
 * not — see `docs/07-audio-proposal.md`'s "Status" section for the full
 * investigation. Two threads drive it, mirroring the same "never block the
 * thread video also depends on" discipline the async version already had:
 * [onAudioFrame] (called from `VideoStream`'s single shared demux thread)
 * only ever does a zero-timeout `dequeueInputBuffer` and drops the frame if
 * none is free, never blocking; a dedicated [outputDrainThread] owns the
 * blocking `dequeueOutputBuffer` loop, and a further dedicated writer thread
 * owns the blocking [AudioTrack.write] call, mirroring [VideoStream]'s
 * `ackThread` pattern for "the demux thread must return fast, the slow
 * blocking operation happens elsewhere."
 *
 * Unlike [VideoStream], this class does not own a socket or an accept loop —
 * [VideoStream]'s read loop is the only demuxer, and drives this class
 * through the narrow [AudioSink] interface: one call per session header, one
 * call per audio frame, one call at session end.
 */
class AudioStream : AudioSink {

    private companion object {
        const val TAG = "PadPlay"

        // getMinBufferSize() is the minimum that avoids immediate underrun
        // under ideal scheduling; real device scheduling jitter is not
        // ideal. 2-4x favors underrun safety (an audible click) over
        // shaving a few ms of latency this app doesn't need to chase.
        const val BUFFER_SIZE_MULTIPLIER = 3

        // Opus CSD synthesis (ExoPlayer's OpusUtil follows the same shape).
        const val OPUS_HEAD_LEN = 19
        const val CSD_INT64_LEN = 8

        // Standard libopus seek pre-roll: 80ms, expressed in nanoseconds.
        // Not wired from the wire header — see docs/07-audio-proposal.md's
        // "Opus CSD" section for why this one stays a derived constant.
        const val SEEK_PREROLL_NS = 80_000_000L

        // How long the output-drain thread blocks per dequeueOutputBuffer
        // call before checking `running` again -- this thread is dedicated
        // and blocking it is fine, unlike the input side on the shared
        // demux thread.
        const val OUTPUT_DEQUEUE_TIMEOUT_US = 20_000L
    }

    @Volatile private var running = false
    @Volatile private var codec: MediaCodec? = null
    @Volatile private var track: AudioTrack? = null
    private var writerThread: Thread? = null
    private var outputDrainThread: Thread? = null
    private var channels = 1

    private val pendingPcm = LinkedBlockingQueue<ByteArray>()

    @Volatile var samplesPlayed: Long = 0; private set
    @Volatile var lastError: String? = null; private set
    @Volatile var phase: Phase = Phase.Waiting; private set
    @Volatile var lastAudioAtMs: Long = 0L; private set

    /** [AudioTrack.getUnderrunCount] — the single most meaningful audio
     * liveness/quality signal, the way frame staleness is for video. */
    val underrunCount: Int get() = track?.underrunCount ?: 0

    /** Whether [start] has been called without a matching [stop] since. */
    val isRunning: Boolean get() = running

    fun start() {
        if (running) return
        running = true
        phase = Phase.Waiting
    }

    fun stop() {
        running = false
        teardown()
        phase = Phase.Disconnected(null)
    }

    override fun onStreamHeader(header: Protocol.StreamHeader) {
        teardown()
        if (!running || !header.hasAudio) {
            phase = Phase.Waiting
            return
        }
        phase = Phase.Connecting
        try {
            configure(header)
            phase = Phase.Streaming
        } catch (e: Exception) {
            lastError = e.message
            phase = Phase.Disconnected(e.message)
            Log.w(TAG, "audio configure failed: ${e.message}")
            teardown()
        }
    }

    override fun onAudioFrame(payload: ByteArray, length: Int, ptsNs: Long) {
        val codec = this.codec ?: return
        // Zero-timeout, non-blocking: this runs on VideoStream's single
        // shared demux thread, which also has to keep reading video frames
        // off the same socket. Any wait here, however short, means a
        // stalled or slow-draining decoder stalls video reads for that
        // long too -- at Opus's ~20ms packet cadence a decoder that's
        // fallen behind turns into the demux thread spending nearly all its
        // time blocked on audio, backing video up in the socket's receive
        // buffer for seconds (measured: median round trip went from ~30ms
        // to ~9000ms before this was a zero-timeout call). A dropped audio
        // frame is inaudible; a demux thread stuck for hundreds of ms is
        // not.
        val index = runCatching { codec.dequeueInputBuffer(0) }.getOrDefault(-1)
        if (index < 0) {
            Log.w(TAG, "audio decoder starved of input buffers; dropping frame")
            return
        }
        val inputBuffer = codec.getInputBuffer(index) ?: return
        inputBuffer.clear()
        inputBuffer.put(payload, 0, length)
        // MediaCodec timestamps are microseconds; the wire uses nanoseconds.
        runCatching { codec.queueInputBuffer(index, 0, length, ptsNs / 1000, 0) }
            .onFailure { lastError = "audio queueInputBuffer: ${it.message}" }
    }

    override fun onSessionEnded() {
        teardown()
        phase = Phase.Waiting
    }

    private fun configure(header: Protocol.StreamHeader) {
        val sampleRate = header.audioSampleRateHz
        channels = header.audioChannels

        val format = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, sampleRate, channels)
        format.setByteBuffer("csd-0", ByteBuffer.wrap(opusHead(sampleRate, channels, header.audioPreSkip)))
        format.setByteBuffer("csd-1", ByteBuffer.wrap(leInt64(codecDelayNs(header.audioPreSkip, sampleRate))))
        format.setByteBuffer("csd-2", ByteBuffer.wrap(leInt64(SEEK_PREROLL_NS)))

        // Synchronous API deliberately — see the class doc comment for why
        // (async left this device's decoder permanently stuck at 0
        // processed packets).
        val codec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS)
        codec.configure(format, null, null, 0)
        codec.start()
        this.codec = codec
        Log.i(TAG, "audio decoder started: ${codec.name} ${sampleRate}Hz/${channels}ch")

        val track = buildAudioTrack(sampleRate, channels)
        track.play()
        this.track = track

        pendingPcm.clear()
        startOutputDrainThread(codec)
        startWriterThread(track)
    }

    private fun buildAudioTrack(sampleRate: Int, channels: Int): AudioTrack {
        val channelMask = if (channels >= 2) AudioFormat.CHANNEL_OUT_STEREO else AudioFormat.CHANNEL_OUT_MONO
        val minBuf = AudioTrack.getMinBufferSize(sampleRate, channelMask, AudioFormat.ENCODING_PCM_16BIT)
        if (minBuf <= 0) {
            throw IOException("AudioTrack.getMinBufferSize returned $minBuf for ${sampleRate}Hz/${channels}ch")
        }

        return AudioTrack.Builder()
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA)
                    .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                    .build()
            )
            .setAudioFormat(
                AudioFormat.Builder()
                    .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                    .setSampleRate(sampleRate)
                    .setChannelMask(channelMask)
                    .build()
            )
            .setBufferSizeInBytes(minBuf * BUFFER_SIZE_MULTIPLIER)
            .setTransferMode(AudioTrack.MODE_STREAM)
            // Silently downgrades on hardware that doesn't support it; safe
            // to always request regardless of device.
            .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
            .build()
    }

    /**
     * Owns the blocking `dequeueOutputBuffer` loop — the sync-API
     * equivalent of what `onOutputBufferAvailable` did in the async
     * version. Dedicated to this one job, so blocking here (unlike on the
     * shared demux thread `onAudioFrame` runs on) is fine.
     */
    private fun startOutputDrainThread(codec: MediaCodec) {
        outputDrainThread = Thread({
            val info = MediaCodec.BufferInfo()
            try {
                while (running && !Thread.currentThread().isInterrupted) {
                    val index = codec.dequeueOutputBuffer(info, OUTPUT_DEQUEUE_TIMEOUT_US)
                    when {
                        index >= 0 -> {
                            // Copy out and release immediately — there's no
                            // "render" concept for audio, so this is always
                            // false, and the copy must happen before
                            // release() invalidates the buffer.
                            val buffer = codec.getOutputBuffer(index)
                            if (buffer != null && info.size > 0) {
                                val pcm = ByteArray(info.size)
                                buffer.get(pcm)
                                pendingPcm.offer(pcm)
                            }
                            codec.releaseOutputBuffer(index, false)
                        }
                        index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED ->
                            Log.i(TAG, "audio output format: ${codec.outputFormat}")
                        // INFO_TRY_AGAIN_LATER and the deprecated
                        // INFO_OUTPUT_BUFFERS_CHANGED both need no action --
                        // just loop and check `running` again.
                    }
                }
            } catch (e: Exception) {
                lastError = "audio decoder: ${e.message}"
                Log.w(TAG, "audio output drain stopped: ${e.message}")
            }
        }, "padplay-audio-drain").apply { start() }
    }

    /**
     * Owns the blocking [AudioTrack.write] call so the output-drain thread
     * above never does — the same reason [VideoStream] has a dedicated
     * `ackThread` rather than writing acks from inside its read loop.
     */
    private fun startWriterThread(track: AudioTrack) {
        writerThread = Thread({
            try {
                while (running && !Thread.currentThread().isInterrupted) {
                    val pcm = pendingPcm.poll(500, TimeUnit.MILLISECONDS) ?: continue
                    var written = 0
                    while (written < pcm.size) {
                        val n = track.write(pcm, written, pcm.size - written)
                        if (n < 0) throw IOException("AudioTrack.write failed: $n")
                        written += n
                    }
                    samplesPlayed += pcm.size / (2 * channels)
                    lastAudioAtMs = System.currentTimeMillis()
                }
            } catch (e: Exception) {
                lastError = "audio writer: ${e.message}"
                Log.w(TAG, "audio writer stopped: ${e.message}")
            }
        }, "padplay-audio-writer").apply { start() }
    }

    private fun teardown() {
        writerThread?.interrupt()
        writerThread = null
        outputDrainThread?.interrupt()
        outputDrainThread = null
        runCatching { codec?.stop() }
        runCatching { codec?.release() }
        codec = null
        runCatching { track?.stop() }
        runCatching { track?.release() }
        track = null
        pendingPcm.clear()
    }

    /**
     * Synthesizes the Opus Identification Header ("OpusHead") `MediaCodec`
     * expects as `csd-0` — 19 bytes for mono/stereo, channel mapping family
     * 0. Opus's own internal fields are little-endian, unlike the wire
     * header this app reads them off of.
     *
     * Layout: `"OpusHead"` (8) + version=1 (1) + channel count (1) +
     * pre-skip u16 LE (2) + input sample rate u32 LE (4) + output gain=0
     * i16 LE (2) + channel mapping family=0 (1) = 19 bytes.
     */
    private fun opusHead(sampleRate: Int, channels: Int, preSkip: Int): ByteArray {
        val buf = ByteBuffer.allocate(OPUS_HEAD_LEN).order(ByteOrder.LITTLE_ENDIAN)
        buf.put("OpusHead".toByteArray(Charsets.US_ASCII))
        buf.put(1) // version
        buf.put(channels.toByte())
        buf.putShort(preSkip.toShort())
        buf.putInt(sampleRate)
        buf.putShort(0) // output gain
        buf.put(0) // channel mapping family
        return buf.array()
    }

    /** `csd-1`: codec delay in nanoseconds, little-endian int64. */
    private fun codecDelayNs(preSkip: Int, sampleRate: Int): Long =
        preSkip.toLong() * 1_000_000_000L / sampleRate.toLong()

    private fun leInt64(value: Long): ByteArray =
        ByteBuffer.allocate(CSD_INT64_LEN).order(ByteOrder.LITTLE_ENDIAN).putLong(value).array()
}
