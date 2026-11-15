package dev.pulsar.video

import android.app.Activity
import android.content.Context
import android.hardware.input.InputManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.Looper
import android.util.Log
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.Window
import java.lang.reflect.InvocationTargetException
import java.lang.reflect.Proxy
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress

/**
 * Physical game controllers -> host (3.1). Reads Android gamepad/joystick input natively
 * (the WebView's Gamepad API only sees a pad after a button press and is unreliable) and
 * sends each pad's state to the Rust side over a loopback UDP socket, which forwards it to
 * the host as `InputEvent::GamepadSlot` — no WebView or per-event plugin IPC in the path.
 *
 * Interception: a proxy around the activity's Window.Callback sees every key / generic
 * motion event before the view tree. While [shouldCapture] is true (a stream is on screen)
 * events FROM A GAMEPAD are consumed — otherwise an unhandled BUTTON_B falls back to BACK
 * and leaves the session. Keyboards, TV-remote D-pads and touch pass through untouched.
 * Outside a stream nothing is consumed (gamepad-nav.js drives the app UI then).
 *
 * Mapping follows Moonlight (ControllerHandler): left stick X/Y, right stick Z/RZ (else
 * RX/RY), triggers LTRIGGER/RTRIGGER (else BRAKE/GAS, else RX/RY on a Z/RZ pad), D-pad
 * from HAT_X/HAT_Y or DPAD keys. Y axes are flipped to the wire's up-positive convention.
 *
 * Wire (little endian), one datagram each:
 *   1 state:      [1, pad, buttons u32, lx i16, ly i16, rx i16, ry i16, lt u8, rt u8] (16 B)
 *   2 connected:  [2, pad, vendorId u16, productId u16, name UTF-8...]
 *   3 removed:    [3, pad]
 * `buttons` uses pulsar-core's XInput-style bits (input::types::buttons).
 */
class PadInput(private val activity: Activity, private val shouldCapture: () -> Boolean) {
    private val TAG = "PulsarPad"

    private object Btn {
        const val A = 1 shl 0; const val B = 1 shl 1; const val X = 1 shl 2; const val Y = 1 shl 3
        const val LB = 1 shl 4; const val RB = 1 shl 5; const val BACK = 1 shl 6; const val START = 1 shl 7
        const val GUIDE = 1 shl 8; const val L3 = 1 shl 9; const val R3 = 1 shl 10
        const val UP = 1 shl 11; const val DOWN = 1 shl 12; const val LEFT = 1 shl 13; const val RIGHT = 1 shl 14
        const val DPAD = UP or DOWN or LEFT or RIGHT
    }

    private class Pad(val idx: Int, val deviceId: Int) {
        var keyButtons = 0   // from key events
        var hatButtons = 0   // from HAT_X/HAT_Y
        var lx = 0; var ly = 0; var rx = 0; var ry = 0
        var lt = 0; var rt = 0          // analog triggers
        var keyLt = false; var keyRt = false // digital L2/R2 (pads without analog triggers)
        var announced = false
        // Axis layout, resolved once per device.
        var rxAxis = MotionEvent.AXIS_Z; var ryAxis = MotionEvent.AXIS_RZ
        var ltAxis = -1; var rtAxis = -1; var triggersCentered = false
    }

    private val pads = HashMap<Int, Pad>() // deviceId -> pad (UI thread only)
    @Volatile private var port = 0
    private var sock: DatagramSocket? = null
    private val sendThread = HandlerThread("pulsar-pad").apply { start() }
    private val sendHandler = Handler(sendThread.looper)
    private val loopback: InetAddress = InetAddress.getByAddress(byteArrayOf(127, 0, 0, 1))
    private var installed = false

    /** Point the feed at the Rust loopback socket and install the interceptor (idempotent). */
    fun start(feedPort: Int) {
        port = feedPort
        activity.runOnUiThread { install() }
    }

