package dev.pulsar.video

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.net.wifi.WifiManager
import android.os.IBinder

/** M16: foreground service of type mediaProjection — required to capture the screen. */
class HostService : Service() {
    companion object {
        /** True once startForeground(mediaProjection) has run — getMediaProjection is only
         *  legal AFTER this (Android 14+), so the plugin waits on it to avoid a race. */
        @Volatile
        var ready = false

        /** Notification "stop sharing" action → set by the plugin; asks the JS layer to
         *  go offline (which tears the capture + this service down). */
        @Volatile
        var onStopRequest: (() -> Unit)? = null

        const val ACTION_STOP = "dev.pulsar.video.STOP_HOST"
    }

    override fun onBind(intent: Intent?): IBinder? = null
    override fun onDestroy() {
        ready = false
        releaseWifiLocks()
        super.onDestroy()
    }

    // 1.17: keep Wi-Fi out of power save while the screen is shared — PSM batches the
    // outgoing stream into ~100 ms bursts (latency spikes, AP queue loss). HIGH_PERF works
    // while our app is in the background (the usual hosting case; the platform stopped
    // honouring it on newer releases), LOW_LATENCY (API 29+) only while the app is in the
    // foreground with the screen on. Both are held; each applies when it can.
    private var wifiLocks: List<WifiManager.WifiLock> = emptyList()

    private fun acquireWifiLocks() {
        if (wifiLocks.isNotEmpty()) return
        try {
            val wm = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
            val modes = mutableListOf(@Suppress("DEPRECATION") WifiManager.WIFI_MODE_FULL_HIGH_PERF)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) modes.add(WifiManager.WIFI_MODE_FULL_LOW_LATENCY)
            wifiLocks = modes.map { m ->
                wm.createWifiLock(m, "pulsar:host-$m").apply { setReferenceCounted(false); acquire() }
            }
            android.util.Log.i("PulsarHostSvc", "wifi locks acquired (${wifiLocks.size})")
        } catch (e: Exception) {
            android.util.Log.w("PulsarHostSvc", "wifi lock failed: ${e.message}")
        }
    }

    private fun releaseWifiLocks() {
        for (l in wifiLocks) try { if (l.isHeld) l.release() } catch (_: Exception) {}
        wifiLocks = emptyList()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            onStopRequest?.invoke()
            return START_NOT_STICKY
        }
        val chan = "pulsar_host"
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            getSystemService(NotificationManager::class.java).createNotificationChannel(
                NotificationChannel(chan, NotifI18n.t(this, "chanHost"), NotificationManager.IMPORTANCE_LOW)
            )
        }
        val b = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            Notification.Builder(this, chan)
        } else {
            @Suppress("DEPRECATION") Notification.Builder(this)
        }
        val stopPi = android.app.PendingIntent.getService(
            this, 3,
            Intent(this, HostService::class.java).setAction(ACTION_STOP),
            android.app.PendingIntent.FLAG_UPDATE_CURRENT or android.app.PendingIntent.FLAG_IMMUTABLE
        )
        val n = b.setContentTitle("Pulsar")
            .setContentText(NotifI18n.t(this, "sharing"))
            // The app's own icon, not a generic system glyph (the status bar renders it
            // monochrome, but the shade shows it recognizably as Pulsar).
            .setSmallIcon(applicationInfo.icon)
            .setOngoing(true)
            .addAction(
                Notification.Action.Builder(
                    android.graphics.drawable.Icon.createWithResource(this, android.R.drawable.ic_media_pause),
                    NotifI18n.t(this, "stopSharing"),
                    stopPi
                ).build()
            )
            .build()
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                startForeground(2, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
            } else {
                startForeground(2, n)
            }
            ready = true
            android.util.Log.i("PulsarHostSvc", "startForeground OK (mediaProjection)")
            acquireWifiLocks()
        } catch (e: Exception) {
            android.util.Log.e("PulsarHostSvc", "startForeground FAILED", e)
        }
        return START_STICKY
    }
}
