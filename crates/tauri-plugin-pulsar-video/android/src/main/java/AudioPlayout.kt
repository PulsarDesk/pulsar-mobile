package dev.pulsar.video

import android.content.Context
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTrack
import android.os.Build
import android.os.SystemClock
import android.util.Log

/**
 * Client audio playout: decoded 48 kHz stereo PCM → an AudioTrack, with the desktop client's
 * queue steering (pulsar-desktop `src-tauri/src/native_audio.rs`, `SdlOut`).
 *
 * The AudioTrack's queue (frames written − frames played) is a sawtooth: +one packet per push,
 * −one mixer burst per device pull. Its LOW point is held near a margin (15 ms) by nudging the
 * playback rate (slowly, at most ±2 %). Playback starts once margin + one device burst is
 * queued; a queue that runs dry (late packets: Wi-Fi jitter) raises the margin by 10 ms, up to
 * 120 ms, and it eases back after 20 s without one. A queue far past the target (a burst after a
 * stall) is flushed and re-buffered, so latency stays bounded.
 *
 * The rate is changed by resampling the PCM here (linear interpolation), not with
 * `AudioTrack.setPlaybackRate`: a low-latency (FAST mixer) track rejects sample-rate changes.
 * The track is asked for the low-latency path and is given a large capacity (it only bounds
 * the queue; the steering decides how much of it is used), so a non-blocking write never
 * truncates a packet the way the old fixed ~100 ms buffer did.
 *
 * Not thread-safe: the plugin calls it under its audio lock.
 */
