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
 * The companion's [probeCodecs] lists the codecs this phone can encode (the host's
 * StreamCaps) and [plan] fits the client's size / fps / bit rate request to the encoder.
 */
class HostEncoder(
    private val projection: MediaProjection,
    initialMime: String,
    private val width: Int,
    private val height: Int,
    initialFps: Int,
    initialBitrate: Int,
    private val densityDpi: Int,
    private val port: Int,
    private val audioPort: Int,
    // Adaptive streaming loss recovery: "normal" (5 s GOP + IDR on request), "short_gop"
    // (keyframe every ~0.5 s) or "intra_refresh" (rolling intra wave, API 24+). Applied at
    // creation — MediaCodec can't change its GOP live.
    private val recovery: String = "normal",
    // The encoder [plan] picked (hardware first); null = MediaCodec's default for the type.
    initialEncoderName: String? = null,
) {
    // Codec / rate / encoder of the RUNNING encoder. A re-stream that changes them rebuilds
    // the encoder in place ([retune]); the drain loop reads its own copy of the codec.
    @Volatile var mime: String = initialMime
        private set
    @Volatile var fps: Int = initialFps
        private set
    @Volatile private var bitrate: Int = initialBitrate
    @Volatile private var encoderName: String? = initialEncoderName
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

    /**
     * The encoder format. [tryNumber] walks a ladder of realtime hints from most to least
     * (1.17), like the decoder's low-latency options: the first format that configures wins.
     *  - try 0: + vendor low-latency keys (only keys the encoder lists, API 31+; Qualcomm's
     *    key by name on API 26-30)
     *  - try 0-1: + KEY_PRIORITY 0 (realtime), KEY_LATENCY 1 (API 30+: one frame in flight),
     *    KEY_OPERATING_RATE = fps
     *  - try 0-2: + CBR when the encoder supports it — a steady per-frame size is what a
     *    paced, loss-sensitive stream wants; VBR overshoots on scene changes
     *  - try 3: the plain format the host always used
     */
    private fun videoFormat(
        w: Int, h: Int, tryNumber: Int = 3, codec: MediaCodec? = null,
        mime: String = this.mime, fps: Int = this.fps, bitrate: Int = this.bitrate,
    ): MediaFormat =
        MediaFormat.createVideoFormat(mime, w, h).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, bitrate)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)
            // GOP 5 s (was 1 s). A per-second IDR is a ~50-150-packet UDP burst every second —
            // the main packet-loss trigger on Wi-Fi (AP queue overflow) and thus the main
            // corruption source. Loss repair is NACK retransmit; an unrepairable hole makes
            // the client send MediaNack([0]) → requestSyncFrame() → immediate IDR. So the
            // scheduled keyframe cadence can be long, like Sunshine/Moonlight.
            when (recovery) {
                "short_gop" -> {
                    // A keyframe at least every ~0.5 s so a lost reference heals fast. Float
                    // intervals are honoured from API 25; older encoders get 1 s.
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N_MR1) setFloat(MediaFormat.KEY_I_FRAME_INTERVAL, 0.5f)
                    else setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 1)
                }
                "intra_refresh" -> {
                    setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 5)
                    // Rolling intra refresh over half a second (best effort: encoders that
                    // don't support it ignore the key).
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) setInteger(MediaFormat.KEY_INTRA_REFRESH_PERIOD, maxOf(1, fps / 2))
                }
                else -> setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 5)
            }
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
            // The client's fps is a cap, not only a hint: a 90/120 Hz panel composes more
            // frames than asked for, and the surface would feed them all to the encoder
            // (more bits per second than the bitrate assumes, and a load the client did not
            // ask for). The framework's input surface drops the excess (API 29+).
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                setFloat(MediaFormat.KEY_MAX_FPS_TO_ENCODER, fps.toFloat())
            }
            // Pulsar's SDR colour rule (every host, every client): BT.709, limited range. A
            // surface-input encoder converts the screen's RGB with these aspects and writes
            // them into the VUI; left unset, the vendor picks (often BT.601, untagged) and the
            // desktop clients decode BT.709 — shifted reds / greens. API 24+; an encoder that
            // ignores the keys keeps its default.
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
                setInteger(MediaFormat.KEY_COLOR_STANDARD, MediaFormat.COLOR_STANDARD_BT709)
                setInteger(MediaFormat.KEY_COLOR_RANGE, MediaFormat.COLOR_RANGE_LIMITED)
                setInteger(MediaFormat.KEY_COLOR_TRANSFER, MediaFormat.COLOR_TRANSFER_SDR_VIDEO)
            }
            if (codec != null) applyRealtimeHints(this, tryNumber, codec, mime, fps)
        }

    private fun applyRealtimeHints(fmt: MediaFormat, tryNumber: Int, codec: MediaCodec, mime: String, fps: Int) {
        if (tryNumber < 1 && Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val name = codec.name.lowercase()
            val known = listOf(
                "vendor.qti-ext-enc-low-latency.enable",
                "vendor.rtc-ext-enc-low-latency.enable",
                "vendor.hisi-ext-low-latency-video-enc.video-scene-for-low-latency-req",
            )
            val supported: List<String>? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                try { codec.supportedVendorParameters } catch (_: Exception) { null }
            } else null
            for (k in known) {
                // API 31+: set only what the encoder declares. Older: Qualcomm's key by name
                // (unknown keys are ignored; one that breaks configure is dropped by try 1).
                val use = supported?.any { it.equals(k, ignoreCase = true) }
                    ?: (k.startsWith("vendor.qti") && (name.startsWith("omx.qcom") || name.startsWith("c2.qti")))
                if (use) fmt.setInteger(k, 1)
            }
        }
        if (tryNumber < 2 && Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            fmt.setInteger(MediaFormat.KEY_PRIORITY, 0)
            fmt.setInteger(MediaFormat.KEY_OPERATING_RATE, fps)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) fmt.setInteger(MediaFormat.KEY_LATENCY, 1)
        }
        if (tryNumber < 3) {
            val cbr = try {
                codec.codecInfo.getCapabilitiesForType(mime).encoderCapabilities
                    ?.isBitrateModeSupported(MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR) == true
            } catch (_: Exception) { false }
            if (cbr) fmt.setInteger(MediaFormat.KEY_BITRATE_MODE, MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR)
        }
    }

    /** A started encoder + its input surface at [w]x[h], with the richest hint set that
     *  configures (see [videoFormat]). A fresh MediaCodec per try: a failed configure can
     *  leave the instance unusable. */
    private fun createEncoder(
        w: Int, h: Int,
        mime: String = this.mime, fps: Int = this.fps, bitrate: Int = this.bitrate,
        name: String? = this.encoderName,
    ): Pair<MediaCodec, Surface> {
        var last: Exception? = null
        for (tryNumber in 0..3) {
            var c: MediaCodec? = null
            try {
                // The hardware encoder [plan] picked; the type's default when it has none
                // or it cannot be created.
                c = name?.let { try { MediaCodec.createByCodecName(it) } catch (_: Exception) { null } }
                    ?: MediaCodec.createEncoderByType(mime)
                c.configure(videoFormat(w, h, tryNumber, c, mime, fps, bitrate), null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
                val surface = c.createInputSurface()
                c.start()
                Log.i(TAG, "encoder ${c.name} ${w}x${h} configured (hints try=$tryNumber)")
                return Pair(c, surface)
            } catch (e: Exception) {
                last = e
                Log.w(TAG, "encoder configure try=$tryNumber failed: ${e.message}")
                try { c?.release() } catch (_: Exception) {}
            }
        }
        throw last ?: RuntimeException("encoder configure failed")
    }

    fun start() {
        val (codec, surface) = createEncoder(width, height)
        encoder = codec
        inputSurface = surface

        // API 34+ requires a registered MediaProjection callback before createVirtualDisplay:
        // the plugin registers it when it obtains the projection (PulsarVideoPlugin
        // .watchProjection — its onStop ends the stream), so not again here per encoder.
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
            bitrate = bps
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
        if (newW == curW && newH == curH) return
        rebuild(newW, newH, mime, fps, bitrate, encoderName)
    }

    /** The size the running encoder streams at. */
    val streamWidth: Int get() = curW
    val streamHeight: Int get() = curH

    /**
     * A re-stream that changes the codec, size or fps (the client picked another one in its
     * session menu, or its ABR stepped the resolution / frame rate): rebuild the encoder on
     * the SAME VirtualDisplay, like a rotation. The desktop host restarts its capture for
     * these; a bitrate-only change stays live ([setBitrate]). On failure the running encoder
     * keeps going unchanged. Returns whether the new encoder runs.
     */
    fun retune(newMime: String, newName: String?, newW: Int, newH: Int, newFps: Int, newBitrate: Int): Boolean =
        rebuild(newW, newH, newMime, newFps, newBitrate, newName)

    private fun rebuild(newW: Int, newH: Int, newMime: String, newFps: Int, newBitrate: Int, newName: String?): Boolean {
        val vd = vdisplay ?: return false
        if (newW <= 0 || newH <= 0 || newFps <= 0) return false
        try {
            // 1) New encoder + surface at the new size (started before we repoint the VD).
            val (codec, surface) = createEncoder(newW, newH, newMime, newFps, newBitrate, newName)
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
            mime = newMime; fps = newFps; bitrate = newBitrate; encoderName = newName
            av1SeqHeader = null
            running = true
            thread = Thread { drainLoop() }.also { it.isDaemon = true; it.start() }
            Log.i(TAG, "rebuild $newMime ${newW}x${newH}@$newFps ${newBitrate / 1000}k (${codec.name})")
            return true
        } catch (e: Exception) {
            Log.e(TAG, "rebuild failed", e)
            return false
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
        try { android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_URGENT_AUDIO) } catch (_: Exception) {}
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
        // Same for the codec: a retune() swaps `mime` only after this loop has stopped.
        val m = mime
        // The capture -> encode -> send path is latency-critical (the client's decode thread
        // runs at this priority too).
        try { android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_URGENT_DISPLAY) } catch (_: Exception) {}
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
                    if (m == MIME_AV1) {
                        sendAv1(sock, addr, data, info.flags, ts, ssrc)
                    } else {
                        val nals = splitNals(data)
                        for ((i, nal) in nals.withIndex()) {
                            seq = packetize(sock, addr, nal, ts, seq, ssrc, i == nals.lastIndex, m)
                        }
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
    private fun packetize(sock: DatagramSocket, addr: InetAddress, nal: ByteArray, ts: Int, seqIn: Int, ssrc: ByteArray, lastNal: Boolean, mime: String): Int {
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

    // ── AV1 (aomedia "RTP Payload Format for AV1", as the desktop host sends it) ──────────
    // The encoder emits low-overhead OBU temporal units (no start codes; every OBU carries its
    // obu_size). Like pulsar-capture's `send_av1_tu`, a TU is cut into MTU-sized chunks, each
    // ONE length-prefixed element under the all-zero aggregation header (Z=0 Y=0 W=0 N=0):
    // both Pulsar clients rebuild a W=0 stream by plain concatenation, and 0x00 reads the
    // same under both clients' header layouts. The marker bit ends the TU.

    /** The sequence header OBU(s) from the codec-config buffer, re-sent with keyframes. */
    @Volatile private var av1SeqHeader: ByteArray? = null

    private fun sendAv1(sock: DatagramSocket, addr: InetAddress, data: ByteArray, flags: Int, ts: Int, ssrc: ByteArray) {
        if (flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
            // csd-0 is an AV1CodecConfigurationRecord (av1C: 4 header bytes, marker bit set,
            // then the config OBUs) on most encoders, plain OBUs on some. An OBU header's
            // first bit is forbidden (0), so the marker tells them apart. Not sent on its own:
            // a TU of just a sequence header is no frame for the decoder.
            av1SeqHeader = if (data.isNotEmpty() && (data[0].toInt() and 0x80) != 0) {
                if (data.size > 4) data.copyOfRange(4, data.size) else null
            } else data
            return
        }
        var tu = data
        // Keyframes must be self-describing (a client that joins late or rebuilds its decoder
        // needs the sequence header), like KEY_PREPEND_HEADER_TO_SYNC_FRAMES for H.264/H.265.
        val sh = av1SeqHeader
        if (flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0 && sh != null && !av1HasObu(tu, 1)) {
            val at = av1LeadingTdEnd(tu) // after a temporal delimiter: it must come first
            tu = tu.copyOfRange(0, at) + sh + tu.copyOfRange(at, tu.size)
        }
        var off = 0
        while (off < tu.size) {
            val end = minOf(off + AV1_CHUNK, tu.size)
            val chunk = end - off
            val lenBytes = leb128(chunk)
            val payload = ByteArray(1 + lenBytes.size + chunk)
            payload[0] = 0x00 // aggregation header: Z=0 Y=0 W=0 N=0 (cross-client safe)
            System.arraycopy(lenBytes, 0, payload, 1, lenBytes.size)
            System.arraycopy(tu, off, payload, 1 + lenBytes.size, chunk)
            send(sock, addr, rtp(seq, ts, end == tu.size, ssrc, payload))
            seq++
            off = end
        }
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
        const val MIME_AV1 = "video/av01"

        /** OBU bytes per AV1 packet: the H.264/H.265 FU budget minus the aggregation header
         *  and a 2-byte LEB128 length (any chunk < 16384 encodes in 2 bytes). */
        private const val AV1_CHUNK = 1100 - 3

        private fun leb128(v: Int): ByteArray {
            val out = ArrayList<Byte>(2)
            var x = v
            do {
                var b = x and 0x7F
                x = x ushr 7
                if (x != 0) b = b or 0x80
                out.add(b.toByte())
            } while (x != 0)
            return out.toByteArray()
        }

        /** Read a LEB128 value at [at]: (value, byte count), or null when truncated. */
        private fun readLeb128(d: ByteArray, at: Int): Pair<Long, Int>? {
            var v = 0L
            for (i in 0 until 8) {
                if (at + i >= d.size) return null
                val b = d[at + i].toInt() and 0xFF
                v = v or ((b and 0x7F).toLong() shl (7 * i))
                if (b and 0x80 == 0) return Pair(v, i + 1)
            }
            return null
        }

        /** Walk the OBUs of a low-overhead TU; [f] gets (type, start, end) and returns false
         *  to stop. Stops at an OBU without a size field (it runs to the end). */
        private fun av1Obus(d: ByteArray, f: (Int, Int, Int) -> Boolean) {
            var i = 0
            while (i < d.size) {
                val hdr = d[i].toInt() and 0xFF
                val type = (hdr shr 3) and 0x0F
                val ext = hdr and 0x04 != 0
                val hasSize = hdr and 0x02 != 0
                val p = i + 1 + (if (ext) 1 else 0)
                if (!hasSize) { f(type, i, d.size); return }
                val (size, n) = readLeb128(d, p) ?: return
                val end = p + n + size
                if (end > d.size || size < 0) return
                if (!f(type, i, end.toInt())) return
                i = end.toInt()
            }
        }

        private fun av1HasObu(d: ByteArray, obuType: Int): Boolean {
            var found = false
            av1Obus(d) { t, _, _ -> if (t == obuType) found = true; !found }
            return found
        }

        /** End of a leading temporal delimiter OBU (type 2), or 0 when the TU has none. */
        private fun av1LeadingTdEnd(d: ByteArray): Int {
            var end = 0
            av1Obus(d) { t, start, e -> if (start == 0 && t == 2) end = e; false }
            return end
        }

        /** Wire codec name ("av1" / "h265" / "h264") → MediaCodec type. */
        fun mimeFor(codec: String?): String = when (codec?.lowercase()) {
            "av1", "av01" -> MIME_AV1
            "h265", "hevc" -> "video/hevc"
            else -> "video/avc"
        }

        /** Whether [info] is a hardware encoder. API 29+ asks the platform; before that the
         *  AOSP / Google software codecs are known by name. */
        fun isHardware(info: MediaCodecInfo): Boolean {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                return info.isHardwareAccelerated && !info.isSoftwareOnly
            }
            val n = info.name.lowercase()
            return !(n.startsWith("omx.google.") || n.startsWith("c2.android.") ||
                n.startsWith("omx.ffmpeg.") || n.startsWith("c2.ffmpeg."))
        }

        /** The encoder to use for [mime]: the first hardware one in the platform's order,
         *  else the first at all; null when the device cannot encode the type. */
        /** The device's codec list, read once (it does not change while the app runs). */
        private val codecInfos: Array<MediaCodecInfo> by lazy {
            try { MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos } catch (e: Exception) {
                Log.w("PulsarHost", "MediaCodecList error", e); emptyArray()
            }
        }

        fun bestEncoder(mime: String): MediaCodecInfo? = try {
            val all = codecInfos.filter { info ->
                info.isEncoder && info.supportedTypes.any { it.equals(mime, ignoreCase = true) }
            }
            all.firstOrNull { isHardware(it) } ?: all.firstOrNull()
        } catch (e: Exception) {
            Log.w("PulsarHost", "bestEncoder($mime) error", e); null
        }

        @Volatile private var probed: List<String>? = null

        /**
         * The video codecs this phone can ENCODE as a host, best first, as the desktop host
         * advertises them: AV1 and H.265 only from a hardware encoder (a software AV1 / HEVC
         * encoder cannot keep up with a screen stream), H.264 always (the fallback every
         * client decodes). Cached: the encoder list does not change while the app runs.
         */
        fun probeCodecs(): List<String> {
            probed?.let { return it }
            val out = ArrayList<String>(3)
            if (bestEncoder(MIME_AV1)?.let { isHardware(it) } == true) out.add("av1")
            if (bestEncoder("video/hevc")?.let { isHardware(it) } == true) out.add("h265")
            out.add("h264")
            probed = out
            Log.i("PulsarHost", "host encoders: $out")
            return out
        }

        /** What one stream is encoded at: the encoder, its size / fps / bit rate. */
        data class Plan(val name: String?, val hardware: Boolean, val w: Int, val h: Int, val fps: Int, val bps: Int)

        // A software encoder (the emulator; a phone without a hardware one for the type)
        // keeps the limits the host always had: a larger frame overloaded it into
        // macroblock garbage.
        private const val SW_MAX_DIM = 1280
        private const val SW_MAX_FPS = 30
        private const val SW_MAX_KBPS = 8000
        private const val DEFAULT_KBPS = 6000
        private const val DEFAULT_FPS = 60

        /**
         * Honour the client's request (0 = "host default") up to what the screen and the
         * encoder can do, like the desktop host streams the size / fps / bit rate it is asked
         * for:
         *  - size: the screen fitted into the requested box, orientation-agnostic (the box's
         *    long edge bounds the screen's long edge — a portrait phone asked for 1920x1080
         *    streams 486x1080, not a stretched frame), never upscaled; then shrunk until the
         *    encoder's VideoCapabilities take it, aligned to its width / height alignment.
         *  - fps: the request (default 60), at most the panel's refresh (it composes no more)
         *    and the encoder's supported rate at that size.
         *  - bit rate: the request (default 6 Mbit/s) inside the encoder's bitrate range.
         */
        fun plan(mime: String, screenW: Int, screenH: Int, refreshHz: Int, reqW: Int, reqH: Int, reqFps: Int, reqKbps: Int): Plan {
            val info = bestEncoder(mime)
            val hw = info?.let { isHardware(it) } ?: false
            val vc = try { info?.getCapabilitiesForType(mime)?.videoCapabilities } catch (_: Exception) { null }
            var w = screenW.toDouble(); var h = screenH.toDouble()
            fun scaleTo(s: Double) { if (s < 1.0) { w *= s; h *= s } }
            if (reqW > 0 && reqH > 0) {
                scaleTo(minOf(maxOf(reqW, reqH) / maxOf(w, h), minOf(reqW, reqH) / minOf(w, h)))
            }
            if (!hw) scaleTo(SW_MAX_DIM / maxOf(w, h))
            val wa = maxOf(2, vc?.widthAlignment ?: 2)
            val ha = maxOf(2, vc?.heightAlignment ?: 2)
            fun aligned(): Pair<Int, Int> = Pair(maxOf(wa, w.toInt() / wa * wa), maxOf(ha, h.toInt() / ha * ha))
            var (ew, eh) = aligned()
            if (vc != null) {
                var tries = 0
                while (!vc.isSizeSupported(ew, eh) && tries++ < 24) {
                    // Over the encoder's widths / heights: shrink both (aspect kept).
                    val sw = vc.supportedWidths.upper.toDouble() / ew
                    val sh = vc.supportedHeights.upper.toDouble() / eh
                    scaleTo(minOf(0.92, sw, sh))
                    val p = aligned(); ew = p.first; eh = p.second
                }
            }
            var fps = if (reqFps > 0) reqFps else DEFAULT_FPS
            if (refreshHz > 0) fps = minOf(fps, refreshHz)
            if (!hw) fps = minOf(fps, SW_MAX_FPS)
            if (vc != null) {
                val maxRate = try { vc.getSupportedFrameRatesFor(ew, eh).upper } catch (_: Exception) { vc.supportedFrameRates.upper.toDouble() }
                if (maxRate >= 1.0) fps = minOf(fps, maxRate.toInt())
            }
            fps = fps.coerceAtLeast(1)
            var kbps = if (reqKbps > 0) reqKbps else DEFAULT_KBPS
            if (!hw) kbps = minOf(kbps, SW_MAX_KBPS)
            var bps = kbps.toLong() * 1000L
            vc?.bitrateRange?.let { r -> bps = bps.coerceIn(r.lower.toLong(), r.upper.toLong()) }
            return Plan(info?.name, hw, ew, eh, fps, bps.coerceAtMost(Int.MAX_VALUE.toLong()).toInt())
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
