//! DualSense over Android USB host: the pad drivers behind `ds5.rs` (Android only).
//!
//! The video plugin (`Ds5Usb.kt`) finds a USB DualSense, gets the user's USB permission,
//! opens it and claims its HID interface (detaching `hid-playstation`), then hands the
//! usbfs file descriptor over. Everything after that runs here, on that fd, with the same
//! ioctls Android's own `UsbDeviceConnection` uses (`USBDEVFS_BULK` on the interrupt
//! endpoints, `USBDEVFS_CONTROL` for feature reports). A driver per pad:
//! - a reader thread: interrupt IN reports → `ds5::Event::Input` (pad_feed forwards them);
//! - the driver thread: `pulsar_core::input::ds5::pad::Pad` (feature reads at open, output
//!   state coalescing and masking) fed by `ds5::Cmd`, plus the haptics → rumble fallback.
//!
//! The manager task polls the plugin every [`POLL`] while [`ds5::wanted`] (a FEAT_DS5
//! session is up): new pads start a driver; when nothing wants them any more the drivers
//! stop, leave the pad clean (triggers + motors off), release the interface and ask the
//! kernel to re-bind its driver (`USBDEVFS_CONNECT`, what `libusb_attach_kernel_driver`
//! does) so the pad is a normal Android gamepad again, then the plugin closes the fd.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pulsar_core::input::ds5::pad::{HidIo, Pad};
use pulsar_core::input::ds5::report::{SetState, USB_INPUT_ID, USB_INPUT_LEN};
use pulsar_core::input::ds5::rumble::HapticsRumble;
use pulsar_core::input::ds5::wire::Ds5Transport;
use pulsar_core::input::ds5::Ds5Features;
use tauri::{AppHandle, Runtime};
use tauri_plugin_pulsar_video::{Ds5UsbPad, PulsarVideoExt as _, PulsarVideoW5Ext as _};

use crate::ds5::{self, Cmd, Event};

/// Plugin poll period while passthrough is wanted (new pads, the capture gate).
const POLL: Duration = Duration::from_millis(500);
/// Interrupt IN read timeout: bounds how long a stop waits for the reader.
const READ_TIMEOUT_MS: u32 = 100;
/// Output report / control transfer timeouts.
const WRITE_TIMEOUT_MS: u32 = 50;
const CONTROL_TIMEOUT_MS: u32 = 500;
/// Least gap between two haptics-rumble updates (as the BT fallback, `ds5::pad`).
const FALLBACK_GAP: Duration = Duration::from_millis(10);

/// Raw usbfs ioctls (`linux/usbdevice_fs.h`). The request numbers use the generic `_IOC`
/// layout (arm, arm64, x86, x86_64 — every Android ABI).
mod usbfs {
    use std::ffi::{c_int, c_uint, c_void};
    use std::io;

    extern "C" {
        // bionic: `int ioctl(int fd, int request, ...)`.
        fn ioctl(fd: c_int, request: c_int, ...) -> c_int;
    }

    const fn ioc(dir: u32, nr: u32, size: usize) -> c_int {
        ((dir << 30) | ((size as u32) << 16) | ((b'U' as u32) << 8) | nr) as c_int
    }
    const W: u32 = 1;
    const R: u32 = 2;

    #[repr(C)]
    struct CtrlTransfer {
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
        timeout: u32,
        data: *mut c_void,
    }

    #[repr(C)]
    struct BulkTransfer {
        ep: c_uint,
        len: c_uint,
        timeout: c_uint,
        data: *mut c_void,
    }

    #[repr(C)]
    struct IoctlReq {
        ifno: c_int,
        ioctl_code: c_int,
        data: *mut c_void,
    }

    const USBDEVFS_CONTROL: c_int = ioc(R | W, 0, std::mem::size_of::<CtrlTransfer>());
    const USBDEVFS_BULK: c_int = ioc(R | W, 2, std::mem::size_of::<BulkTransfer>());
    const USBDEVFS_RELEASEINTERFACE: c_int = ioc(R, 16, std::mem::size_of::<c_uint>());
    const USBDEVFS_IOCTL: c_int = ioc(R | W, 18, std::mem::size_of::<IoctlReq>());
    const USBDEVFS_CONNECT: c_int = ioc(0, 23, 0);

