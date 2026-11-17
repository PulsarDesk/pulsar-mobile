//! DualSense passthrough, client side — the Android counterpart of pulsar-desktop
//! `ds5/client.rs`.
//!
//! When the active pane's host advertises [`FEAT_DS5`], a DualSense on USB is driven raw:
//! - the pad's USB input report (every button, both sticks, gyro + accelerometer, the
//!   touchpad, battery) goes to the host as `InputEvent::Ds5Input` (≤ 250 Hz, latest wins,
//!   1 s keepalive), preceded by `DataMsg::Ds5Caps` + the pad's feature reports
//!   (`Ds5Feature` 0x05 calibration, 0x20 firmware) so the host's virtual DualSense
//!   answers games like the real pad;
//! - host `DataMsg::Ds5Output` (adaptive triggers, lightbar, player LEDs, mic LED, classic
//!   rumble) is written to the pad as a USB 0x02 output report;
//! - host HD haptics (`TAG_DS5` media slots) become classic rumble on the pad (the
//!   envelope follower desktop uses when it sheds haptics, `pulsar_core::input::ds5::rumble`):
//!   on USB the desktop plays haptics through the pad's own USB audio device, which an
//!   Android app cannot address as a 4-channel output.
//!
//! The pad itself is opened by the video plugin (`Ds5Usb.kt`, USB host API: one
//! permission dialog per pad) and its HID interface is claimed away from the kernel's
//! `hid-playstation` driver; all report I/O then runs here on the usbfs fd
//! (`ds5_usb.rs`). Pads are claimed only while a session to a FEAT_DS5 host is up, and
//! handed back to the kernel driver (`USBDEVFS_CONNECT`) afterwards.
//!
//! Limits on stock Android (no root):
//! - **Bluetooth:** no raw HID. The kernel driver owns the pad and Android exposes no
//!   hidraw node or HID-host API to apps, so a BT DualSense stays a plain `GamepadSlot`
//!   (pad_feed.rs) — no gyro/touchpad, triggers, lightbar, haptics or speaker.
//! - **USB:** no pad speaker, no pad mic (they are USB audio interfaces), HD haptics only
//!   as classic rumble. `Ds5Caps` says so (speaker / mic off).
//!
//! Input forwarding and targeting live in `pad_feed.rs` (it already follows the active
//! pane); this module holds the shared state, the pure report helpers and the routing of
//! host messages to the pad drivers.

// The pad drivers (ds5_usb.rs) are Android-only; elsewhere their half of this API is unused.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use pulsar_core::input::ds5::haptics::Frame;
use pulsar_core::input::ds5::report::{flag, SetState, STATE_LEN, USB_INPUT_LEN};
use pulsar_core::input::ds5::wire::{Ds5Media, Ds5Report};
use pulsar_core::input::ds5::Ds5Features;
use pulsar_core::input::{button, GamepadState};

pub use pulsar_core::input::ds5::wire::FEAT_DS5;

/// What the pad lets through to the host (`Ds5Caps`): no speaker / mic over Android USB
/// host; haptics stay on because they arrive here and are played as rumble.
pub const USB_FEATURES: Ds5Features = Ds5Features {
    triggers: true,
    rumble: true,
    leds: true,
    haptics: true,
    speaker: false,
    mic: false,
};

// ── Settings ─────────────────────────────────────────────────────────────────

/// Master switch (Settings → Controllers → DualSense passthrough). On by default, like
/// the desktop's `Ds5Prefs::enabled`. Off = every DualSense is a plain gamepad.
static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// JS: `invoke('set_ds5_enabled', { enabled })` — applies to the next poll (≤ 0.5 s): off
/// hands claimed pads back to Android.
#[tauri::command]
pub fn set_ds5_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

// ── Which hosts take raw DualSense input ─────────────────────────────────────

fn ds5_hosts() -> &'static Mutex<HashSet<u8>> {
    static H: OnceLock<Mutex<HashSet<u8>>> = OnceLock::new();
    H.get_or_init(Default::default)
}

/// The session on `pane` started; remember whether its host advertised [`FEAT_DS5`].
pub fn set_host_features(pane: u8, features: &[String]) {
    let mut h = ds5_hosts().lock().unwrap();
    if features.iter().any(|f| f == FEAT_DS5) {
        h.insert(pane);
    } else {
        h.remove(&pane);
    }
}

