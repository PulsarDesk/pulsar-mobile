package dev.pulsar.video

import android.content.Context
import android.content.Intent
import android.content.pm.PackageInfo
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.provider.Settings
import android.util.Log
import androidx.core.content.FileProvider
import app.tauri.plugin.JSObject
import java.io.File
import java.io.FileOutputStream
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Own FileProvider subclass for the update APK: the app manifest (tauri-generated) already
 * declares androidx.core.content.FileProvider for `${applicationId}.fileprovider`, and two
 * <provider> entries with the same class name collide in the manifest merger.
 */
class UpdateFileProvider : FileProvider()

/**
 * In-app updater (Android): downloads the release APK into `<external files | cache>/updates/`
 * on a background thread and hands it to the system package installer.
 *
 * Progress is kept in @Volatile fields that `update_status` returns — the plugin's own
 * `trigger()` events never reach JS in this build (registerListener is not permitted), so the
 * UI polls. The events are still emitted for when that channel works.
 *
 * Files: `Pulsar-<version>.apk.part` while downloading, fsync'd and renamed to
 * `Pulsar-<version>.apk` only when the byte count matches, so an existing `.apk` is complete.
 */
object Updater {
    private const val TAG = "PulsarUpdate"
    private const val MIME_APK = "application/vnd.android.package-archive"
    private const val PROGRESS_MS = 250L   // ≤ 4 progress events / s
    private const val BPS_WINDOW_MS = 1500L

    private val cancel = AtomicBoolean(false)
    @Volatile private var worker: Thread? = null
    @Volatile private var conn: HttpURLConnection? = null
    @Volatile var version: String = ""; private set
    @Volatile var received: Long = 0; private set
    @Volatile var total: Long = 0; private set
    @Volatile var bps: Long = 0; private set
    @Volatile var error: String = ""; private set

    val active: Boolean get() = worker?.isAlive == true

    /** `<external files dir | cache dir>/updates` — both roots are listed in res/xml/pulsar_updates.xml. */
    fun dir(ctx: Context): File {
        val root = ctx.getExternalFilesDir(null) ?: ctx.cacheDir
        return File(root, "updates").apply { mkdirs() }
    }

    private fun safe(v: String) = v.replace(Regex("[^0-9A-Za-z._-]"), "_")
    fun apkFile(ctx: Context, v: String) = File(dir(ctx), "Pulsar-${safe(v)}.apk")
    private fun partFile(ctx: Context, v: String) = File(dir(ctx), "Pulsar-${safe(v)}.apk.part")

    /**
     * Start downloading [url] as [ver]. Returns an error string when a download is already
     * running, null when started. [emit] is called with (event, payload) from the worker.
     */
    @Synchronized
    fun start(ctx: Context, url: String, ver: String, size: Long, emit: (String, JSObject) -> Unit): String? {
        // A just-cancelled worker may still be unwinding (its read is being aborted) — give it
        // a moment instead of reporting "busy" to an immediate retry.
        if (active && cancel.get()) worker?.let { try { it.join(1500) } catch (_: InterruptedException) {} }
        if (active) return if (ver == version) null else "busy"
        val app = ctx.applicationContext
        cancel.set(false)
        version = ver; received = 0; total = size; bps = 0; error = ""
        // Drop older update APKs / stale partials (anything not for this version).
        val keep = setOf(apkFile(app, ver).name, partFile(app, ver).name)
        dir(app).listFiles()?.forEach { if (it.name !in keep) it.delete() }
        val t = Thread({ run(app, url, ver, size, emit) }, "pulsar-update")
        worker = t
        t.start()
        return null
    }

    /** Non-blocking: flags the worker and aborts its socket; the worker deletes the .part too. */
    fun cancel(ctx: Context) {
        cancel.set(true)
        try { conn?.disconnect() } catch (_: Exception) {}
        if (version.isNotEmpty()) partFile(ctx.applicationContext, version).delete()
        received = 0; bps = 0; error = ""
    }