    fn check(r: c_int) -> io::Result<usize> {
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(r as usize)
        }
    }

    /// One transfer on a bulk or interrupt endpoint (`ep` with bit 7 = IN).
    pub fn bulk(fd: c_int, ep: u8, buf: &mut [u8], timeout_ms: u32) -> io::Result<usize> {
        let mut t = BulkTransfer {
            ep: ep as c_uint,
            len: buf.len() as c_uint,
            timeout: timeout_ms,
            data: buf.as_mut_ptr().cast(),
        };
        // SAFETY: `t` points at `buf`, which outlives the synchronous ioctl.
        check(unsafe { ioctl(fd, USBDEVFS_BULK, &mut t as *mut BulkTransfer) })
    }

    /// A control transfer on endpoint 0 (`buf` is read into or written from by direction).
    pub fn control(
        fd: c_int,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        buf: &mut [u8],
        timeout_ms: u32,
    ) -> io::Result<usize> {
        let mut t = CtrlTransfer {
            request_type,
            request,
            value,
            index,
            length: buf.len() as u16,
            timeout: timeout_ms,
            data: buf.as_mut_ptr().cast(),
        };
        // SAFETY: as in `bulk`.
        check(unsafe { ioctl(fd, USBDEVFS_CONTROL, &mut t as *mut CtrlTransfer) })
    }

    /// Release our claim on `iface` and let the kernel bind its own driver again.
    pub fn hand_back(fd: c_int, iface: u8) -> io::Result<()> {
        let mut n: c_uint = iface as c_uint;
        // SAFETY: `n` lives across the ioctl.
        check(unsafe { ioctl(fd, USBDEVFS_RELEASEINTERFACE, &mut n as *mut c_uint) })?;
        let mut req = IoctlReq {
            ifno: iface as c_int,
            ioctl_code: USBDEVFS_CONNECT,
            data: std::ptr::null_mut(),
        };
        // SAFETY: `req` lives across the ioctl; CONNECT takes no data.
        check(unsafe { ioctl(fd, USBDEVFS_IOCTL, &mut req as *mut IoctlReq) }).map(|_| ())
    }
}

/// HID access to a claimed pad: output reports on the interrupt OUT endpoint (or
/// SET_REPORT when it has none), feature reports with GET_REPORT.
struct UsbHid {
    fd: i32,
    iface: u8,
    ep_out: u8,
}

impl HidIo for UsbHid {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut b = buf.to_vec();
        if self.ep_out != 0 {
            usbfs::bulk(self.fd, self.ep_out, &mut b, WRITE_TIMEOUT_MS)
        } else {
            // SET_REPORT (class, interface, host → device), report type 2 = output.
            let value = (2u16 << 8) | b[0] as u16;
            usbfs::control(self.fd, 0x21, 0x09, value, self.iface as u16, &mut b, WRITE_TIMEOUT_MS)
        }
    }

    fn get_feature(&mut self, id: u8, len: usize) -> std::io::Result<Vec<u8>> {
        let mut b = vec![0u8; len];
        // GET_REPORT (class, interface, device → host), report type 3 = feature.
        let value = (3u16 << 8) | id as u16;
        let n = usbfs::control(self.fd, 0xA1, 0x01, value, self.iface as u16, &mut b, CONTROL_TIMEOUT_MS)?;
        b.truncate(n);
        if b.first() != Some(&id) {
            // Some stacks strip the report id; the host side expects it in byte 0.
            b.insert(0, id);
            b.truncate(len);
        }
        Ok(b)
    }
}

struct Driver {
    id: u32,
    stop: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

/// Start the manager task (once per process). Called at each connect.
pub fn ensure_started<R: Runtime>(app: &AppHandle<R>) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move { manage(app).await });
}

async fn manage<R: Runtime>(app: AppHandle<R>) {
    let mut drivers: Vec<Driver> = Vec::new();
    let mut was_wanted = false;
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let want = ds5::wanted();
        // Drivers that ended on their own (unplugged, I/O error) or must stop now.
        let mut done = Vec::new();
        let mut i = 0;
        while i < drivers.len() {
            if !want || drivers[i].join.is_finished() {
                let d = drivers.swap_remove(i);
                d.stop.store(true, Ordering::Relaxed);
                done.push(d);
            } else {
                i += 1;
            }
        }
        for d in done {
            let app = app.clone();
            let _ = tauri::async_runtime::spawn_blocking(move || {
                let _ = d.join.join();
                let _ = app.pulsar_video().ds5_usb_close(d.id);
            })
            .await;
        }
        if want {
            let fresh = !was_wanted;
            let app2 = app.clone();
            let poll = tauri::async_runtime::spawn_blocking(move || app2.pulsar_video().ds5_usb_poll(fresh)).await;
            match poll {
                Ok(Ok(p)) => {
                    ds5::set_capture(p.capture);
                    for pad in p.pads {
                        match start_driver(pad.clone()) {
                            Some(d) => drivers.push(d),
                            None => {
                                let app = app.clone();
                                let _ = tauri::async_runtime::spawn_blocking(move || {
                                    let _ = app.pulsar_video().ds5_usb_close(pad.id);
                                })
                                .await;
                            }
                        }
                    }
                }
                Ok(Err(e)) => log::warn!("pulsar: ds5 usb poll failed: {e:?}"),
                Err(_) => {}
            }
        } else {
            ds5::set_capture(false);
        }
        was_wanted = want;
    }
}

