package dev.pulsar.video

import android.Manifest
import android.app.Activity
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.ClipData
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.MediaScannerConnection
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.util.Log
import android.webkit.MimeTypeMap
import androidx.annotation.RequiresApi
import androidx.core.app.ActivityCompat
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import androidx.core.content.FileProvider
import java.io.File
import java.util.concurrent.atomic.AtomicInteger

/**
 * Files received from a peer (a host's file browser download, `DataMsg::File*`). Rust
 * reassembles them into private app storage, which no file manager or other app can reach;
 * [publish] copies the result into the public Downloads/Pulsar Received folder and posts a
 * "file received" notification with Open / Share actions.
 *
 * Android 10+ (API 29): MediaStore.Downloads with RELATIVE_PATH, no permission needed.
 * Before that: the public Downloads dir when WRITE_EXTERNAL_STORAGE (maxSdkVersion 28 in the
 * manifest) is granted, else the app's external files dir (reachable with a file manager on
 * those versions); the permission is requested for the next file. Those files are exposed to
 * other apps through the plugin's FileProvider (res/xml/pulsar_updates.xml).
 */
object ReceivedFiles {
    private const val TAG = "PulsarFiles"
    private const val SUBDIR = "Pulsar Received"
    private const val CHANNEL = "pulsar-files"
    private const val WRITE_PERM_CODE = 0x5746 // 'WF'
    /** Notification ids for received files: 100, 101, ... (distinct from 2, 7 and 8). */
    private val nextNotifId = AtomicInteger(100)

    class Result(val uri: Uri, val location: String, val mime: String)

