package dev.pulsar.video

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.pm.PackageManager
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTrack
import android.media.Ringtone
import android.media.RingtoneManager
import android.os.Build
import android.os.VibrationEffect
import android.os.Vibrator
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat

/**
 * Voice calls (pulsar-mobile `src/call.rs`, protocol in pulsar-core `call`):
 *
 * - **Peer voice:** the other side's mic (s16le 48 kHz mono, 20 ms chunks) on an AudioTrack of
 *   its own with the VOICE_COMMUNICATION usage, so it goes to the earpiece/headset route the
 *   system uses for calls, the mic's echo canceller hears it as the far end, and the host's
 *   playback capture (media/game usages only) never sends it back to the caller.
 * - **Ringing:** a built-in tone (one cycle of PCM from Rust) looped on a static AudioTrack
 *   with the NOTIFICATION_RINGTONE usage (follows the ring volume), or the phone's own
 *   ringtone; plus vibration when the ringer mode is not silent.
 * - **Notification:** a high-priority "incoming call" notice while it rings (the app may be in
 *   the background while this phone hosts); tapping it opens Pulsar, where the call card is.
 *
 * Thread-safe: every entry point is synchronized.
 */
internal object CallAudio {
    private const val TAG = "PulsarCall"
    private const val RATE = 48_000
    /** Peer voice queued past this (a burst after a network stall) is dropped: stay live. */
    private const val MAX_QUEUED_MS = 250
    private const val CHANNEL = "pulsar_calls"
    private const val NOTIF_ID = 12

    private var voice: AudioTrack? = null
    private var voiceWritten = 0L
    private var ringTrack: AudioTrack? = null
    private var ringSystem: Ringtone? = null
    private var vibrating: Vibrator? = null

    // ---------------- peer voice ----------------

    @Synchronized
    fun voiceFeed(pcm: ByteArray) {
        if (pcm.isEmpty()) return
        val t = voice ?: openVoice() ?: return
        // Frames written minus frames played = what is still queued.
        val queuedMs = (voiceWritten - (t.playbackHeadPosition.toLong() and 0xffffffffL)) * 1000 / RATE
        if (queuedMs > MAX_QUEUED_MS) {
            t.pause(); t.flush(); t.play()
            voiceWritten = 0
        }
        val n = t.write(pcm, 0, pcm.size, AudioTrack.WRITE_NON_BLOCKING)
        if (n > 0) voiceWritten += n / 2
    }