/// The session on `pane` ended.
pub fn clear_host(pane: u8) {
    ds5_hosts().lock().unwrap().remove(&pane);
}

/// True when the host on `pane` takes `Ds5Input` (and the user left passthrough on).
pub fn host_takes_raw(pane: u8) -> bool {
    enabled() && ds5_hosts().lock().unwrap().contains(&pane)
}

/// USB pads are worth claiming: passthrough on and at least one FEAT_DS5 session up.
pub fn wanted() -> bool {
    enabled() && !ds5_hosts().lock().unwrap().is_empty()
}

// ── Capture gate (stream on screen + window focused, from the plugin poll) ───

static CAPTURE: AtomicBool = AtomicBool::new(false);

pub fn set_capture(on: bool) {
    CAPTURE.store(on, Ordering::Relaxed);
}

/// While false the host gets every claimed pad as released (desktop `focus_gate`).
pub fn capture() -> bool {
    CAPTURE.load(Ordering::Relaxed)
}

// ── Pad drivers (ds5_usb.rs threads) and host → pad routing ──────────────────

/// A command for one pad driver.
#[derive(Debug)]
pub enum Cmd {
    /// The host's output state (`Ds5Output`).
    Output(SetState),
    /// One HD haptics frame (played as rumble).
    Haptics(Frame),
    /// Input now goes to another host: triggers off, motors off.
    Reset,
}

fn drivers() -> &'static Mutex<HashMap<u8, std::sync::mpsc::Sender<Cmd>>> {
    static D: OnceLock<Mutex<HashMap<u8, std::sync::mpsc::Sender<Cmd>>>> = OnceLock::new();
    D.get_or_init(Default::default)
}

pub fn register_driver(pad: u8, tx: std::sync::mpsc::Sender<Cmd>) {
    drivers().lock().unwrap().insert(pad, tx);
}

pub fn unregister_driver(pad: u8) {
    drivers().lock().unwrap().remove(&pad);
}

fn to_driver(pad: u8, cmd: Cmd) {
    if let Some(tx) = drivers().lock().unwrap().get(&pad) {
        let _ = tx.send(cmd);
    }
}

/// The pane whose host the pads are forwarded to now (set by pad_feed's retarget). Host
/// output from any other pane is dropped: one pad, one driver.
static OUT_PANE: Mutex<Option<u8>> = Mutex::new(None);

/// pad_feed retargeted to `pane`: reset the pads (another host's triggers / rumble must
/// not stay engaged) and accept output only from the new pane.
pub fn set_output_pane(pane: Option<u8>) {
    let mut cur = OUT_PANE.lock().unwrap();
    if *cur != pane {
        *cur = pane;
        drop(cur);
        let ids: Vec<u8> = drivers().lock().unwrap().keys().copied().collect();
        for p in ids {
            to_driver(p, Cmd::Reset);
        }
    }
}

fn from_target(pane: u8) -> bool {
    *OUT_PANE.lock().unwrap() == Some(pane)
}

/// Host `DataMsg::Ds5Output` for gamepad slot `slot`, from the session on `pane`.
pub fn on_output(pane: u8, slot: u8, report: &[u8]) {
    if !from_target(pane) {
        return;
    }
    if let Some(s) = SetState::from_bytes(report) {
        to_driver(slot, Cmd::Output(s));
    }
}

/// A `TAG_DS5` media payload (whole session payload, tag included) from `pane`.
pub fn on_media(pane: u8, payload: &[u8]) {
    if !from_target(pane) {
        return;
    }
    if let Some(Ds5Media::Slot { slot, haptics: Some(h), .. }) = Ds5Media::decode(payload) {
        to_driver(slot, Cmd::Haptics(h));
    }
}

// ── Driver → pad_feed events ─────────────────────────────────────────────────

/// What a pad driver tells the forwarder (pad_feed.rs).
#[derive(Debug)]
pub enum Event {
    /// The pad is open: its name and the feature reports read at open (`Ds5Feature`).
    Opened { pad: u8, name: String, features: Vec<(u8, Vec<u8>)> },
    /// One USB input report (`[0x01, state..]`).
    Input { pad: u8, report: [u8; USB_INPUT_LEN] },
    /// The pad went away (unplugged, handed back, or passthrough turned off).
    Closed { pad: u8 },
}