    private fun run(ctx: Context, url: String, ver: String, size: Long, emit: (String, JSObject) -> Unit) {
        val part = partFile(ctx, ver)
        val dest = apkFile(ctx, ver)
        try {
            part.delete()
            // Follow redirects by hand (GitHub → objects.githubusercontent.com); also covers
            // the cross-protocol case HttpURLConnection refuses to follow on its own.
            var u = URL(url)
            var hops = 0
            while (true) {
                val c = u.openConnection() as HttpURLConnection
                conn = c
                c.instanceFollowRedirects = false
                c.connectTimeout = 15000
                c.readTimeout = 30000
                c.setRequestProperty("Accept", "application/octet-stream")
                c.setRequestProperty("User-Agent", "Pulsar-Android")
                val code = c.responseCode
                if (code in 300..399) {
                    val loc = c.getHeaderField("Location") ?: throw Exception("redirect without Location")
                    c.disconnect()
                    if (++hops > 8) throw Exception("too many redirects")
                    u = URL(u, loc)
                    continue
                }
                if (code != 200) { c.disconnect(); throw Exception("HTTP $code") }
                break
            }
            val c = conn!!
            val len = c.contentLengthLong
            if (len > 0) total = len else if (size > 0) total = size
            val expect = if (size > 0) size else len
            if (size > 0 && len > 0 && len != size) throw Exception("size mismatch ($len ≠ $size)")

            val buf = ByteArray(64 * 1024)
            // (timestamp, bytes) samples for a ~1.5 s sliding-window speed.
            val samples = ArrayDeque<Pair<Long, Long>>()
            var lastEmit = 0L
            var got = 0L
            c.inputStream.use { input ->
                FileOutputStream(part).use { out ->
                    while (true) {
                        if (cancel.get()) throw InterruptedException()
                        val n = input.read(buf)
                        if (n < 0) break
                        out.write(buf, 0, n)
                        got += n
                        received = got
                        val now = System.currentTimeMillis()
                        if (now - lastEmit >= PROGRESS_MS) {
                            lastEmit = now
                            samples.addLast(now to got)
                            while (samples.size > 1 && now - samples.first().first > BPS_WINDOW_MS) samples.removeFirst()
                            val (t0, b0) = samples.first()
                            if (now > t0) bps = (got - b0) * 1000 / (now - t0)
                            val p = JSObject()
                            p.put("received", got); p.put("total", total); p.put("bps", bps)
                            emit("update-progress", p)
                        }
                    }
                    out.flush()
                    out.fd.sync()
                }
            }
            if (expect > 0 && got != expect) throw Exception("incomplete ($got / $expect)")
            dest.delete()
            if (!part.renameTo(dest)) throw Exception("rename failed")
            received = got; total = got; bps = 0
            val p = JSObject(); p.put("version", ver)
            emit("update-done", p)
        } catch (_: InterruptedException) {
            part.delete()
            received = 0; bps = 0
        } catch (e: Exception) {
            part.delete()
            bps = 0
            // A cancel aborts the socket, which surfaces here as an IOException — not an error.
            if (cancel.get()) { received = 0; return }
            Log.w(TAG, "download failed: $e")
            error = e.message ?: e.toString()
            val p = JSObject(); p.put("message", error)
            emit("update-error", p)
        } finally {
            try { conn?.disconnect() } catch (_: Exception) {}
            conn = null
        }
    }

    /** Status for the UI: is THIS version's APK on disk (+ size), and the live download state. */
    fun status(ctx: Context, ver: String, size: Long): JSObject {
        val f = apkFile(ctx.applicationContext, ver)
        val ok = f.isFile && (size <= 0 || f.length() == size)
        val busy = active && version == ver
        val r = JSObject()
        r.put("downloaded", ok && !busy)
        r.put("bytes", if (f.isFile) f.length() else 0L)
        r.put("active", busy)
        r.put("received", if (version == ver) received else 0L)
        r.put("total", if (version == ver) total else 0L)
        r.put("bps", if (busy) bps else 0L)
        r.put("error", if (version == ver && !busy) error else "")
        return r
    }

