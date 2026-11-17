package dev.pulsar.video

import android.app.Activity
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbEndpoint
import android.hardware.usb.UsbInterface
import android.hardware.usb.UsbManager
import android.os.Build
import android.util.Log
import android.view.InputDevice
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject

/**
 * DualSense raw passthrough over USB host (the Rust side is mobile/src/ds5.rs + ds5_usb.rs).
 *
 * Android gives apps no raw HID access to a pad the kernel drives (no hidraw node, no
 * HID-host API), so a Bluetooth DualSense can only be a plain gamepad. Over USB the app
 * may open the device itself: with the user's per-device USB permission it claims the HID
 * interface (forcing `hid-playstation` off it) and hands the usbfs file descriptor to Rust,
 * which does all report I/O with usbfs ioctls (input reports incl. gyro + touchpad, output
 * reports for triggers / lightbar / LEDs / rumble, feature reports).
 *
 * Rust polls [poll] while a session to a host advertising FEAT_DS5 is up, and when it no
 * longer needs a pad it releases the interface, re-attaches the kernel driver
 * (USBDEVFS_CONNECT) and calls [close]; the pad is then a normal Android gamepad again.
 * While claimed the pad is not an InputDevice, so it keeps a slot in [PadInput]'s
 * numbering ([PadInput.reserveIdx]) and is listed by `gamepadBattery` from [names].
 *
 * Permission: one system dialog per pad (asked once per passthrough period, `fresh`);
 * without a grant the pad stays a normal gamepad. All calls run on the UI thread.
 */
class Ds5Usb(private val activity: Activity, private val pads: PadInput) {
    private val TAG = "PulsarDs5"

    companion object {
        const val VID_SONY = 0x054C
        val PIDS = setOf(0x0CE6 /* DualSense */, 0x0DF2 /* DualSense Edge */)
        const val ACTION_PERMISSION = "dev.pulsar.video.DS5_USB_PERMISSION"
    }

    private class Claimed(val pad: Int, val key: String, val conn: UsbDeviceConnection, val name: String)

    private val claimed = HashMap<Int, Claimed>()
    private val asked = HashSet<String>()
    // Pads Rust could not drive (init failed): left alone until the next `fresh` period,
    // so a pad that refuses its feature reads is not claimed and dropped every poll.
    private val skip = HashSet<String>()
    private var nextId = 1
    private val usb: UsbManager? by lazy { activity.getSystemService(Context.USB_SERVICE) as? UsbManager }

    /** Names of the claimed pads (they are not InputDevices while claimed). */
    @Synchronized
    fun names(): List<String> = claimed.values.map { it.name }

    /**
     * Open every USB DualSense not open yet. Pads without permission get the system dialog
     * once (per [fresh] period) and are picked up by a later poll once granted. Returns the
     * pads opened by this call: `{id, pad, fd, iface, epIn, epOut, product, name}`.
     */
    @Synchronized
    fun poll(fresh: Boolean): JSArray {
        val out = JSArray()
        val m = usb ?: return out
        if (fresh) { asked.clear(); skip.clear() }
        val held = claimed.values.map { it.key }.toSet()
        for (dev in m.deviceList.values) {
            if (dev.vendorId != VID_SONY || dev.productId !in PIDS) continue
            if (dev.deviceName in held || dev.deviceName in skip) continue
            if (!m.hasPermission(dev)) {
                if (asked.add(dev.deviceName)) requestPermission(m, dev)
                continue
            }
            try {
                open(m, dev)?.let { out.put(it) }
            } catch (e: Exception) {
                Log.w(TAG, "open ${dev.deviceName} failed: ${e.message}")
            }
        }
        return out
    }

    /** Close pad [id] (Rust already released its interface) and free its slot. [failed] =
     *  Rust could not drive it: skip it until the next passthrough period. */
    @Synchronized
    fun close(id: Int, failed: Boolean = false) {
        val c = claimed.remove(id) ?: return
        if (failed) skip.add(c.key)
        try { c.conn.close() } catch (e: Exception) { Log.w(TAG, "close failed: ${e.message}") }
        pads.releaseIdx(c.pad)
        Log.i(TAG, "pad ${c.pad} (${c.name}) handed back")
    }

    private fun requestPermission(m: UsbManager, dev: UsbDevice) {
        try {
            // Explicit (package-scoped) intent: a mutable PendingIntent needs one on Android 14+,
            // and UsbManager must be able to fill in its extras (hence FLAG_MUTABLE on 12+).
            val intent = Intent(ACTION_PERMISSION).setPackage(activity.packageName)
            val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) PendingIntent.FLAG_MUTABLE else 0
            m.requestPermission(dev, PendingIntent.getBroadcast(activity, 0, intent, flags))
            Log.i(TAG, "asked for USB permission: ${dev.deviceName}")
        } catch (e: Exception) {
            Log.w(TAG, "USB permission request failed: ${e.message}")
        }
    }

    /** The name Android shows for this pad (same key as Settings' emulation choice). */
    private fun inputName(dev: UsbDevice): String {
        try {
            for (id in InputDevice.getDeviceIds()) {
                val d = InputDevice.getDevice(id) ?: continue
                if (!d.isVirtual && d.vendorId == dev.vendorId && d.productId == dev.productId) {
                    d.name?.let { return it }
                }
            }
        } catch (_: Exception) {}
        return dev.productName ?: "DualSense Wireless Controller"
    }

    private fun open(m: UsbManager, dev: UsbDevice): JSObject? {
        var intf: UsbInterface? = null
        for (i in 0 until dev.interfaceCount) {
            val f = dev.getInterface(i)
            if (f.interfaceClass == UsbConstants.USB_CLASS_HID) { intf = f; break }
        }
        if (intf == null) { Log.w(TAG, "${dev.deviceName}: no HID interface"); return null }
        var epIn: UsbEndpoint? = null
        var epOut: UsbEndpoint? = null
        for (e in 0 until intf.endpointCount) {
            val ep = intf.getEndpoint(e)
            if (ep.type != UsbConstants.USB_ENDPOINT_XFER_INT) continue
            if (ep.direction == UsbConstants.USB_DIR_IN) { if (epIn == null) epIn = ep } else if (epOut == null) epOut = ep
        }
        if (epIn == null) { Log.w(TAG, "${dev.deviceName}: no interrupt IN endpoint"); return null }
        // Read the name before the claim: the claim removes the pad's InputDevice.
        val name = inputName(dev)
        val conn = m.openDevice(dev) ?: run { Log.w(TAG, "${dev.deviceName}: openDevice failed"); return null }
        if (!conn.claimInterface(intf, true)) {
            conn.close()
            Log.w(TAG, "${dev.deviceName}: claimInterface failed")
            return null
        }
        val id = nextId++
        val pad = pads.reserveIdx()
        claimed[id] = Claimed(pad, dev.deviceName, conn, name)
        val o = JSObject()
        o.put("id", id)
        o.put("pad", pad)
        o.put("fd", conn.fileDescriptor)
        o.put("iface", intf.id)
        o.put("epIn", epIn.address)
        o.put("epOut", epOut?.address ?: 0)
        o.put("product", dev.productId)
        o.put("name", name)
        Log.i(TAG, "pad $pad = $name (${dev.deviceName}, iface ${intf.id}) claimed for raw passthrough")
        return o
    }
}