internal class AudioPlayout private constructor(
    private val track: AudioTrack,
    /** The mixer's pull size (ms): what the queue must hold at every pull. */
    private val deviceMs: Float,
    private val capacityFrames: Int,
) {
    companion object {
        private const val TAG = "PulsarAudio"
        private const val RATE = 48_000
        private const val FRAMES_PER_MS = RATE / 1000f
        /** Margin the queue's low point is held at, before any dry queue raised it. */
        private const val BASE_TARGET_MS = 15f
        /** Largest playback-rate correction (±2 %). Real clock drift stays well under 0.1 %. */
        private const val MAX_RATE_ADJ = 0.02f
        /** Queue error (ms) inside which the rate stays exactly 1.0. */
        private const val DEADBAND_MS = 3f
        /** Rate change per ms of smoothed queue error past the deadband: 10 ms over → +0.5 %. */
        private const val GAIN_PER_MS = 0.0005f
        /** Target growth on a dry queue, its ceiling, and how long without one before it eases. */
        private const val DRY_STEP_MS = 10f
        private const val MAX_TARGET_MS = 120f
        private const val EASE_AFTER_MS = 20_000L
        /** Queue past target + device burst + this is flushed and re-buffered. */
        private const val FLUSH_OVER_MS = 150f
        /** Pushes per steering update (≈ 0.25 s at 10 ms packets). */
        private const val STEER_EVERY = 25
        /** AudioTrack capacity: room for the largest target + the flush threshold + a burst. */
        private const val CAPACITY_MS = 400
        private const val STATS_EVERY_MS = 5_000L

        /** Open a 48 kHz stereo 16-bit track on the low-latency path where the device has one. */
        fun open(ctx: Context, gain: Float): AudioPlayout {
            val minBuf = AudioTrack.getMinBufferSize(RATE, AudioFormat.CHANNEL_OUT_STEREO, AudioFormat.ENCODING_PCM_16BIT)
            val capBytes = maxOf(minBuf, CAPACITY_MS * RATE / 1000 * 4)
            val track = AudioTrack.Builder()
                .setAudioAttributes(
                    AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_MEDIA)
                        .setContentType(AudioAttributes.CONTENT_TYPE_MOVIE).build()
                )
                .setAudioFormat(
                    AudioFormat.Builder().setSampleRate(RATE)
                        .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                        .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO).build()
                )
                .setBufferSizeInBytes(capBytes)
                .setTransferMode(AudioTrack.MODE_STREAM)
                .apply {
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                        setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
                    }
                }
                .build()
            var capFrames = capBytes / 4
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
                // A low-latency track starts with a small usable size; open it up to the whole
                // capacity (the steering, not this size, sets the latency).
                try {
                    capFrames = track.setBufferSizeInFrames(track.bufferCapacityInFrames).takeIf { it > 0 } ?: capFrames
                } catch (_: Exception) {}
            }
            val fast = Build.VERSION.SDK_INT >= Build.VERSION_CODES.O &&
                track.performanceMode == AudioTrack.PERFORMANCE_MODE_LOW_LATENCY
            val am = ctx.getSystemService(Context.AUDIO_SERVICE) as? AudioManager
            val burst = am?.getProperty(AudioManager.PROPERTY_OUTPUT_FRAMES_PER_BUFFER)?.toIntOrNull() ?: 0
            val nativeRate = am?.getProperty(AudioManager.PROPERTY_OUTPUT_SAMPLE_RATE)?.toIntOrNull() ?: RATE
            val burstMs = if (burst > 0 && nativeRate > 0) burst * 1000f / nativeRate else 10f
            // The FAST mixer pulls one HAL burst at a time; the normal mixer ~20 ms.
            val deviceMs = if (fast) burstMs.coerceIn(1f, 20f) else maxOf(burstMs, 20f)
            track.setVolume(gain)
            Log.i(
                TAG,
                "audio player up: fast=$fast device=${"%.1f".format(deviceMs)}ms burst=$burst@$nativeRate " +
                    "capacity=${capFrames * 1000 / RATE}ms target=${BASE_TARGET_MS.toInt()}ms"
            )
            // Opens paused: playback starts once the prebuffer is in (push).
            return AudioPlayout(track, deviceMs, capFrames)
        }
    }

    private var targetMs = BASE_TARGET_MS
    /** Smoothed low point of the queue (ms), the steering input. */
    private var levelMs: Float? = null
    /** Lowest queue level seen since the last steering update. */
    private var winMinMs = Float.MAX_VALUE
    private var winPushes = 0
    private var ratio = 1f
    private var playing = false
    private var lastDryMs = SystemClock.elapsedRealtime()
    /** Frames written since the last flush, and the head position the flush left. */
    private var written = 0L
    private var headBase = 0L
    private var underruns = underrunCount()

    // Resampler state: the last input frame of the previous packet and the read position past it.
    private var phase = 0.0
    private var prevL = 0
    private var prevR = 0
    private var out = ShortArray(0)

    // Stats window (logged every STATS_EVERY_MS).
    private var stPackets = 0
    private var stFrames = 0L
    private var stDry = 0
    private var stFlushes = 0
    private var stQMin = Float.MAX_VALUE
    private var stQMax = 0f
    private var stRatioMin = Float.MAX_VALUE
    private var stRatioMax = 0f
    private var stRaw = false
    private var statsAt = SystemClock.elapsedRealtime()

    fun setVolume(gain: Float) {
        try { track.setVolume(gain) } catch (_: Exception) {}
    }

    fun release() {
        try { track.pause() } catch (_: Exception) {}
        try { track.flush() } catch (_: Exception) {}
        try { track.release() } catch (_: Exception) {}
    }

    private fun underrunCount(): Int =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
            try { track.underrunCount } catch (_: Exception) { 0 }
        } else 0

    /** Frames queued in the track (written, not yet played), in ms. */
    private fun queuedMs(): Float {
        val head = (track.playbackHeadPosition.toLong() and 0xFFFFFFFFL) - headBase
        val q = written - (head and 0xFFFFFFFFL)
        // The head can never pass what was written; a nonsense value (a device that did not
        // reset the head on flush) reads as empty.
        if (q < 0 || q > capacityFrames * 2L) return 0f
        return q / FRAMES_PER_MS
    }

    private fun flush() {
        try { track.pause() } catch (_: Exception) {}
        try { track.flush() } catch (_: Exception) {}
        playing = false
        written = 0
        headBase = track.playbackHeadPosition.toLong() and 0xFFFFFFFFL
        phase = 0.0; prevL = 0; prevR = 0
        ratio = 1f
        levelMs = null
        underruns = underrunCount()
    }

    /**
     * Queue [frames] frames of interleaved stereo PCM from [pcm] and steer the queue toward the
     * target. [raw]: the packet was uncompressed L16 (stats only).
     */
    fun push(pcm: ShortArray, frames: Int, raw: Boolean) {
        if (frames <= 0) return
        val now = SystemClock.elapsedRealtime()
        val before = queuedMs()
        val ur = underrunCount()
        if (playing && (before <= 0f || ur > underruns)) {
            // Ran dry: the mixer has been playing silence — a crackle. Buffer more.
            stDry++
            targetMs = minOf(targetMs + DRY_STEP_MS, MAX_TARGET_MS)
            lastDryMs = now
        } else if (targetMs > BASE_TARGET_MS && now - lastDryMs > EASE_AFTER_MS) {
            targetMs = maxOf(targetMs - 5f, BASE_TARGET_MS)
            lastDryMs = now
        }
        underruns = ur
        val n = resample(pcm, frames)
        val wrote = track.write(out, 0, n * 2, AudioTrack.WRITE_NON_BLOCKING)
        if (wrote > 0) written += wrote / 2
        stPackets++
        stFrames += frames
        if (raw) stRaw = true
        val after = queuedMs()
        // What must not hit zero is the sawtooth's LOW point, so that is what gets steered.
        val full = targetMs + deviceMs
        if (!playing) {
            if (after >= full) {
                levelMs = null
                winMinMs = Float.MAX_VALUE
                winPushes = 0
                try { track.play(); playing = true } catch (e: Exception) { Log.w(TAG, "play failed", e) }
            }
        } else if (after > full + FLUSH_OVER_MS) {
            // Far past the target: drop it and re-buffer rather than play all of it late.
            flush()
            stFlushes++
        } else {
            winMinMs = minOf(winMinMs, before)
            winPushes++
            if (winPushes >= STEER_EVERY) {
                val low = winMinMs
                val level = levelMs?.let { it + 0.3f * (low - it) } ?: low
                levelMs = level
                winMinMs = Float.MAX_VALUE
                winPushes = 0
                val err = level - targetMs
                val adj = if (Math.abs(err) <= DEADBAND_MS) 0f
                else ((err - Math.copySign(DEADBAND_MS, err)) * GAIN_PER_MS).coerceIn(-MAX_RATE_ADJ, MAX_RATE_ADJ)
                ratio = 1f + adj
            }
        }
        stQMin = minOf(stQMin, before)
        stQMax = maxOf(stQMax, after)
        stRatioMin = minOf(stRatioMin, ratio)
        stRatioMax = maxOf(stRatioMax, ratio)
        val span = now - statsAt
        if (span >= STATS_EVERY_MS) {
            Log.i(
                TAG,
                "audio: player stats packets=$stPackets in_hz=${stFrames * 1000 / span} dry=$stDry " +
                    "flushes=$stFlushes codec=${if (stRaw) "l16" else "opus"} " +
                    "q_ms=${"%.0f".format(if (stQMin == Float.MAX_VALUE) 0f else stQMin)}-${"%.0f".format(stQMax)} " +
                    "target_ms=${targetMs.toInt()} ratio=${"%.4f".format(stRatioMin)}-${"%.4f".format(stRatioMax)}"
            )
            stPackets = 0; stFrames = 0; stDry = 0; stFlushes = 0
            stQMin = Float.MAX_VALUE; stQMax = 0f; stRatioMin = Float.MAX_VALUE; stRatioMax = 0f
            stRaw = false
            statsAt = now
        }
    }

    /**
     * Linear-interpolation resample of [frames] stereo frames from [inp] into [out] at the
     * current [ratio] (input frames consumed per output frame: > 1 plays faster, the queue
     * shrinks). Continuous across packets: the previous packet's last frame and the fractional
     * read position carry over. Returns the output frame count.
     */
    private fun resample(inp: ShortArray, frames: Int): Int {
        val step = ratio.toDouble()
        val maxOut = (frames / (1.0 - MAX_RATE_ADJ)).toInt() + 4
        if (out.size < maxOut * 2) out = ShortArray(maxOut * 2)
        var n = 0
        var pos = phase
        // Virtual input: index 0 = the previous packet's last frame, index j = inp frame j-1.
        while (pos < frames && n < maxOut) {
            val i = pos.toInt()
            val f = pos - i
            val l0 = if (i == 0) prevL else inp[(i - 1) * 2].toInt()
            val r0 = if (i == 0) prevR else inp[(i - 1) * 2 + 1].toInt()
            val l1 = inp[i * 2].toInt()
            val r1 = inp[i * 2 + 1].toInt()
            out[n * 2] = (l0 + (l1 - l0) * f).toInt().toShort()
            out[n * 2 + 1] = (r0 + (r1 - r0) * f).toInt().toShort()
            n++
            pos += step
        }
        phase = (pos - frames).coerceAtLeast(0.0)
        prevL = inp[(frames - 1) * 2].toInt()
        prevR = inp[(frames - 1) * 2 + 1].toInt()
        return n
    }
}