fn start_driver(p: Ds5UsbPad) -> Option<Driver> {
    if p.fd < 0 || p.ep_in & 0x80 == 0 {
        log::warn!("pulsar: ds5 usb pad {} unusable (fd {}, ep_in {:#x})", p.id, p.fd, p.ep_in);
        return None;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let id = p.id;
    let join = std::thread::Builder::new()
        .name(format!("ds5-usb-{}", p.pad))
        .spawn(move || run(p, s2))
        .ok()?;
    Some(Driver { id, stop, join })
}

fn run(p: Ds5UsbPad, stop: Arc<AtomicBool>) {
    let slot = p.pad;
    let mut io = UsbHid { fd: p.fd, iface: p.iface, ep_out: p.ep_out };
    let opened = Pad::open(&mut io, Ds5Transport::Usb, ds5::USB_FEATURES);
    let (mut pad, features) = match opened {
        Ok(v) => v,
        Err(e) => {
            log::warn!("pulsar: ds5 usb pad {slot}: init failed ({e}); handing it back");
            if let Err(e) = usbfs::hand_back(p.fd, p.iface) {
                log::warn!("pulsar: ds5 usb pad {slot}: hand back failed ({e})");
            }
            return;
        }
    };
    // Network jitter is irrelevant here (no audio reports on USB), but keep the desktop's
    // default sizing so a future USB audio path behaves the same.
    let (pre, max) = pulsar_core::input::ds5::degrade::Ds5Prefs::default().jitter_slots();
    pad.set_jitter_buffer(pre, max);
    let (tx, rx) = std::sync::mpsc::channel();
    ds5::register_driver(slot, tx);
    ds5::emit(Event::Opened { pad: slot, name: p.name.clone(), features });
    log::info!("pulsar: ds5 usb pad {slot} = {} ({:04x}), raw passthrough", p.name, p.product);

    // Reader: interrupt IN → the forwarder. Stops on `stop` or a dead device.
    let failed = Arc::new(AtomicBool::new(false));
    let reader = {
        let (stop, failed) = (stop.clone(), failed.clone());
        let (fd, ep) = (p.fd, p.ep_in);
        std::thread::Builder::new().name(format!("ds5-usb-rd-{slot}")).spawn(move || {
            let mut buf = [0u8; 64];
            while !stop.load(Ordering::Relaxed) {
                match usbfs::bulk(fd, ep, &mut buf, READ_TIMEOUT_MS) {
                    Ok(n) if n >= USB_INPUT_LEN && buf[0] == USB_INPUT_ID => {
                        let mut r = [0u8; USB_INPUT_LEN];
                        r.copy_from_slice(&buf[..USB_INPUT_LEN]);
                        ds5::emit(Event::Input { pad: slot, report: r });
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(e) => {
                        log::info!("pulsar: ds5 usb pad {slot}: read ended ({e})");
                        failed.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        })
    };

    let mut fb = HapticsRumble::default();
    // The game's latest state and its own rumble (the fallback re-merges into it).
    let mut game: Option<SetState> = None;
    let mut host_rumble = (0u8, 0u8);
    let mut fb_sent: Option<(u8, u8)> = None;
    let mut fb_at: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) && !failed.load(Ordering::Relaxed) {
        let wait = pad.poll(Instant::now(), &mut io).clamp(Duration::from_millis(1), FALLBACK_GAP);
        let mut cmd = match rx.recv_timeout(wait) {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        while let Some(c) = cmd.take() {
            match c {
                Cmd::Output(s) => {
                    if let Some(r) = s.rumble() {
                        host_rumble = r;
                        fb_sent = None;
                    }
                    game = Some(s);
                    pad.set_output(s);
                }
                Cmd::Haptics(h) => fb.feed(&h, Instant::now()),
                Cmd::Reset => {
                    game = None;
                    host_rumble = (0, 0);
                    fb.reset();
                    fb_sent = None;
                    pad.set_output(ds5::reset_state());
                }
            }
            cmd = rx.try_recv().ok();
        }
        // HD haptics → classic rumble (the game's own rumble wins where it is stronger).
        let now = Instant::now();
        let lvl = fb.level(now);
        if lvl != (0, 0) || fb_sent.is_some() {
            let want = (lvl.0.max(host_rumble.0), lvl.1.max(host_rumble.1));
            let due = fb_at.is_none_or(|t| now.saturating_duration_since(t) >= FALLBACK_GAP);
            if fb_sent != Some(want) && due {
                pad.set_output(ds5::with_rumble(game, want.0, want.1));
                fb_sent = (lvl != (0, 0)).then_some(want);
                fb_at = Some(now);
            }
        }
    }
    // Leave the pad clean: triggers released, motors off.
    pad.set_features(Ds5Features { mic: false, ..Ds5Features::all() });
    pad.set_output(ds5::reset_state());
    let _ = pad.poll(Instant::now() + Duration::from_millis(10), &mut io);
    ds5::unregister_driver(slot);
    ds5::emit(Event::Closed { pad: slot });
    stop.store(true, Ordering::Relaxed);
    if let Ok(r) = reader {
        let _ = r.join();
    }
    if !failed.load(Ordering::Relaxed) {
        if let Err(e) = usbfs::hand_back(p.fd, p.iface) {
            log::warn!("pulsar: ds5 usb pad {slot}: hand back failed ({e}); replug it to use it on the phone");
        }
    }
    log::info!("pulsar: ds5 usb pad {slot} released");
}