    private fun install() {
        if (installed) return
        val win = activity.window ?: return
        val orig = win.callback ?: return
        val proxy = Proxy.newProxyInstance(
            Window.Callback::class.java.classLoader, arrayOf(Window.Callback::class.java)
        ) { _, method, args ->
            when (method.name) {
                "dispatchKeyEvent" -> if (args != null && handleKey(args[0] as KeyEvent)) return@newProxyInstance true
                "dispatchGenericMotionEvent" -> if (args != null && handleMotion(args[0] as MotionEvent)) return@newProxyInstance true
            }
            try {
                if (args == null) method.invoke(orig) else method.invoke(orig, *args)
            } catch (e: InvocationTargetException) {
                throw e.targetException ?: e
            }
        } as Window.Callback
        win.callback = proxy
        installed = true
        try {
            val im = activity.getSystemService(Context.INPUT_SERVICE) as InputManager
            im.registerInputDeviceListener(object : InputManager.InputDeviceListener {
                override fun onInputDeviceAdded(id: Int) {}
                override fun onInputDeviceChanged(id: Int) {}
                override fun onInputDeviceRemoved(id: Int) { removed(id) }
            }, Handler(Looper.getMainLooper()))
        } catch (e: Exception) { Log.w(TAG, "input device listener failed: ${e.message}") }
        Log.i(TAG, "gamepad interceptor installed (feed port $port)")
    }

    private fun isPadSource(source: Int): Boolean =
        (source and InputDevice.SOURCE_GAMEPAD) == InputDevice.SOURCE_GAMEPAD ||
            (source and InputDevice.SOURCE_JOYSTICK) == InputDevice.SOURCE_JOYSTICK

    private fun padFor(dev: InputDevice): Pad {
        pads[dev.id]?.let { return it }
        val used = pads.values.map { it.idx }.toSet()
        val idx = (0..15).first { it !in used }
        val p = Pad(idx, dev.id)
        val has = { axis: Int -> dev.getMotionRange(axis) != null }
        if (!(has(MotionEvent.AXIS_Z) && has(MotionEvent.AXIS_RZ))) {
            p.rxAxis = MotionEvent.AXIS_RX; p.ryAxis = MotionEvent.AXIS_RY
        }
        when {
            has(MotionEvent.AXIS_LTRIGGER) && has(MotionEvent.AXIS_RTRIGGER) -> { p.ltAxis = MotionEvent.AXIS_LTRIGGER; p.rtAxis = MotionEvent.AXIS_RTRIGGER }
            has(MotionEvent.AXIS_BRAKE) && has(MotionEvent.AXIS_GAS) -> { p.ltAxis = MotionEvent.AXIS_BRAKE; p.rtAxis = MotionEvent.AXIS_GAS }
            p.rxAxis == MotionEvent.AXIS_Z && has(MotionEvent.AXIS_RX) && has(MotionEvent.AXIS_RY) -> {
                // Older DualShock 4 kernels: triggers on RX/RY, resting at -1.
                p.ltAxis = MotionEvent.AXIS_RX; p.rtAxis = MotionEvent.AXIS_RY; p.triggersCentered = true
            }
        }
        pads[dev.id] = p
        return p
    }

    private fun announce(p: Pad, dev: InputDevice) {
        if (p.announced) return
        p.announced = true
        val name = (dev.name ?: "Controller").toByteArray(Charsets.UTF_8).let { if (it.size > 200) it.copyOf(200) else it }
        val b = ByteArray(6 + name.size)
        b[0] = 2; b[1] = p.idx.toByte()
        putU16(b, 2, dev.vendorId); putU16(b, 4, dev.productId)
        System.arraycopy(name, 0, b, 6, name.size)
        send(b)
        Log.i(TAG, "pad ${p.idx} = ${dev.name} (${"%04x".format(dev.vendorId)}:${"%04x".format(dev.productId)})")
    }