type EventChan = (tokio::sync::mpsc::Sender<Event>, Mutex<Option<tokio::sync::mpsc::Receiver<Event>>>);

/// Created on first use by either side, so no event is lost to start-up order.
fn events() -> &'static EventChan {
    static E: OnceLock<EventChan> = OnceLock::new();
    E.get_or_init(|| {
        let (tx, rx) = tokio::sync::mpsc::channel(512);
        (tx, Mutex::new(Some(rx)))
    })
}

/// The forwarder's receiver (pad_feed calls this once). `None` on a second call.
pub fn take_events() -> Option<tokio::sync::mpsc::Receiver<Event>> {
    events().1.lock().unwrap().take()
}

/// Send an event from a driver thread; dropped when the forwarder is backlogged (the next
/// input report replaces a dropped one anyway).
pub fn emit(ev: Event) {
    use tokio::sync::mpsc::error::TrySendError;
    let tx = &events().0;
    let keep = !matches!(ev, Event::Input { .. });
    let mut ev = ev;
    // Open / close must not be lost: wait up to ~0.5 s for room (never forever: a driver
    // thread must always get to hand its pad back).
    for _ in 0..100 {
        match tx.try_send(ev) {
            Err(TrySendError::Full(back)) if keep => {
                ev = back;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            _ => return,
        }
    }
}

// ── Pure report helpers ──────────────────────────────────────────────────────

/// `r` with every control let go — sticks centred, triggers, buttons (PS / touchpad click
/// / mute included) and both touch points up. Motion + timestamps stay as read, so the host
/// sees a pad lying still rather than a jump. Same as desktop `ds5::client::released`.
pub fn released(r: Ds5Report) -> Ds5Report {
    let mut s: [u8; STATE_LEN] = r.0;
    s[0..4].fill(0x80); // LX LY RX RY
    s[4] = 0; // L2
    s[5] = 0; // R2
    s[7] = 0x08; // hat neutral, no face buttons
    s[8] = 0; // L1 R1 L2 R2 create options L3 R3
    s[9] = 0; // PS, touchpad click, mute (+ Edge paddles / Fn)
    s[32] |= 0x80; // touch point 0 lifted
    s[36] |= 0x80; // touch point 1 lifted
    Ds5Report(s)
}

/// A DualSense USB input report (0x01) as [`GamepadState`], with SDL's mapping (cross = A,
/// circle = B, square = X, triangle = Y, create = BACK, options = START, PS = GUIDE) — for
/// hosts without FEAT_DS5 while the pad is claimed. Same as desktop
/// `controllers::ds5_usb_to_state`.
pub fn usb_to_state(r: &[u8; USB_INPUT_LEN]) -> GamepadState {
    let mut st = GamepadState::default();
    let axis = |v: u8| ((v as i32 - 128) * 256).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    st.left_x = axis(r[1]);
    st.left_y = axis(r[2]).saturating_neg();
    st.right_x = axis(r[3]);
    st.right_y = axis(r[4]).saturating_neg();
    st.left_trigger = r[5];
    st.right_trigger = r[6];
    let (b0, b1, b2) = (r[8], r[9], r[10]);
    let hat = b0 & 0x0F;
    st.set(button::DPAD_UP, matches!(hat, 7 | 0 | 1));
    st.set(button::DPAD_RIGHT, matches!(hat, 1..=3));
    st.set(button::DPAD_DOWN, matches!(hat, 3..=5));
    st.set(button::DPAD_LEFT, matches!(hat, 5..=7));
    st.set(button::X, b0 & 0x10 != 0);
    st.set(button::A, b0 & 0x20 != 0);
    st.set(button::B, b0 & 0x40 != 0);
    st.set(button::Y, b0 & 0x80 != 0);
    st.set(button::LB, b1 & 0x01 != 0);
    st.set(button::RB, b1 & 0x02 != 0);
    st.set(button::BACK, b1 & 0x10 != 0);
    st.set(button::START, b1 & 0x20 != 0);
    st.set(button::L3, b1 & 0x40 != 0);
    st.set(button::R3, b1 & 0x80 != 0);
    st.set(button::GUIDE, b2 & 0x01 != 0);
    st
}

/// A state that releases both adaptive triggers and stops the motors (session end /
/// retarget). Desktop `triggers_off` plus rumble 0.
pub fn reset_state() -> SetState {
    let mut s = SetState::default();
    s.0[0] = flag::B0_RIGHT_TRIGGER | flag::B0_LEFT_TRIGGER;
    s.0[10] = 0x05;
    s.0[21] = 0x05;
    s.set_rumble(0, 0);
    s
}

/// The game's last state with the rumble motors set to `(heavy, light)`, for the haptics →
/// rumble fallback. One-shot flags (reset lights) are cleared so re-sending the state does
/// not replay them.
pub fn with_rumble(game: Option<SetState>, heavy: u8, light: u8) -> SetState {
    let mut s = game.unwrap_or_default();
    s.0[1] &= !flag::B1_RESET_LIGHTS;
    s.set_rumble(heavy, light);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn released_report_lets_every_control_go() {
        let mut s = [0u8; STATE_LEN];
        s[0..4].copy_from_slice(&[0x00, 0xff, 0x10, 0xe0]);
        s[4] = 0xff;
        s[5] = 0x80;
        s[6] = 0x42;
        s[7] = 0x20 | 0x02;
        s[8] = 0x03;
        s[9] = 0x01;
        s[15] = 0x55;
        s[32] = 0x05;
        s[36] = 0x86;
        let r = released(Ds5Report(s)).0;
        assert_eq!(&r[0..4], &[0x80; 4]);
        assert_eq!((r[4], r[5]), (0, 0));
        assert_eq!(r[7], 0x08);
        assert_eq!((r[8], r[9]), (0, 0));
        assert_eq!(r[32] & 0x80, 0x80);
        assert_eq!(r[36], 0x86);
        assert_eq!((r[6], r[15]), (0x42, 0x55), "counter + motion untouched");
    }

    #[test]
    fn usb_report_maps_like_sdl() {
        let mut r = [0u8; USB_INPUT_LEN];
        r[0] = 0x01;
        r[1..5].copy_from_slice(&[128, 0, 255, 128]);
        r[5] = 10;
        r[6] = 200;
        r[8] = 0x20 | 0x02; // cross + d-pad right
        r[9] = 0x20; // options
        r[10] = 0x01; // PS
        let st = usb_to_state(&r);
        assert_eq!(st.left_x, 0);
        assert!(st.left_y > 30_000, "stick pushed up = up-positive");
        assert!(st.right_x > 30_000);
        assert_eq!((st.left_trigger, st.right_trigger), (10, 200));
        assert!(st.is_pressed(button::A) && st.is_pressed(button::DPAD_RIGHT));
        assert!(st.is_pressed(button::START) && st.is_pressed(button::GUIDE));
        assert!(!st.is_pressed(button::DPAD_UP) && !st.is_pressed(button::B));
        r[8] = 0x08;
        assert_eq!(usb_to_state(&r).buttons & (button::DPAD_UP | button::DPAD_DOWN), 0);
    }

    #[test]
    fn host_registry_and_routing_gate() {
        set_host_features(7, &["nack".into(), FEAT_DS5.into()]);
        assert!(host_takes_raw(7));
        assert!(wanted());
        set_host_features(7, &["nack".into()]);
        assert!(!host_takes_raw(7));
        set_host_features(7, &[FEAT_DS5.into()]);
        clear_host(7);
        assert!(!host_takes_raw(7));
    }

    #[test]
    fn reset_and_rumble_states() {
        let r = reset_state();
        assert!(r.has_triggers());
        assert_eq!(r.rumble(), Some((0, 0)));
        let mut game = SetState::default();
        game.0[1] = flag::B1_LED_COLOR | flag::B1_RESET_LIGHTS;
        let s = with_rumble(Some(game), 200, 40);
        assert_eq!(s.rumble(), Some((200, 40)));
        assert_eq!(s.0[1], flag::B1_LED_COLOR, "lightbar kept, reset dropped");
    }
}
