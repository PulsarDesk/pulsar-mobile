package dev.pulsar.video

import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.os.Build
import android.util.Log

/**
 * What this device can decode, and how well: the connect-time codec choice (only codecs
 * with a HARDWARE decoder are offered to the host), the frame-rate cap (decoder
 * capability, not a fixed 60) and the decoder-name gates of the low-latency options.
 *
 * Hardware detection: API 29+ asks MediaCodecInfo (isHardwareAccelerated and not
 * isSoftwareOnly). Older APIs have no such flag, so a name allowlist of SoC vendor
 * prefixes decides there. Known software decoders are refused on every API level
 * (Moonlight's blacklist: AOSP/Google, ffmpeg, the Qualcomm/Samsung HEVC "sw" ones).
 */
object DecoderProbe {
    private const val TAG = "PulsarDecProbe"

    /** Wire codec name -> MIME, in the client's own preference order (used only when the
     *  host gives none). AV1 last: its hardware decoders are the rarest and youngest. */
    val CODECS = listOf("h265" to "video/hevc", "h264" to "video/avc", "av1" to "video/av01")

    private val SOFTWARE_PREFIXES = listOf(
        "omx.google.", "c2.android.", "omx.ffmpeg.", "c2.ffmpeg.", "avcdecoder",
    )

    /** SoC vendor decoder prefixes counted as hardware before API 29. */
    private val HARDWARE_PREFIXES = listOf(
        "omx.qcom.", "c2.qti.", "omx.exynos.", "c2.exynos.", "omx.sec.", "c2.sec.",
        "omx.mtk.", "c2.mtk.", "omx.hisi.", "c2.hisi.", "omx.amlogic.", "c2.amlogic.",
        "omx.nvidia.", "c2.nvidia.", "omx.rk.", "c2.rk.", "omx.realtek.", "c2.realtek.",
        "omx.brcm.", "c2.brcm.", "omx.intel.", "c2.intel.", "omx.img.", "omx.allwinner.",
        "c2.allwinner.", "omx.ms.", "c2.ms.", "omx.sprd.", "c2.sprd.", "c2.unisoc.",
        "omx.marvell.", "omx.mstar.", "omx.vpu.",
    )

    private val QUALCOMM_PREFIXES = listOf("omx.qcom", "c2.qti")

    fun isSoftwareName(name: String): Boolean {
        val n = name.lowercase()
        return SOFTWARE_PREFIXES.any { n.startsWith(it) } ||
            n.contains("swvdec") || n.contains(".sw.") || n.endsWith(".sw")
    }

    fun isHardware(info: MediaCodecInfo): Boolean {
        if (isSoftwareName(info.name)) return false
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            return info.isHardwareAccelerated && !info.isSoftwareOnly
        }
        val n = info.name.lowercase()
        return HARDWARE_PREFIXES.any { n.startsWith(it) }
    }

    /** The device's codec list, built once per process: building MediaCodecList is slow
     *  (it parses every codec's capabilities) and the list does not change at runtime. */
    private val allCodecs: Array<MediaCodecInfo> by lazy {
        MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
    }

    /** Every decoder that takes [mime], in MediaCodecList order (the platform's own
     *  preference). Aliases (API 29+) are skipped: they duplicate a real entry. */
    fun decodersFor(mime: String): List<MediaCodecInfo> = try {
        allCodecs.filter { info ->
            !info.isEncoder &&
                !(Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q && info.isAlias) &&
                info.supportedTypes.any { it.equals(mime, ignoreCase = true) }
        }
    } catch (e: Exception) {
        Log.w(TAG, "decodersFor($mime) failed", e)
        emptyList()
    }

    fun bestHardware(mime: String): MediaCodecInfo? = decodersFor(mime).firstOrNull { isHardware(it) }

    /** The decoder the live pane should open: hardware first, else any decoder that is not
     *  a known software one, else null (the caller falls back to createDecoderByType). */
    fun pickDecoder(mime: String): MediaCodecInfo? {
        val all = decodersFor(mime)
        return all.firstOrNull { isHardware(it) } ?: all.firstOrNull { !isSoftwareName(it.name) }
    }

    /** Highest common stream rate the decoder declares for [w]x[h] (either orientation;
     *  phone decoders often list the portrait size only). 0 = unknown. This is
     *  areSizeAndRateSupported: a capability limit, not a performance guarantee. */
    fun maxFps(info: MediaCodecInfo, mime: String, w: Int, h: Int): Int {
        val vc = try { info.getCapabilitiesForType(mime)?.videoCapabilities } catch (_: Exception) { null } ?: return 0
        val sw = if (w > 0) w else 1920
        val sh = if (h > 0) h else 1080
        for (f in intArrayOf(240, 165, 144, 120, 90, 75, 72, 60, 50, 48, 30, 25, 24)) {
            val ok = try {
                vc.areSizeAndRateSupported(sw, sh, f.toDouble()) || vc.areSizeAndRateSupported(sh, sw, f.toDouble())
            } catch (_: Exception) { false }
            if (ok) return f
        }
        return 0
    }

    fun isQualcomm(decoderName: String): Boolean {
        val n = decoderName.lowercase()
        return QUALCOMM_PREFIXES.any { n.startsWith(it) }
    }

    /**
     * Snapdragon 765/765G/768G (Adreno 620). Moonlight found KEY_OPERATING_RATE=MAX makes
     * their decoder crash (Mi 10 Lite 5G, Redmi K30i 5G). Moonlight reads the GL renderer
     * string; without a GL context we match the SoC: SOC_MODEL on API 31+, else the board
     * codename ("lito") / model number in BOARD or HARDWARE.
     */
    val isAdreno620Soc: Boolean by lazy {
        try {
            val soc = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) Build.SOC_MODEL.orEmpty() else ""
            val ids = listOf(soc, Build.BOARD.orEmpty(), Build.HARDWARE.orEmpty()).map { it.lowercase() }
            ids.any { it == "sm7250" || it == "lito" || it.startsWith("sm7250") }
        } catch (_: Exception) { false }
    }

    /**
     * Moonlight's gate (MediaCodecHelper.decoderSupportsMaxOperatingRate): only Qualcomm
     * decoders (minus Adreno 620 SoCs) get KEY_OPERATING_RATE=MAX; every other decoder gets
     * KEY_PRIORITY=0 (realtime) instead. Never both: a decoder asked for realtime priority
     * at an operating rate it cannot meet fails.
     */
    fun supportsMaxOperatingRate(decoderName: String): Boolean =
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.M && isQualcomm(decoderName) && !isAdreno620Soc
}