    fun mimeOf(name: String): String {
        val ext = name.substringAfterLast('.', "").lowercase()
        if (ext.isEmpty()) return "application/octet-stream"
        return MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext) ?: "application/octet-stream"
    }

    /** Copy [src] into Downloads/Pulsar Received as [name]. Blocking (runs off the UI thread). */
    fun publish(activity: Activity, src: File, name: String): Result {
        val mime = mimeOf(name)
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            publishMediaStore(activity, src, name, mime)
        } else {
            publishLegacy(activity, src, name, mime)
        }
    }

    @RequiresApi(Build.VERSION_CODES.Q)
    private fun publishMediaStore(ctx: Context, src: File, name: String, mime: String): Result {
        val resolver = ctx.contentResolver
        val values = ContentValues().apply {
            put(MediaStore.Downloads.DISPLAY_NAME, name)
            put(MediaStore.Downloads.MIME_TYPE, mime)
            put(MediaStore.Downloads.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS + "/" + SUBDIR)
            put(MediaStore.Downloads.IS_PENDING, 1)
        }
        val uri = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
            ?: throw IllegalStateException("MediaStore insert returned null")
        try {
            resolver.openOutputStream(uri)?.use { out ->
                src.inputStream().use { it.copyTo(out, 1 shl 16) }
            } ?: throw IllegalStateException("MediaStore output stream is null")
            val done = ContentValues().apply { put(MediaStore.Downloads.IS_PENDING, 0) }
            resolver.update(uri, done, null, null)
        } catch (e: Exception) {
            try { resolver.delete(uri, null, null) } catch (_: Exception) {}
            throw e
        }
        // MediaStore renames on a clash ("photo (1).png"): show the name it really got.
        var finalName = name
        try {
            resolver.query(uri, arrayOf(MediaStore.Downloads.DISPLAY_NAME), null, null, null)?.use { c ->
                if (c.moveToFirst()) c.getString(0)?.let { finalName = it }
            }
        } catch (_: Exception) {}
        return Result(uri, Environment.DIRECTORY_DOWNLOADS + "/" + SUBDIR + "/" + finalName, mime)
    }

    private fun publishLegacy(activity: Activity, src: File, name: String, mime: String): Result {
        val granted = ContextCompat.checkSelfPermission(activity, Manifest.permission.WRITE_EXTERNAL_STORAGE) ==
            PackageManager.PERMISSION_GRANTED
        @Suppress("DEPRECATION")
        val base = if (granted) {
            Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS)
        } else {
            // Ask once for the next file; this one goes to the app's external files dir.
            activity.runOnUiThread {
                try {
                    ActivityCompat.requestPermissions(
                        activity, arrayOf(Manifest.permission.WRITE_EXTERNAL_STORAGE), WRITE_PERM_CODE
                    )
                } catch (_: Exception) {}
            }
            activity.getExternalFilesDir(Environment.DIRECTORY_DOWNLOADS)
                ?: throw IllegalStateException("no external storage")
        }
        val dir = File(base, SUBDIR)
        if (!dir.isDirectory && !dir.mkdirs()) throw IllegalStateException("cannot create $dir")
        val stem = name.substringBeforeLast('.', name)
        val ext = if (name.contains('.')) "." + name.substringAfterLast('.') else ""
        var dst = File(dir, name)
        var n = 1
        while (dst.exists() && n < 10_000) { dst = File(dir, "$stem ($n)$ext"); n++ }
        src.inputStream().use { input -> dst.outputStream().use { input.copyTo(it, 1 shl 16) } }
        try { MediaScannerConnection.scanFile(activity, arrayOf(dst.absolutePath), arrayOf(mime), null) } catch (_: Exception) {}
        val uri = FileProvider.getUriForFile(activity, activity.packageName + ".updates", dst)
        val location = if (granted) Environment.DIRECTORY_DOWNLOADS + "/" + SUBDIR + "/" + dst.name else dst.absolutePath
        return Result(uri, location, mime)
    }

    private fun viewIntent(uri: Uri, mime: String): Intent =
        Intent(Intent.ACTION_VIEW).apply {
            setDataAndType(uri, mime)
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
        }

    private fun shareIntent(uri: Uri, mime: String): Intent {
        val send = Intent(Intent.ACTION_SEND).apply {
            type = mime
            putExtra(Intent.EXTRA_STREAM, uri)
            clipData = ClipData.newRawUri("", uri)
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
        // createChooser carries the ClipData + read grant over to the chooser intent.
        return Intent.createChooser(send, null).apply { addFlags(Intent.FLAG_ACTIVITY_NEW_TASK) }
    }

    /** Open [uri] in another app. Throws ActivityNotFoundException when none handles it. */
    fun open(ctx: Context, uri: Uri, mime: String) {
        ctx.startActivity(viewIntent(uri, mime.ifEmpty { ctx.contentResolver.getType(uri) ?: "*/*" }))
    }

    fun share(ctx: Context, uri: Uri, mime: String) {
        ctx.startActivity(shareIntent(uri, mime.ifEmpty { ctx.contentResolver.getType(uri) ?: "*/*" }))
    }

    /** "File received" notification: tap or Open opens it, Share opens the share sheet. */
    fun notifyReceived(ctx: Context, r: Result, name: String) {
        try {
            val nm = ctx.getSystemService(NotificationManager::class.java) ?: return
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                nm.createNotificationChannel(
                    NotificationChannel(CHANNEL, NotifI18n.t(ctx, "chanFiles"), NotificationManager.IMPORTANCE_DEFAULT)
                )
            }
            val id = nextNotifId.getAndIncrement()
            val flags = PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
            val openPi = PendingIntent.getActivity(ctx, id * 2, viewIntent(r.uri, r.mime), flags)
            val sharePi = PendingIntent.getActivity(ctx, id * 2 + 1, shareIntent(r.uri, r.mime), flags)
            val n = NotificationCompat.Builder(ctx, CHANNEL)
                .setSmallIcon(ctx.applicationInfo.icon)
                .setContentTitle(NotifI18n.t(ctx, "fileRecvTitle"))
                .setContentText(name)
                .setSubText(r.location.substringBeforeLast('/'))
                .setAutoCancel(true)
                .setContentIntent(openPi)
                .addAction(0, NotifI18n.t(ctx, "fileOpen"), openPi)
                .addAction(0, NotifI18n.t(ctx, "fileShare"), sharePi)
                .build()
            nm.notify(id, n)
        } catch (e: Exception) {
            Log.w(TAG, "file notification failed: ${e.message}")
        }
    }
}