    /** SHA-256 of every signing cert we can read from [pi] (current signers + rotation history). */
    @Suppress("DEPRECATION")
    private fun certs(pi: PackageInfo?): Set<String> {
        if (pi == null) return emptySet()
        val sigs = mutableListOf<android.content.pm.Signature>()
        if (Build.VERSION.SDK_INT >= 28) {
            pi.signingInfo?.let { si ->
                (if (si.hasMultipleSigners()) si.apkContentsSigners else si.signingCertificateHistory)
                    ?.let { sigs.addAll(it) }
            }
        }
        pi.signatures?.let { sigs.addAll(it) }
        val md = MessageDigest.getInstance("SHA-256")
        return sigs.map { md.digest(it.toByteArray()).joinToString("") { b -> "%02x".format(b) } }.toSet()
    }

    @Suppress("DEPRECATION")
    private fun archiveCerts(pm: PackageManager, path: String): Set<String> {
        // Some Android 9–12 builds return a null signingInfo for ARCHIVES with
        // GET_SIGNING_CERTIFICATES; GET_SIGNATURES still reads them, so try both.
        val viaNew = if (Build.VERSION.SDK_INT >= 28)
            certs(pm.getPackageArchiveInfo(path, PackageManager.GET_SIGNING_CERTIFICATES)) else emptySet()
        if (viaNew.isNotEmpty()) return viaNew
        return certs(pm.getPackageArchiveInfo(path, PackageManager.GET_SIGNATURES))
    }

    @Suppress("DEPRECATION")
    private fun installedCerts(ctx: Context): Set<String> {
        val pm = ctx.packageManager
        val flags = if (Build.VERSION.SDK_INT >= 28) PackageManager.GET_SIGNING_CERTIFICATES else PackageManager.GET_SIGNATURES
        return certs(pm.getPackageInfo(ctx.packageName, flags))
    }

    /**
     * Hand the downloaded APK to the system installer. Result keys: started / needsPermission /
     * signatureMismatch / error (string). Never launches the installer for a differently-signed
     * APK — Android would refuse it anyway ("App not installed") after the user confirmed.
     */
    fun install(activity: android.app.Activity, ver: String): JSObject {
        val r = JSObject()
        val f = apkFile(activity, ver)
        if (!f.isFile) { r.put("error", "not downloaded"); return r }
        val pm = activity.packageManager
        if (Build.VERSION.SDK_INT >= 26 && !pm.canRequestPackageInstalls()) {
            try {
                activity.startActivity(
                    Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:" + activity.packageName))
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                )
            } catch (e: Exception) { Log.w(TAG, "unknown-sources settings: $e") }
            r.put("needsPermission", true)
            return r
        }
        try {
            val mine = installedCerts(activity)
            val theirs = archiveCerts(pm, f.absolutePath)
            // Only refuse when BOTH sides were readable and share no certificate (debug build vs
            // release APK). Unreadable → let the installer decide.
            if (mine.isNotEmpty() && theirs.isNotEmpty() && mine.intersect(theirs).isEmpty()) {
                r.put("signatureMismatch", true)
                return r
            }
        } catch (e: Exception) { Log.w(TAG, "signature check: $e") }
        try {
            val intent = if (Build.VERSION.SDK_INT >= 24) {
                val uri = FileProvider.getUriForFile(activity, activity.packageName + ".updates", f)
                Intent(Intent.ACTION_VIEW).setDataAndType(uri, MIME_APK)
                    .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            } else {
                Intent(Intent.ACTION_VIEW).setDataAndType(Uri.fromFile(f), MIME_APK)
            }
            intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            activity.startActivity(intent)
            r.put("started", true)
        } catch (e: Exception) {
            Log.w(TAG, "launch installer: $e")
            r.put("error", e.message ?: e.toString())
        }
        return r
    }
}