    // AudioTrack.Builder is API 23; Android 5 (API 21/22) phones get no call audio.
    private fun openVoice(): AudioTrack? = if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) null else try {
        val min = AudioTrack.getMinBufferSize(RATE, AudioFormat.CHANNEL_OUT_MONO, AudioFormat.ENCODING_PCM_16BIT)
        val t = AudioTrack.Builder()
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_VOICE_COMMUNICATION)
                    .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                    .build()
            )
            .setAudioFormat(
                AudioFormat.Builder()
                    .setSampleRate(RATE)
                    .setChannelMask(AudioFormat.CHANNEL_OUT_MONO)
                    .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                    .build()
            )
            // Room for the drop threshold + a margin; the queue itself stays short.
            .setBufferSizeInBytes(maxOf(min, RATE * 2 * 400 / 1000))
            .setTransferMode(AudioTrack.MODE_STREAM)
            .build()
        t.play()
        voice = t
        voiceWritten = 0
        Log.i(TAG, "peer voice track open")
        t
    } catch (e: Exception) {
        Log.w(TAG, "peer voice track failed: ${e.message}")
        null
    }

    @Synchronized
    fun voiceStop() {
        voice?.let {
            try { it.stop() } catch (_: Exception) {}
            it.release()
            Log.i(TAG, "peer voice track closed")
        }
        voice = null
        voiceWritten = 0
    }

    // ---------------- ringing ----------------

    @Synchronized
    fun ringStart(ctx: Context, pcm: ByteArray, system: Boolean) {
        ringStopLocked(ctx)
        val am = ctx.getSystemService(Context.AUDIO_SERVICE) as? AudioManager
        val mode = am?.ringerMode ?: AudioManager.RINGER_MODE_NORMAL
        if (mode != AudioManager.RINGER_MODE_SILENT) vibrate(ctx)
        if (mode != AudioManager.RINGER_MODE_NORMAL) return // vibrate / silent: no sound
        if (system) {
            try {
                val uri = RingtoneManager.getActualDefaultRingtoneUri(ctx, RingtoneManager.TYPE_RINGTONE)
                val r = uri?.let { RingtoneManager.getRingtone(ctx, it) }
                if (r != null) {
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) r.isLooping = true
                    r.play()
                    ringSystem = r
                    return
                }
            } catch (e: Exception) {
                Log.w(TAG, "system ringtone failed (${e.message}) — built-in tone instead")
            }
        }
        if (pcm.size < 4 || Build.VERSION.SDK_INT < Build.VERSION_CODES.M) return
        try {
            val frames = pcm.size / 2
            val t = AudioTrack.Builder()
                .setAudioAttributes(
                    AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_NOTIFICATION_RINGTONE)
                        .setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
                        .build()
                )
                .setAudioFormat(
                    AudioFormat.Builder()
                        .setSampleRate(RATE)
                        .setChannelMask(AudioFormat.CHANNEL_OUT_MONO)
                        .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                        .build()
                )
                .setBufferSizeInBytes(frames * 2)
                .setTransferMode(AudioTrack.MODE_STATIC)
                .build()
            t.write(pcm, 0, frames * 2)
            t.setLoopPoints(0, frames, -1)
            t.play()
            ringTrack = t
        } catch (e: Exception) {
            Log.w(TAG, "ringtone track failed: ${e.message}")
        }
    }

    private fun vibrate(ctx: Context) {
        try {
            val v = ctx.getSystemService(Context.VIBRATOR_SERVICE) as? Vibrator ?: return
            if (!v.hasVibrator()) return
            val pattern = longArrayOf(0, 800, 1200)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                v.vibrate(VibrationEffect.createWaveform(pattern, 0))
            } else {
                @Suppress("DEPRECATION")
                v.vibrate(pattern, 0)
            }
            vibrating = v
        } catch (e: Exception) {
            Log.w(TAG, "vibrate failed: ${e.message}")
        }
    }

    @Synchronized
    fun ringStop(ctx: Context) = ringStopLocked(ctx)

    private fun ringStopLocked(@Suppress("UNUSED_PARAMETER") ctx: Context) {
        ringTrack?.let {
            try { it.stop() } catch (_: Exception) {}
            it.release()
        }
        ringTrack = null
        ringSystem?.let { try { it.stop() } catch (_: Exception) {} }
        ringSystem = null
        vibrating?.let { try { it.cancel() } catch (_: Exception) {} }
        vibrating = null
    }

    // ---------------- notification ----------------

    @Synchronized
    fun notifyIncoming(ctx: Context, peer: String) {
        try {
            if (Build.VERSION.SDK_INT >= 33 &&
                ContextCompat.checkSelfPermission(ctx, Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
            ) return
            val nm = ctx.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                // Silent channel: the ringtone above is the sound (and stops with the call).
                val ch = NotificationChannel(CHANNEL, NotifI18n.t(ctx, "chanCalls"), NotificationManager.IMPORTANCE_HIGH)
                ch.setSound(null, null)
                ch.enableVibration(false)
                nm.createNotificationChannel(ch)
            }
            val launch = ctx.packageManager.getLaunchIntentForPackage(ctx.packageName)
            val pi = launch?.let {
                PendingIntent.getActivity(ctx, NOTIF_ID, it, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
            }
            val title = NotifI18n.t(ctx, "callInTitle")
            val text = if (peer.isBlank()) NotifI18n.t(ctx, "callInText") else "$peer — ${NotifI18n.t(ctx, "callInText")}"
            val n = NotificationCompat.Builder(ctx, CHANNEL)
                .setSmallIcon(ctx.applicationInfo.icon)
                .setContentTitle(title)
                .setContentText(text)
                .setCategory(NotificationCompat.CATEGORY_CALL)
                .setPriority(NotificationCompat.PRIORITY_HIGH)
                .setOngoing(true)
                .setAutoCancel(true)
                .apply { if (pi != null) setContentIntent(pi) }
                .build()
            nm.notify(NOTIF_ID, n)
        } catch (e: Exception) {
            Log.w(TAG, "call notification failed: ${e.message}")
        }
    }

    @Synchronized
    fun notifyClear(ctx: Context) {
        try {
            (ctx.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager).cancel(NOTIF_ID)
        } catch (_: Exception) {}
    }
}
