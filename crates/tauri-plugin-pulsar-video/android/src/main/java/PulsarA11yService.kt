package dev.pulsar.video

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.graphics.Path
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo

/**
 * M16 polish: lets a remote peer CONTROL this phone. A normal app can't inject raw
 * touch events, but an AccessibilityService can `dispatchGesture`, so the mobile
 * host turns the peer's pointer (tap/swipe) into injected gestures. The user enables
 * it once in Settings → Accessibility.
 */
class PulsarA11yService : AccessibilityService() {
    companion object {
        @Volatile
        var instance: PulsarA11yService? = null
    }

    override fun onServiceConnected() { instance = this }
    override fun onDestroy() { instance = null; super.onDestroy() }
    override fun onAccessibilityEvent(event: AccessibilityEvent?) {}
    override fun onInterrupt() {}

    /**
     * Tap (start≈end) or swipe between two screen-pixel points. `durMs>0` forces the
     * stroke duration (from the press→release elapsed time) so a real long-press or a
     * slow drag is reproduced; otherwise it's derived from the distance.
     */
    fun gesture(x1: Float, y1: Float, x2: Float, y2: Float, durMs: Long = -1L) {
        val dist = Math.hypot((x2 - x1).toDouble(), (y2 - y1).toDouble())
        val ex = if (dist < 2) x1 + 1f else x2
        val ey = if (dist < 2) y1 + 1f else y2
        val path = Path().apply { moveTo(x1, y1); lineTo(ex, ey) }
        val dur = if (durMs > 0) durMs.coerceIn(30L, 2000L) else if (dist < 16) 60L else 250L
        Log.i("PulsarA11y", "gesture ($x1,$y1)->($ex,$ey) dur=$dur")
        try {
            val gd = GestureDescription.Builder()
                .addStroke(GestureDescription.StrokeDescription(path, 0, dur))
                .build()
            dispatchGesture(gd, null, null)
        } catch (_: Exception) {}
    }

    // ── Live drag (API 26+) ───────────────────────────────────────────────────
    // A press-and-drag streamed as CONTINUED strokes so the phone follows the pointer
    // in real time instead of replaying one straight line on release. Each continued
    // stroke must only be dispatched after the previous one's callback fires, so moves
    // arriving while a dispatch is in flight coalesce into `pendingX/Y` and flush from
    // `onCompleted` (intermediate points are dropped — the finger just takes the
    // latest known position, which is what a touch screen does anyway).

    private var stroke: GestureDescription.StrokeDescription? = null
    private var strokeX = 0f
    private var strokeY = 0f
    private var pendingX = 0f
    private var pendingY = 0f
    private var hasPending = false
    private var endPending = false
    private var busy = false

    /** True while a streamed drag is in progress (between dragStart and its final flush). */
    val dragging: Boolean get() = stroke != null

    private val dragCallback = object : GestureResultCallback() {
        override fun onCompleted(g: GestureDescription?) { busy = false; flushDrag() }
        override fun onCancelled(g: GestureDescription?) {
            // System killed the gesture (e.g. real touch intervened) — abandon the drag.
            busy = false; stroke = null; hasPending = false; endPending = false
        }
    }

    /** Put the virtual finger DOWN at (x,y) and keep it held. API 26+ only. */
    fun dragStart(x: Float, y: Float): Boolean {
        if (Build.VERSION.SDK_INT < 26) return false
        if (stroke != null) return true // already down
        val path = Path().apply { moveTo(x, y); lineTo(x + 0.1f, y) }
        return try {
            val s = GestureDescription.StrokeDescription(path, 0, 30L, true)
            val ok = dispatchGesture(GestureDescription.Builder().addStroke(s).build(), dragCallback, null)
            if (ok) { stroke = s; strokeX = x; strokeY = y; busy = true; hasPending = false; endPending = false }
            ok
        } catch (_: Exception) { false }
    }

    /** Move the held finger toward (x,y); coalesces while a segment is in flight. */
    fun dragMove(x: Float, y: Float) {
        if (stroke == null) return
        pendingX = x; pendingY = y; hasPending = true
        if (!busy) flushDrag()
    }

    /** Lift the finger at (x,y), ending the drag. */
    fun dragEnd(x: Float, y: Float) {
        if (stroke == null) return
        pendingX = x; pendingY = y; hasPending = true; endPending = true
        if (!busy) flushDrag()
    }

    // ── Keyboard (remote client → this phone) ─────────────────────────────────
    // An AccessibilityService cannot inject arbitrary KeyEvents (that needs the
    // system INJECT_EVENTS permission), so remote keys are mapped to what a11y CAN
    // do: global navigation actions and text editing on the focused input node.
    // evdev codes (see input-event-codes.h): 1=ESC 14=BACKSPACE 15=TAB 28=ENTER
    // 56=LALT 100=RALT 96=KPENTER 105=LEFT 106=RIGHT 111=DELETE 125/126=META.

