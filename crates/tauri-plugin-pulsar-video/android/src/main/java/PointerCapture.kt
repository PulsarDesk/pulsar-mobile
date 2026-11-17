package dev.pulsar.video

import android.annotation.TargetApi
import android.app.Activity
import android.os.Build
import android.util.Log
import android.view.InputDevice
import android.view.MotionEvent
import android.view.View
import android.view.ViewTreeObserver
import android.webkit.WebView

/**
 * Physical-mouse pointer capture while the HOST's pointer is locked (a game grabbed the
 * mouse and reads relative motion — `DataMsg::PointerLock(true)`).
 *
 * Android 8+ `View.requestPointerCapture`: the local pointer disappears and the mouse's raw
 * deltas, buttons and wheel arrive through the captured-pointer listener instead of as
 * positioned mouse events. They are handed to the webview as
 * `window.__pulsarCapMouse(kind, a, b)` — the JS input layer (app.js) owns the session and
 * slot and forwards them as `PointerRelative` / `PointerButton` / `Scroll`:
 *
 * - `'m', dx, dy` — relative motion in device counts (all batched samples summed)
 * - `'b', button, down` — wire button id (0 left, 1 right, 2 middle, 3 back, 4 forward), 1/0
 * - `'w', dx, dy` — wheel in wire units (100 = one notch, +dy = down, +dx = right)
 *
 * Capture is lost whenever the window loses focus (app switch, a system dialog); it is
 * requested again when focus returns, as long as the lock is still wanted. Touch input is
 * not affected by pointer capture, so the overlay stays reachable by touch.
 */
// Every capture API is Android 8+ (O); `set` returns early below that, and the listener is
// only created behind that check, so nothing here loads the missing classes on older devices.
@TargetApi(Build.VERSION_CODES.O)
internal class PointerCapture(private val activity: Activity, private val webView: () -> WebView?) {
    private val TAG = "PulsarPtrCap"
    /** The JS layer wants the mouse captured (host pointer locked, session live). */
    private var wanted = false
    /** The view the listeners are installed on (the webview), null when none. */
    private var installed: WebView? = null

    /** MotionEvent button bits the captured mouse holds down (as last forwarded). */
    private var held = 0

    private val focusListener = ViewTreeObserver.OnWindowFocusChangeListener { hasFocus ->
        // Focus lost = capture lost: no release would ever arrive for a held button.
        if (!hasFocus) releaseAll()
        if (hasFocus && wanted) request()
    }

    private var listener: View.OnCapturedPointerListener? = null

    /** Apply the wanted state. Any thread; runs on the UI thread. */
    fun set(on: Boolean) {
        activity.runOnUiThread {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return@runOnUiThread
            wanted = on
            val wv = webView() ?: return@runOnUiThread
            if (on) {
                if (installed !== wv) {
                    uninstall()
                    val l = listener ?: View.OnCapturedPointerListener { _, ev -> onCaptured(ev) }.also { listener = it }
                    wv.setOnCapturedPointerListener(l)
                    wv.viewTreeObserver.addOnWindowFocusChangeListener(focusListener)
                    installed = wv
                }
                if (wv.hasWindowFocus()) request()
            } else {
                releaseAll()
                try { if (wv.hasPointerCapture()) wv.releasePointerCapture() } catch (_: Exception) {}
                uninstall()
            }
        }
    }

    private fun request() {
        val wv = installed ?: return
        try {
            // Captured events are dispatched along the focus path: the webview must hold focus.
            if (!wv.hasFocus()) wv.requestFocus()
            if (!wv.hasPointerCapture()) wv.requestPointerCapture()
            Log.i(TAG, "pointer capture requested")
        } catch (e: Exception) { Log.w(TAG, "requestPointerCapture failed: ${e.message}") }
    }

    private fun uninstall() {
        val wv = installed ?: return
        try { wv.setOnCapturedPointerListener(null) } catch (_: Exception) {}
        try {
            if (wv.viewTreeObserver.isAlive) wv.viewTreeObserver.removeOnWindowFocusChangeListener(focusListener)
        } catch (_: Exception) {}
        installed = null
    }

    private fun onCaptured(ev: MotionEvent): Boolean {
        // A captured TOUCHPAD reports absolute finger positions on the pad, not deltas: only a
        // relative mouse is forwarded (a touchpad still works for clicks below).
        val relative = ev.isFromSource(InputDevice.SOURCE_MOUSE_RELATIVE)
        // Buttons from the button-state diff on EVERY captured event: captured streams carry
        // DOWN/UP and BUTTON_PRESS/RELEASE inconsistently across devices, the state is exact.
        syncButtons(ev.buttonState)
        when (ev.actionMasked) {
            MotionEvent.ACTION_MOVE, MotionEvent.ACTION_HOVER_MOVE -> {
                if (!relative) return true
                var dx = ev.x
                var dy = ev.y
                for (i in 0 until ev.historySize) {
                    dx += ev.getHistoricalX(i)
                    dy += ev.getHistoricalY(i)
                }
                if (dx != 0f || dy != 0f) emit("m", dx, dy)
            }
            MotionEvent.ACTION_SCROLL -> {
                // AXIS_VSCROLL is +1 per notch UP; the wire wants +dy = down, 100 per notch.
                val v = ev.getAxisValue(MotionEvent.AXIS_VSCROLL)
                val h = ev.getAxisValue(MotionEvent.AXIS_HSCROLL)
                if (v != 0f || h != 0f) emit("w", h * 100f, -v * 100f)
            }
        }
        return true
    }

    private fun syncButtons(state: Int) {
        val changed = state xor held
        if (changed == 0) return
        for ((bit, id) in BUTTONS) {
            if ((changed and bit) == 0) continue
            emit("b", id.toFloat(), if ((state and bit) != 0) 1f else 0f)
        }
        held = state
    }

    /** Release every button still held (capture ends or the window loses focus). */
    private fun releaseAll() = syncButtons(0)

    private companion object {
        /** MotionEvent button bit → wire button id (0 left, 1 right, 2 middle, 3 back, 4 fwd). */
        val BUTTONS = arrayOf(
            MotionEvent.BUTTON_PRIMARY to 0,
            MotionEvent.BUTTON_SECONDARY to 1,
            MotionEvent.BUTTON_TERTIARY to 2,
            MotionEvent.BUTTON_BACK to 3,
            MotionEvent.BUTTON_FORWARD to 4,
        )
    }

    private fun emit(kind: String, a: Float, b: Float) {
        if (!a.isFinite() || !b.isFinite()) return
        val wv = installed ?: return
        // Double.toString may print exponent form ("1.0E-4"), which is a valid JS literal too.
        val js = "window.__pulsarCapMouse&&window.__pulsarCapMouse('$kind',${a.toDouble()},${b.toDouble()})"
        try { wv.evaluateJavascript(js, null) } catch (_: Exception) {}
    }
}