    private fun keyBit(code: Int): Int = when (code) {
        KeyEvent.KEYCODE_BUTTON_A -> Btn.A
        KeyEvent.KEYCODE_BUTTON_B -> Btn.B
        KeyEvent.KEYCODE_BUTTON_X -> Btn.X
        KeyEvent.KEYCODE_BUTTON_Y -> Btn.Y
        KeyEvent.KEYCODE_BUTTON_L1 -> Btn.LB
        KeyEvent.KEYCODE_BUTTON_R1 -> Btn.RB
        KeyEvent.KEYCODE_BUTTON_THUMBL -> Btn.L3
        KeyEvent.KEYCODE_BUTTON_THUMBR -> Btn.R3
        KeyEvent.KEYCODE_BUTTON_START, KeyEvent.KEYCODE_MENU -> Btn.START
        KeyEvent.KEYCODE_BUTTON_SELECT, KeyEvent.KEYCODE_BACK -> Btn.BACK
        KeyEvent.KEYCODE_BUTTON_MODE -> Btn.GUIDE
        KeyEvent.KEYCODE_DPAD_UP -> Btn.UP
        KeyEvent.KEYCODE_DPAD_DOWN -> Btn.DOWN
        KeyEvent.KEYCODE_DPAD_LEFT -> Btn.LEFT
        KeyEvent.KEYCODE_DPAD_RIGHT -> Btn.RIGHT
        else -> 0
    }

    /** UI thread. True = consumed. */
    private fun handleKey(ev: KeyEvent): Boolean {
        if (port == 0 || !shouldCapture()) return false
        if (!isPadSource(ev.source)) return false
        val dev = ev.device ?: return false
        if (dev.isVirtual) return false
        val code = ev.keyCode
        val isL2 = code == KeyEvent.KEYCODE_BUTTON_L2
        val isR2 = code == KeyEvent.KEYCODE_BUTTON_R2
        val bit = keyBit(code)
        if (bit == 0 && !isL2 && !isR2) {
            // Some other key from the pad (volume, a vendor button): let Android have it,
            // except unknown gamepad buttons, which must not fall back to navigation.
            return KeyEvent.isGamepadButton(code)
        }
        val down = when (ev.action) {
            KeyEvent.ACTION_DOWN -> true
            KeyEvent.ACTION_UP -> false
            else -> return true
        }
        val p = padFor(dev)
        announce(p, dev)
        when {
            isL2 -> p.keyLt = down
            isR2 -> p.keyRt = down
            down -> p.keyButtons = p.keyButtons or bit
            else -> p.keyButtons = p.keyButtons and bit.inv()
        }
        sendState(p)
        return true
    }

    private fun axis(ev: MotionEvent, dev: InputDevice, a: Int): Float {
        val v = ev.getAxisValue(a)
        val flat = dev.getMotionRange(a, ev.source)?.flat ?: 0f
        return if (kotlin.math.abs(v) <= flat) 0f else v
    }

    private fun stick(v: Float): Int = (v.coerceIn(-1f, 1f) * 32767f).toInt()
    private fun trig(v: Float): Int = (v.coerceIn(0f, 1f) * 255f).toInt()

    /** UI thread. True = consumed. */
    private fun handleMotion(ev: MotionEvent): Boolean {
        if (port == 0 || !shouldCapture()) return false
        if ((ev.source and InputDevice.SOURCE_JOYSTICK) != InputDevice.SOURCE_JOYSTICK) return false
        if (ev.action != MotionEvent.ACTION_MOVE) return false
        val dev = ev.device ?: return false
        if (dev.isVirtual) return false
        val p = padFor(dev)
        announce(p, dev)
        p.lx = stick(axis(ev, dev, MotionEvent.AXIS_X))
        p.ly = stick(-axis(ev, dev, MotionEvent.AXIS_Y))
        p.rx = stick(axis(ev, dev, p.rxAxis))
        p.ry = stick(-axis(ev, dev, p.ryAxis))
        if (p.ltAxis >= 0) {
            var l = ev.getAxisValue(p.ltAxis); var r = ev.getAxisValue(p.rtAxis)
            if (p.triggersCentered) { l = (l + 1f) / 2f; r = (r + 1f) / 2f }
            p.lt = trig(l); p.rt = trig(r)
        }
        val hx = ev.getAxisValue(MotionEvent.AXIS_HAT_X)
        val hy = ev.getAxisValue(MotionEvent.AXIS_HAT_Y)
        var hat = 0
        if (hx < -0.5f) hat = hat or Btn.LEFT else if (hx > 0.5f) hat = hat or Btn.RIGHT
        if (hy < -0.5f) hat = hat or Btn.UP else if (hy > 0.5f) hat = hat or Btn.DOWN
        p.hatButtons = hat
        sendState(p)
        return true
    }

