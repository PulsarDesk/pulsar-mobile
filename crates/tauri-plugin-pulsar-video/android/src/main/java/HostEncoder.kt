package dev.pulsar.video

import android.annotation.SuppressLint
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.media.projection.MediaProjection
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.Surface
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress

/**
 * Mobile HOST (task #16, Stage 2): mirror the screen via MediaProjection into a
 * MediaCodec ENCODER, packetize the encoded H.264/H.265 to RTP, and send it to a
 * loopback UDP port the Rust side forwards into the encrypted session.
 *
 * W5-native addition: companion object [probeCodecs] enumerates available hardware
 * video encoders for the `host_codecs` probe (W5-host-codecprobe).
 */
class HostEncoder(
    private val projection: MediaProjection,
    private val mime: String,
    private val width: Int,
    private val height: Int,
    private val fps: Int,
    private val bitrate: Int,
    private val densityDpi: Int,
    private val port: Int,
    private val audioPort: Int,
) {
    private val TAG = "PulsarHost"
    private var encoder: MediaCodec? = null
    private var inputSurface: Surface? = null
    private var vdisplay: VirtualDisplay? = null
    private var thread: Thread? = null
    private var audioRecord: AudioRecord? = null
    private var audioEncoder: MediaCodec? = null
    private var audioThread: Thread? = null
    @Volatile private var running = false
    // RTP video sequence number — a FIELD (not a drainLoop local) so it stays continuous
    // across a reconfigure() (rotation), otherwise it would reset to 1 and the client's
    // jitter buffer would treat the whole stream as stale/reordered.
    private var seq = 1
    private var curW = width
    private var curH = height

    private fun videoFormat(w: Int, h: Int): MediaFormat =
        MediaFormat.createVideoFormat(mime, w, h).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, bitrate)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)
            // GOP 5 s (was 1 s). A per-second IDR is a ~50-150-packet UDP burst every second —
            // the main packet-loss trigger on Wi-Fi (AP queue overflow) and thus the main
            // corruption source. Loss repair is NACK retransmit; an unrepairable hole makes
            // the client send MediaNack([0]) → requestSyncFrame() → immediate IDR. So the
            // scheduled keyframe cadence can be long, like Sunshine/Moonlight.
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 5)
            // Repeat SPS/PPS on EVERY keyframe (every 1s). The desktop's ffmpeg/pulsar-render
            // demuxer can't decode ("unspecified size") until it sees the parameter sets; if its
            // renderer starts late or restarts on a re-stream it misses the one-shot SPS and
            // freezes. Prepending headers to sync frames makes the stream self-describing.
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                setInteger(MediaFormat.KEY_PREPEND_HEADER_TO_SYNC_FRAMES, 1)
            }
            // A surface-input encoder only emits a frame when the captured screen CHANGES.
            // A static screen would otherwise produce nothing after the first keyframe → the
            // remote sees a frozen image. Repeat the last frame every ~frame-interval so the
            // stream keeps flowing (repeated static frames compress to almost nothing).
            setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, 1_000_000L / fps)
        }

    fun start() {
        val codec = MediaCodec.createEncoderByType(mime)
        codec.configure(videoFormat(width, height), null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        val surface = codec.createInputSurface()
        codec.start()
        encoder = codec
        inputSurface = surface

        // API 34+ requires a registered callback before createVirtualDisplay.
        projection.registerCallback(object : MediaProjection.Callback() {}, Handler(Looper.getMainLooper()))
        // FLAG_PUBLIC (the standard MediaProjection screen-capture flag) keeps capturing
        // when our app is backgrounded; AUTO_MIRROR couples to the source display state and
        // pauses on the emulator when the app leaves the foreground → remote freeze.
        vdisplay = projection.createVirtualDisplay(
            "pulsar-host", width, height, densityDpi,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_PUBLIC, surface, null, null
        )

        running = true
        thread = Thread { drainLoop() }.also { it.isDaemon = true; it.start() }
        Log.i(TAG, "host encoder: $mime ${width}x${height}@$fps ${bitrate}bps -> 127.0.0.1:$port")

        if (audioPort > 0) startAudio()
    }

    /**
     * Force the next encoded frame to be an IDR — the client asks for this
     * (MediaNack([0]) sentinel) when a lost packet couldn't be repaired by retransmit,
     * so the picture recovers immediately instead of smearing until the scheduled
     * keyframe (5 s GOP).
     */
    fun requestSyncFrame() {
        val enc = encoder ?: return
        try {
            val b = android.os.Bundle()
            b.putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0)
            enc.setParameters(b)
        } catch (_: Exception) {}
    }

    /**
     * Live bitrate change (MediaCodec dynamic parameter) — applied when the desktop's
     * adaptive-bitrate loop re-requests the stream at a lower/higher rate. No encoder
     * restart, no keyframe glitch; takes effect within a few frames.
     */
    fun setBitrate(bps: Int) {
        val enc = encoder ?: return
        if (bps <= 0) return
        try {
            val b = android.os.Bundle()
            b.putInt(MediaCodec.PARAMETER_KEY_VIDEO_BITRATE, bps)
            enc.setParameters(b)
            Log.i(TAG, "live bitrate → ${bps / 1000}k")
        } catch (e: Exception) {
            Log.w(TAG, "setBitrate failed", e)
        }
    }

    /**
     * Rotation: re-encode at new dims WITHOUT a second createVirtualDisplay (Android 14+
     * throws SecurityException if createVirtualDisplay is called twice on one MediaProjection).
     * Instead build a fresh encoder at the new size and REPOINT the existing VirtualDisplay at
     * it (resize + setSurface). The new SPS (self-describing, headers-on-keyframes) carries the
     * new size so the client's decoder reconfigures + re-aspects. RTP seq stays continuous.
     */
    fun reconfigure(newW: Int, newH: Int) {
        val vd = vdisplay ?: return
        if (newW <= 0 || newH <= 0 || (newW == curW && newH == curH)) return
        try {
            // 1) New encoder + surface at the new size (started before we repoint the VD).
            val codec = MediaCodec.createEncoderByType(mime)
            codec.configure(videoFormat(newW, newH), null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            val surface = codec.createInputSurface()
            codec.start()
            // 2) Repoint the SAME virtual display at the new surface + size.
            vd.resize(newW, newH, densityDpi)
            vd.setSurface(surface)
            // 3) Stop the old drain loop, then release the old encoder/surface.
            running = false
            thread?.interrupt()
            try { thread?.join(300) } catch (_: Exception) {}
            try { encoder?.stop() } catch (_: Exception) {}
            try { encoder?.release() } catch (_: Exception) {}
            try { inputSurface?.release() } catch (_: Exception) {}
            // 4) Swap in the new encoder + restart draining (seq is a field → continuous).
            encoder = codec
            inputSurface = surface
            curW = newW; curH = newH
            running = true
            thread = Thread { drainLoop() }.also { it.isDaemon = true; it.start() }
            Log.i(TAG, "reconfigure ${newW}x${newH}")
        } catch (e: Exception) {
            Log.e(TAG, "reconfigure failed", e)
        }
    }

    /** Capture the device's playback audio (MediaProjection), Opus-encode, RTP → audio port. */
    @SuppressLint("MissingPermission") // playback capture is gated by the projection, not RECORD_AUDIO
    private fun startAudio() {
        try {
            val config = AudioPlaybackCaptureConfiguration.Builder(projection)
                .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
                .addMatchingUsage(AudioAttributes.USAGE_GAME)
                .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
                .build()
            val afmt = AudioFormat.Builder()
                .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                .setSampleRate(48000)
                .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
                .build()
            val minBuf = AudioRecord.getMinBufferSize(48000, AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_16BIT)
            val rec = AudioRecord.Builder()
                .setAudioFormat(afmt)
                .setBufferSizeInBytes(maxOf(minBuf, 8192))
                .setAudioPlaybackCaptureConfig(config)
                .build()
            rec.startRecording()
            audioRecord = rec

            val efmt = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, 48000, 2).apply {
                setInteger(MediaFormat.KEY_BIT_RATE, 96_000)
            }
            val enc = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS)
            enc.configure(efmt, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            enc.start()
            audioEncoder = enc

            audioThread = Thread { audioLoop(rec, enc) }.also { it.isDaemon = true; it.start() }
            Log.i(TAG, "host audio: opus 48000/2 -> 127.0.0.1:$audioPort")
        } catch (e: Exception) {
            Log.e(TAG, "audio start failed (no playback-capture / opus encoder)", e)
        }
    }

    private fun audioLoop(rec: AudioRecord, enc: MediaCodec) {
        val sock = DatagramSocket()
        val addr = InetAddress.getByName("127.0.0.1")
        val info = MediaCodec.BufferInfo()
        var aseq = 1
        val assrc = byteArrayOf(0x50, 0x55, 0x4C, 0x41) // "PULA"
        try {
            while (running) {
                val inIdx = enc.dequeueInputBuffer(10_000)
                if (inIdx >= 0) {
                    val ib = enc.getInputBuffer(inIdx)
                    if (ib != null) {
                        ib.clear()
                        val n = rec.read(ib, ib.capacity())
                        enc.queueInputBuffer(inIdx, 0, if (n > 0) n else 0, System.nanoTime() / 1000, 0)
                    }
                }
                var oi = enc.dequeueOutputBuffer(info, 0)
                while (oi >= 0) {
                    val ob = enc.getOutputBuffer(oi)
                    if (ob != null && info.size > 0 && (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG) == 0) {
                        val pkt = ByteArray(info.size); ob.position(info.offset); ob.get(pkt)
                        val ts = ((info.presentationTimeUs * 48) / 1000).toInt() // 48 kHz Opus clock
                        val rtpPkt = rtp(aseq, ts, true, assrc, pkt, 97)
                        try { sock.send(DatagramPacket(rtpPkt, rtpPkt.size, addr, audioPort)) } catch (_: Exception) {}
                        aseq++
                    }
                    enc.releaseOutputBuffer(oi, false)
                    oi = enc.dequeueOutputBuffer(info, 0)
                }
            }
        } catch (e: Exception) {
            Log.e(TAG, "audio loop error", e)
        } finally {
            try { sock.close() } catch (_: Exception) {}
        }
    }

    private fun drainLoop() {
        // Captured once per invocation. reconfigure() stops + joins this loop BEFORE swapping
        // `encoder`, so the captured ref stays valid for this loop's lifetime.
        val codec = encoder ?: return
        val sock = DatagramSocket()
        val addr = InetAddress.getByName("127.0.0.1")
        val info = MediaCodec.BufferInfo()
        val ssrc = byteArrayOf(0x50, 0x55, 0x4C, 0x53) // "PULS"
        // Static-screen watchdog. A fully static screen starves the encoder: AOSP's
        // GraphicBufferSource caps KEY_REPEAT_PREVIOUS_FRAME_AFTER at ~10 repeats
        // (kRepeatLastFrameCount) and some vendor encoders (Exynos/MTK) frame-skip
        // bit-identical input to zero output — so packets stop, and the client shows its
        // "stream stopped" stall UI on a perfectly healthy session (and a freshly-added
        // cursor overlay never becomes visible, since visibility rides on fresh frames).
        // Recovery, escalating once per second of silence:
        //   1) request a sync frame — fixes the vendor frame-skip case (input flows,
        //      output suppressed); the next encoded frame is a fresh IDR.
        //   2) if STILL silent (producer fully starved — repeat cap hit), re-point the
        //      VirtualDisplay at its own surface: setSurface(null) + setSurface(surface)
        //      forces SurfaceFlinger to recompose into the surface → a real input frame
        //      arrives → with the pending sync request it encodes as an IDR.
        var lastOut = System.currentTimeMillis()
        var lastKick = 0L
        var kicks = 0
        try {
            while (running) {
                val outIdx = codec.dequeueOutputBuffer(info, 50_000)
                if (outIdx < 0) {
                    // Cap recovery attempts at 3 per stall episode: on Samsung neither the
                    // sync request nor the display poke revives a FULLY static pipeline (the
                    // client now treats a quiet-but-alive session as "paused", not stalled),
                    // so endless kicking is just churn/log spam. The counter re-arms on the
                    // next real output.
                    val now = System.currentTimeMillis()
                    if (kicks < 3 && now - lastOut > 1000 && now - lastKick > 1000) {
                        lastKick = now
                        kicks++
                        try {
                            val b = android.os.Bundle()
                            b.putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0)
                            codec.setParameters(b)
                        } catch (_: Exception) {}
                        if (kicks >= 2) {
                            // Sync request alone produced nothing (no input frames at
                            // all) — poke the display pipeline to force a recompose.
                            try {
                                vdisplay?.let { vd ->
                                    vd.setSurface(null)
                                    vd.setSurface(inputSurface)
                                }
                                Log.i(TAG, "static stall ${now - lastOut}ms — poked VirtualDisplay")
                            } catch (e: Exception) {
                                Log.w(TAG, "display poke failed", e)
                            }
                        }
                    }
                    continue
                }
                lastOut = System.currentTimeMillis()
                kicks = 0
                val buf = codec.getOutputBuffer(outIdx)
                if (buf != null && info.size > 0) {
                    val data = ByteArray(info.size)
                    buf.position(info.offset); buf.get(data)
                    val ts = ((info.presentationTimeUs * 90) / 1000).toInt() // 90 kHz RTP clock
                    val nals = splitNals(data)
                    for ((i, nal) in nals.withIndex()) {
                        seq = packetize(sock, addr, nal, ts, seq, ssrc, i == nals.lastIndex)
                    }
                }
                codec.releaseOutputBuffer(outIdx, false)
                if (info.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) break
            }
        } catch (e: Exception) {
            Log.e(TAG, "drain error", e)
        } finally {
            try { sock.close() } catch (_: Exception) {}
        }
    }

    private fun splitNals(d: ByteArray): List<ByteArray> {
        val out = ArrayList<ByteArray>(); val n = d.size; var p = 0; var start = -1
        fun sc(at: Int): Int {
            if (at + 2 < n && d[at].toInt() == 0 && d[at + 1].toInt() == 0 && d[at + 2].toInt() == 1) return 3
            if (at + 3 < n && d[at].toInt() == 0 && d[at + 1].toInt() == 0 && d[at + 2].toInt() == 0 && d[at + 3].toInt() == 1) return 4
            return 0
        }
        while (p < n) { val s = sc(p); if (s > 0) { if (start in 0 until p) out.add(d.copyOfRange(start, p)); p += s; start = p } else p++ }
        if (start in 0 until n) out.add(d.copyOfRange(start, n))
        return out
    }

    private val MAX = 1100

    /** RTP-packetize one NAL: single packet if small, else FU-A (H.264) / FU (H.265). */
    private fun packetize(sock: DatagramSocket, addr: InetAddress, nal: ByteArray, ts: Int, seqIn: Int, ssrc: ByteArray, lastNal: Boolean): Int {
        var seq = seqIn
        if (nal.size <= MAX) {
            send(sock, addr, rtp(seq, ts, lastNal, ssrc, nal)); return seq + 1
        }
        val h265 = mime == "video/hevc"
        if (h265) {
            val b0 = nal[0].toInt() and 0xFF; val b1 = nal[1].toInt()
            val type = (b0 shr 1) and 0x3F
            val hdr0 = ((b0 and 0x81) or (49 shl 1)).toByte()
            var off = 2; var first = true
            while (off < nal.size) {
                val chunk = minOf(MAX, nal.size - off); val last = off + chunk >= nal.size
                val fu = ByteArray(3 + chunk)
                fu[0] = hdr0; fu[1] = b1.toByte()
                fu[2] = (((if (first) 0x80 else 0) or (if (last) 0x40 else 0) or type)).toByte()
                System.arraycopy(nal, off, fu, 3, chunk)
                send(sock, addr, rtp(seq, ts, last && lastNal, ssrc, fu)); seq++; off += chunk; first = false
            }
        } else {
            val b0 = nal[0].toInt() and 0xFF
            val type = b0 and 0x1F
            val ind = ((b0 and 0xE0) or 28).toByte()
            var off = 1; var first = true
            while (off < nal.size) {
                val chunk = minOf(MAX, nal.size - off); val last = off + chunk >= nal.size
                val fu = ByteArray(2 + chunk)
                fu[0] = ind
                fu[1] = (((if (first) 0x80 else 0) or (if (last) 0x40 else 0) or type)).toByte()
                System.arraycopy(nal, off, fu, 2, chunk)
                send(sock, addr, rtp(seq, ts, last && lastNal, ssrc, fu)); seq++; off += chunk; first = false
            }
        }
        return seq
    }

    private fun rtp(seq: Int, ts: Int, marker: Boolean, ssrc: ByteArray, payload: ByteArray, pt: Int = 96): ByteArray {
        val pkt = ByteArray(12 + payload.size)
        pkt[0] = 0x80.toByte()
        pkt[1] = ((if (marker) 0x80 else 0) or pt).toByte()
        pkt[2] = ((seq shr 8) and 0xFF).toByte(); pkt[3] = (seq and 0xFF).toByte()
        pkt[4] = ((ts shr 24) and 0xFF).toByte(); pkt[5] = ((ts shr 16) and 0xFF).toByte()
        pkt[6] = ((ts shr 8) and 0xFF).toByte(); pkt[7] = (ts and 0xFF).toByte()
        System.arraycopy(ssrc, 0, pkt, 8, 4)
        System.arraycopy(payload, 0, pkt, 12, payload.size)
        return pkt
    }

    private fun send(sock: DatagramSocket, addr: InetAddress, pkt: ByteArray) {
        try { sock.send(DatagramPacket(pkt, pkt.size, addr, port)) } catch (_: Exception) {}
    }

    companion object {
        /**
         * W5-native: Probe the MediaCodecList for available **encoder** MIME types that
         * Pulsar supports as streaming codecs. Returns a list of human-friendly names in
         * preference order (best/most capable first): ["h265", "h264"].
         *
         * Called by `PulsarVideoPlugin.enumerateDecoders` (Kotlin) and mirrored on the
         * Rust side by `mobile.rs::enumerate_host_codecs` for the `host_codecs` command
         * (W5-rust-host lane).
         *
         * AV1 encoding is NOT included: most Android devices lack a hardware AV1 encoder
         * at sufficient frame rates, and software AV1 encoding is too slow for streaming.
         * The decoder side (`enumerateDecoders`) may report AV1 if the device has HW
         * decode capability.
         */
        fun probeCodecs(): List<String> {
            val supported = mutableSetOf<String>()
            try {
                val list = MediaCodecList(MediaCodecList.REGULAR_CODECS)
                for (info in list.codecInfos) {
                    if (!info.isEncoder) continue
                    for (mime in info.supportedTypes) {
                        when (mime.lowercase()) {
                            "video/hevc" -> supported.add("h265")
                            "video/avc"  -> supported.add("h264")
                            else -> {}
                        }
                    }
                }
            } catch (e: Exception) {
                Log.w("PulsarHost", "probeCodecs error", e)
            }
            // Return in preference order (h265 before h264)
            return listOf("h265", "h264").filter { it in supported }
        }
    }

    fun stop() {
        running = false
        thread?.interrupt(); thread = null
        audioThread?.interrupt(); audioThread = null
        try { audioRecord?.stop() } catch (_: Exception) {}
        try { audioRecord?.release() } catch (_: Exception) {}
        try { audioEncoder?.stop() } catch (_: Exception) {}
        try { audioEncoder?.release() } catch (_: Exception) {}
        try { vdisplay?.release() } catch (_: Exception) {}
        try { encoder?.stop() } catch (_: Exception) {}
        try { encoder?.release() } catch (_: Exception) {}
        try { inputSurface?.release() } catch (_: Exception) {}
        // NOTE: do NOT stop the projection here — a re-stream (adaptive bitrate) recreates the
        // encoder via startCapture(), which stops THIS encoder then builds a new one on the SAME
        // projection. Stopping the projection here would kill that reused projection → freeze.
        // The projection is torn down only in the plugin's stopHost (session end).
        vdisplay = null; encoder = null; inputSurface = null
        audioRecord = null; audioEncoder = null
    }
}
