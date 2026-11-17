package dev.pulsar.video

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log

/**
 * A voice call is up (pulsar-mobile `call.rs`): a `microphone` foreground service, so the
 * call's AudioRecord keeps hearing the user after Pulsar leaves the screen (Android 11+
 * silences a background app's mic otherwise; the stream / host services are mediaPlayback /
 * mediaProjection types, which do not cover the mic). Started when a call is answered and
 * stopped when it ends. Android only allows starting it while Pulsar is in the foreground;
 * a call answered by the PEER while Pulsar is in the background still works, but the mic is
 * silent until Pulsar is brought back (the start failure is logged).
 */
class CallService : Service() {
    companion object {
        private const val TAG = "PulsarCall"
        private const val NOTIF_ID = 13

        fun start(ctx: Context) {
            try {
                val i = Intent(ctx, CallService::class.java)
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) ctx.startForegroundService(i) else ctx.startService(i)
            } catch (e: Exception) {
                Log.w(TAG, "call service failed to start (app in background?): ${e.message}")
            }
        }

        fun stop(ctx: Context) {
            try { ctx.stopService(Intent(ctx, CallService::class.java)) } catch (_: Exception) {}
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val chan = "pulsar_call_active"
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            getSystemService(NotificationManager::class.java).createNotificationChannel(
                NotificationChannel(chan, NotifI18n.t(this, "chanCalls"), NotificationManager.IMPORTANCE_LOW)
            )
        }
        val builder = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            Notification.Builder(this, chan)
        } else {
            @Suppress("DEPRECATION") Notification.Builder(this)
        }
        val launch = packageManager.getLaunchIntentForPackage(packageName)
        val pi = launch?.let {
            android.app.PendingIntent.getActivity(
                this, NOTIF_ID, it,
                android.app.PendingIntent.FLAG_UPDATE_CURRENT or android.app.PendingIntent.FLAG_IMMUTABLE
            )
        }
        val notif = builder
            .setContentTitle("Pulsar")
            .setContentText(NotifI18n.t(this, "callActive"))
            .setSmallIcon(applicationInfo.icon)
            .setOngoing(true)
            .apply { if (pi != null) setContentIntent(pi) }
            .build()
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                startForeground(NOTIF_ID, notif, ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE)
            } else {
                startForeground(NOTIF_ID, notif)
            }
        } catch (e: Exception) {
            Log.w(TAG, "call service startForeground failed: ${e.message}")
            stopSelf()
        }
        return START_NOT_STICKY
    }
}