    private var altDown = false

    /** Handle a remote key press/release (evdev `code`). */
    fun key(code: Int, down: Boolean) {
        Log.i("PulsarA11y", "key code=$code down=$down alt=$altDown")
        when (code) {
            56, 100 -> { altDown = down; return }
            15 -> if (down && altDown) { performGlobalAction(GLOBAL_ACTION_RECENTS); return }
        }
        if (!down) return
        when (code) {
            125, 126 -> performGlobalAction(GLOBAL_ACTION_HOME) // Win/Meta → Home
            1 -> performGlobalAction(GLOBAL_ACTION_BACK)        // Esc → Back
            14 -> editText { text, s, e ->                      // Backspace
                if (s == e && s == 0) null
                else if (s == e) Triple(text.substring(0, s - 1) + text.substring(e), s - 1, s - 1)
                else Triple(text.substring(0, s) + text.substring(e), s, s)
            }
            111 -> editText { text, s, e ->                     // Delete
                if (s == e && e >= text.length) null
                else if (s == e) Triple(text.substring(0, s) + text.substring(e + 1), s, s)
                else Triple(text.substring(0, s) + text.substring(e), s, s)
            }
            28, 96 -> {                                         // Enter
                val node = focusedEditable()
                if (node != null && Build.VERSION.SDK_INT >= 30) {
                    node.performAction(android.R.id.accessibilityActionImeEnter)
                } else {
                    typeText("\n")
                }
            }
            105 -> moveCaret(-1)                                // ← caret left
            106 -> moveCaret(1)                                 // → caret right
        }
    }

    /** Insert `str` at the caret of the focused editable node (replacing any selection). */
    fun typeText(str: String) {
        if (str.isEmpty()) return
        editText { text, s, e -> Triple(text.substring(0, s) + str + text.substring(e), s + str.length, s + str.length) }
    }

    private fun focusedEditable(): AccessibilityNodeInfo? =
        try { findFocus(AccessibilityNodeInfo.FOCUS_INPUT)?.takeIf { it.isEditable } } catch (_: Exception) { null }

    /**
     * Read the focused editable's text+selection, let `f(text, selStart, selEnd)` produce
     * (newText, newSelStart, newSelEnd) — or null for no-op — then apply via
     * ACTION_SET_TEXT + ACTION_SET_SELECTION.
     */
    private fun editText(f: (String, Int, Int) -> Triple<String, Int, Int>?) {
        val node = focusedEditable()
        if (node == null) {
            Log.i("PulsarA11y", "editText: no focused editable node")
            return
        }
        val text = node.text?.toString() ?: ""
        var s = node.textSelectionStart.let { if (it < 0) text.length else it.coerceAtMost(text.length) }
        var e = node.textSelectionEnd.let { if (it < 0) text.length else it.coerceAtMost(text.length) }
        if (s > e) { val t = s; s = e; e = t }
        val (newText, ns, ne) = f(text, s, e) ?: return
        try {
            val args = Bundle().apply {
                putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, newText)
            }
            node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)
            val sel = Bundle().apply {
                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, ns.coerceIn(0, newText.length))
                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, ne.coerceIn(0, newText.length))
            }
            node.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, sel)
        } catch (_: Exception) {}
    }

    private fun moveCaret(delta: Int) {
        val node = focusedEditable() ?: return
        val text = node.text?.toString() ?: ""
        val cur = node.textSelectionEnd.let { if (it < 0) text.length else it }
        val pos = (cur + delta).coerceIn(0, text.length)
        try {
            val sel = Bundle().apply {
                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, pos)
                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, pos)
            }
            node.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, sel)
        } catch (_: Exception) {}
    }

    private fun flushDrag() {
        val s = stroke ?: return
        if (!hasPending) {
            if (endPending) { stroke = null; endPending = false }
            return
        }
        // Zero-length continued segments get rejected — nudge the end point.
        val ex = if (pendingX == strokeX && pendingY == strokeY) pendingX + 0.1f else pendingX
        val path = Path().apply { moveTo(strokeX, strokeY); lineTo(ex, pendingY) }
        val isEnd = endPending
        try {
            val next = s.continueStroke(path, 0, 30L, !isEnd)
            val ok = dispatchGesture(GestureDescription.Builder().addStroke(next).build(), dragCallback, null)
            if (!ok) { stroke = null; hasPending = false; endPending = false; return }
            busy = true; hasPending = false
            strokeX = pendingX; strokeY = pendingY
            if (isEnd) { stroke = null; endPending = false } else { stroke = next }
        } catch (_: Exception) {
            stroke = null; hasPending = false; endPending = false
        }
    }
}