    private fun sendState(p: Pad) {
        val buttons = p.keyButtons or p.hatButtons
        val lt = if (p.keyLt) 255 else p.lt
        val rt = if (p.keyRt) 255 else p.rt
        val b = ByteArray(16)
        b[0] = 1; b[1] = p.idx.toByte()
        b[2] = (buttons and 0xFF).toByte(); b[3] = ((buttons shr 8) and 0xFF).toByte()
        b[4] = ((buttons shr 16) and 0xFF).toByte(); b[5] = ((buttons shr 24) and 0xFF).toByte()
        putU16(b, 6, p.lx); putU16(b, 8, p.ly); putU16(b, 10, p.rx); putU16(b, 12, p.ry)
        b[14] = lt.toByte(); b[15] = rt.toByte()
        send(b)
    }

    private fun removed(deviceId: Int) {
        val p = pads.remove(deviceId) ?: return
        if (p.announced) send(byteArrayOf(3, p.idx.toByte()))
        Log.i(TAG, "pad ${p.idx} removed")
    }

    private fun putU16(b: ByteArray, at: Int, v: Int) {
        b[at] = (v and 0xFF).toByte(); b[at + 1] = ((v shr 8) and 0xFF).toByte()
    }

    private fun send(b: ByteArray) {
        val dst = port
        if (dst == 0) return
        // Never on the UI thread (StrictMode network-on-main); one ordered sender thread.
        sendHandler.post {
            try {
                val s = sock ?: DatagramSocket().also { sock = it }
                s.send(DatagramPacket(b, b.size, loopback, dst))
            } catch (e: Exception) { Log.w(TAG, "pad send failed: ${e.message}") }
        }
    }

    // ---------------- rumble ----------------

    private fun deviceFor(idx: Int): InputDevice? =
        pads.values.firstOrNull { it.idx == idx }?.let { InputDevice.getDevice(it.deviceId) }

    /** Host rumble for pad [idx]: [large]/[small] motor 0..255, both 0 = stop. UI thread.
     *  The host sends rumble on change, so each command holds until the next (capped at
     *  60 s by the one-shot length). */
    fun rumble(idx: Int, large: Int, small: Int) {
        val dev = deviceFor(idx) ?: return
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                val vm = dev.vibratorManager
                val ids = vm.vibratorIds
                if (ids.isNotEmpty()) {
                    if (large <= 0 && small <= 0) { vm.cancel(); return }
                    val comb = android.os.CombinedVibration.startParallel()
                    if (ids.size >= 2) {
                        if (large > 0) comb.addVibrator(ids[0], oneShot(large))
                        if (small > 0) comb.addVibrator(ids[1], oneShot(small))
                    } else {
                        comb.addVibrator(ids[0], oneShot(maxOf(large, small)))
                    }
                    vm.vibrate(comb.combine())
                    return
                }
            }
            @Suppress("DEPRECATION")
            val v = dev.vibrator
            if (!v.hasVibrator()) return
            val amp = maxOf(large, small)
            if (amp <= 0) { v.cancel(); return }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) v.vibrate(oneShot(amp))
            else @Suppress("DEPRECATION") v.vibrate(60_000L)
        } catch (e: Exception) { Log.w(TAG, "rumble failed: ${e.message}") }
    }

    private fun oneShot(amp: Int): android.os.VibrationEffect =
        android.os.VibrationEffect.createOneShot(60_000L, amp.coerceIn(1, 255))

    /** Stop every pad's motors (stream ended). UI thread. */
    fun stopAllRumble() {
        for (p in pads.values) rumble(p.idx, 0, 0)
    }
}
